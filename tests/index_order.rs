//! v0.6 Teil 3: Index-Order / Top-K.
//!
//! `ORDER BY indexed_field LIMIT n` wird unter der **Presence-Garantie** (ein
//! positives, indexierbares Literal auf dem Sortierfeld in jeder DNF-Klausel +
//! READY-Index) als `Limit{ Filter{ IndexOrderScan } }` ausgeführt — bounded,
//! lazy, verifizierend. Ohne Garantie bleibt der unveränderte `Sort`-Fallback.
//! Alle Ergebnisse müssen exakt dem Full-Scan-Oracle (`sort_rows`) entsprechen.

use std::cmp::Ordering;

use my_lsm_db::codec::{self, Value};
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::keycodec;
use my_lsm_db::ordering;
use my_lsm_db::query::executor;
use my_lsm_db::query::expression::eval;
use my_lsm_db::query::logical::{LogicalPlan, SortDir};
use my_lsm_db::query::planner;
use my_lsm_db::query::{Predicate, SortDir as QSortDir, ge, gt, lt};
use my_lsm_db::schema::Schema;
use my_lsm_db::{Database, DirectMutator, Mutator, ScanStream};

fn int(v: i64) -> Value {
    Value::Int(v)
}

/// Oracle-Sortierung — identisch zur Executor-Regel (`sort_rows`).
fn sort_rows(rows: &mut [(String, Entity)], field: &str, dir: SortDir) {
    let asc = dir == SortDir::Asc;
    rows.sort_by(|a, b| {
        let va = a.1.field(field);
        let vb = b.1.field(field);
        let ord = match (va, vb) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => {
                if asc {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (Some(_), None) => {
                if asc {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (Some(x), Some(y)) => {
                let o = ordering::value_cmp(x, y);
                if asc { o } else { o.reverse() }
            }
        };
        if ord != Ordering::Equal {
            ord
        } else {
            a.0.cmp(&b.0)
        }
    });
}

/// Dummer Full-Scan-Oracle: scan_collection + eval + sort + limit.
fn oracle(
    store: &mut EntityStore,
    pred: Option<&Predicate>,
    sort: Option<(QSortDir, &str)>,
    limit: Option<usize>,
) -> Vec<(String, Entity)> {
    let mut rows = store.scan_collection("users").unwrap();
    if let Some(p) = pred {
        rows.retain(|(_, e)| eval(e, p).unwrap());
    }
    if let Some((dir, f)) = sort {
        let d = if dir == QSortDir::Asc {
            SortDir::Asc
        } else {
            SortDir::Desc
        };
        sort_rows(&mut rows, f, d);
    }
    if let Some(n) = limit {
        rows.truncate(n);
    }
    rows
}

fn execute(
    store: &mut EntityStore,
    pred: Option<&Predicate>,
    sort: Option<(QSortDir, &str)>,
    limit: Option<usize>,
) -> Vec<(String, Entity)> {
    let mut b = store.query("users").unwrap();
    if let Some(p) = pred {
        b = b.filter(p.clone());
    }
    if let Some((dir, f)) = sort {
        b = b.sort(f, dir);
    }
    if let Some(n) = limit {
        b = b.limit(n);
    }
    store.execute_query(b).unwrap()
}

fn ent(age: i64, score: i64, active: bool) -> Entity {
    let mut e = Entity::new();
    e.insert("age", int(age));
    e.insert("score", int(score));
    e.insert("active", Value::Bool(active));
    e
}

/// Store mit `age`-Index und deterministischen Daten.
fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        for i in 0..100 {
            // age immer vorhanden (Presence-Garantie-Fälle), score/active teils fehlend.
            let mut e = ent(i as i64 % 40, (i as i64 * 7) % 1000, i % 2 == 0);
            if i % 11 == 0 {
                e.fields.retain(|(n, _)| n == "age");
            }
            col.put(&format!("u{i:03}"), &e).unwrap();
        }
    }
    drop(store);
    dir
}

#[test]
fn top_k_asc_matches_oracle() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let pred = ge("age", int(0));
    for n in [1usize, 5, 37] {
        let exp = oracle(
            &mut store,
            Some(&pred),
            Some((QSortDir::Asc, "age")),
            Some(n),
        );
        let got = execute(
            &mut store,
            Some(&pred),
            Some((QSortDir::Asc, "age")),
            Some(n),
        );
        assert_eq!(got, exp, "asc top-{n} mismatch");
    }
}

#[test]
fn top_k_desc_matches_oracle() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let pred = ge("age", int(0));
    for n in [1usize, 5, 37] {
        let exp = oracle(
            &mut store,
            Some(&pred),
            Some((QSortDir::Desc, "age")),
            Some(n),
        );
        let got = execute(
            &mut store,
            Some(&pred),
            Some((QSortDir::Desc, "age")),
            Some(n),
        );
        assert_eq!(got, exp, "desc top-{n} mismatch");
    }
}

#[test]
fn missing_field_presence_falls_back_to_sort() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    // Kein Literal auf `age` → keine Presence-Garantie → Sort-Fallback.
    let b = store
        .query("users")
        .unwrap()
        .filter(eq_active())
        .sort("age", QSortDir::Asc)
        .limit(5);
    let s = store.explain_query(&b).unwrap();
    assert!(s.contains("Sort"), "expected Sort fallback, got:\n{s}");
    assert!(
        !s.contains("IndexOrderScan"),
        "unexpected IndexOrderScan:\n{s}"
    );

    let pred = Some(eq_active());
    let exp = oracle(
        &mut store,
        pred.as_ref(),
        Some((QSortDir::Asc, "age")),
        Some(5),
    );
    let got = execute(
        &mut store,
        pred.as_ref(),
        Some((QSortDir::Asc, "age")),
        Some(5),
    );
    assert_eq!(got, exp, "missing-field fallback must match oracle");
}

#[test]
fn sort_field_without_index_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    // Nur `score` ist indexiert, `age` nicht. Trotz Presence-Garantie auf `age`
    // (Literal vorhanden) verlangt die Enablement-Regel einen READY-Index auf
    // dem Sortierfeld → Sort-Fallback.
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("score").unwrap();
        col.put("u1", &ent(30, 100, true)).unwrap();
        col.put("u2", &ent(10, 50, true)).unwrap();
        col.put("u3", &ent(20, 200, true)).unwrap();
    }
    let pred = ge("age", int(0));
    let exp = oracle(
        &mut store,
        Some(&pred),
        Some((QSortDir::Asc, "age")),
        Some(2),
    );
    let got = execute(
        &mut store,
        Some(&pred),
        Some((QSortDir::Asc, "age")),
        Some(2),
    );
    assert_eq!(got, exp);
    assert_eq!(got[0].0, "u2", "sort must order by age asc");
}

fn eq_active() -> Predicate {
    use my_lsm_db::query::eq;
    eq("active", Value::Bool(true))
}

#[test]
fn multi_clause_or_index_order() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    // OR über zwei Klauseln, BEIDE mit positivem `age`-Literal.
    let pred = gt("age", int(30)).or(gt("age", int(0)).and(gt("score", int(500))));
    for n in [5usize, 20] {
        let exp = oracle(
            &mut store,
            Some(&pred),
            Some((QSortDir::Asc, "age")),
            Some(n),
        );
        let got = execute(
            &mut store,
            Some(&pred),
            Some((QSortDir::Asc, "age")),
            Some(n),
        );
        assert_eq!(got, exp, "or top-{n} mismatch");
        let exp_d = oracle(
            &mut store,
            Some(&pred),
            Some((QSortDir::Desc, "age")),
            Some(n),
        );
        let got_d = execute(
            &mut store,
            Some(&pred),
            Some((QSortDir::Desc, "age")),
            Some(n),
        );
        assert_eq!(got_d, exp_d, "or desc top-{n} mismatch");
    }
}

#[test]
fn and_residual_filter_index_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        col.create_index("score").unwrap();
        for i in 0..80 {
            col.put(
                &format!("u{i:03}"),
                &ent(i as i64 % 30, i as i64, i % 2 == 0),
            )
            .unwrap();
        }
    }
    // `score` bleibt als Residual im Filter; `age` deckt den Index-Order ab.
    let pred = gt("age", int(5)).and(gt("score", int(100)));
    for n in [3usize, 10] {
        let exp = oracle(
            &mut store,
            Some(&pred),
            Some((QSortDir::Asc, "age")),
            Some(n),
        );
        let got = execute(
            &mut store,
            Some(&pred),
            Some((QSortDir::Asc, "age")),
            Some(n),
        );
        assert_eq!(got, exp, "and-residual top-{n} mismatch");
    }
}

#[test]
fn equal_values_tie_break() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        // Gleiche `age`-Werte, aber IDs variabler Länge → ID-Tie-Break (string-asc).
        for id in ["a1", "a2", "a10", "a11", "a3"] {
            col.put(id, &ent(50, 1, true)).unwrap();
        }
        col.put("b1", &ent(40, 1, true)).unwrap();
        col.put("b2", &ent(60, 1, true)).unwrap();
    }
    let pred = ge("age", int(0));
    let exp = oracle(&mut store, Some(&pred), Some((QSortDir::Asc, "age")), None);
    let got = execute(&mut store, Some(&pred), Some((QSortDir::Asc, "age")), None);
    assert_eq!(got, exp);
    let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
    // 40 → 50 (a1,a10,a11,a2,a3 string-asc) → 60
    assert_eq!(ids, vec!["b1", "a1", "a10", "a11", "a2", "a3", "b2"]);

    let exp_d = oracle(&mut store, Some(&pred), Some((QSortDir::Desc, "age")), None);
    let got_d = execute(&mut store, Some(&pred), Some((QSortDir::Desc, "age")), None);
    assert_eq!(got_d, exp_d);
    let ids_d: Vec<&str> = got_d.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids_d, vec!["b2", "a1", "a10", "a11", "a2", "a3", "b1"]);
}

#[test]
fn bounds_respected_both_dirs() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    // Exklusive Grenzen: age > 5 UND age < 30.
    let pred = gt("age", int(5)).and(lt("age", int(30)));
    for (d, n) in [(QSortDir::Asc, 7usize), (QSortDir::Desc, 7usize)] {
        let exp = oracle(&mut store, Some(&pred), Some((d, "age")), Some(n));
        let got = execute(&mut store, Some(&pred), Some((d, "age")), Some(n));
        assert_eq!(got, exp, "bounds dir mismatch");
        for (id, e) in &got {
            let a = e.field("age").unwrap();
            assert!(
                ordering::value_cmp(a, &int(5)) == Ordering::Greater,
                "{id} above bound"
            );
            assert!(
                ordering::value_cmp(a, &int(30)) == Ordering::Less,
                "{id} below bound"
            );
        }
    }
}

#[test]
fn top_k_is_prefix_of_full_sort() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let pred = ge("age", int(0));
    // Ohne Limit → Sort-Fallback (vollständige, deterministische Ordnung).
    let full = oracle(&mut store, Some(&pred), Some((QSortDir::Asc, "age")), None);
    // Mit Limit → IndexOrderScan. Ergebnis muss Präfix der vollen Ordnung sein.
    for n in [1usize, 5, 50] {
        let got = execute(
            &mut store,
            Some(&pred),
            Some((QSortDir::Asc, "age")),
            Some(n),
        );
        assert_eq!(
            got,
            full[..n].to_vec(),
            "top-{n} must be prefix of full order"
        );
    }
}

/// Zählender Mutator: zählt `get`-Aufrufe (Verifikations-Punkt-Lookups).
struct CountingMutator<'a> {
    inner: DirectMutator<'a>,
    gets: usize,
}

impl<'a> Mutator for CountingMutator<'a> {
    fn get(&mut self, key: &[u8]) -> my_lsm_db::error::Result<Option<Vec<u8>>> {
        self.gets += 1;
        self.inner.get(key)
    }
    fn scan<'s>(
        &'s mut self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> my_lsm_db::error::Result<ScanStream<'s>> {
        self.inner.scan(start, end)
    }
    fn put(&mut self, key: &[u8], value: &[u8]) -> my_lsm_db::error::Result<()> {
        self.inner.put(key, value)
    }
    fn delete(&mut self, key: &[u8]) -> my_lsm_db::error::Result<()> {
        self.inner.delete(key)
    }
}

#[test]
fn limit_pulls_only_necessary_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
    let mut schema = Schema::new();
    let cid = schema.collection_id("users");
    let fid = schema.field_id(cid, "age");
    let idx = schema.create_index(cid, fid);
    schema.set_index_ready(idx);
    for i in 0..1000 {
        let id = format!("u{i:04}");
        let ekey = keycodec::encode_entity_key(cid, id.as_bytes(), fid);
        db.put(&ekey, &codec::encode(&Value::Int(i as i64)))
            .unwrap();
        let ik = keycodec::encode_index_key(
            cid,
            fid,
            &ordering::encode_ordered(&Value::Int(i as i64)),
            id.as_bytes(),
        );
        db.put(&ik, &[]).unwrap();
    }

    // `ORDER BY age ASC LIMIT 3` (age>=0 → Presence-Garantie → IndexOrderScan).
    let logical = LogicalPlan::Limit {
        input: Box::new(LogicalPlan::Sort {
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(LogicalPlan::Collection {
                    name: "users".into(),
                }),
                pred: ge("age", int(0)),
            }),
            field: "age".into(),
            dir: SortDir::Asc,
        }),
        n: 3,
    };
    let plan = planner::plan(&schema, logical);
    let mut cm = CountingMutator {
        inner: DirectMutator { db: &mut db },
        gets: 0,
    };
    let rows = executor::run_m(&mut cm, &schema, &plan).unwrap();
    assert_eq!(rows.len(), 3, "top-3 must return 3 rows");
    let ids: Vec<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, vec!["u0000", "u0001", "u0002"]);
    // Der Limit-Stop darf nur eine kleine, konstante Zahl an
    // Verifikations-Punkt-Lookups verursachen — NICHT ~1000.
    assert!(
        cm.gets <= 5,
        "limit must stop early: {} verification lookups performed",
        cm.gets
    );
}
