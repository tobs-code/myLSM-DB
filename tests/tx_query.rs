//! v0.6 Teil 1: Transaktionale Query-Ausführung.
//!
//! Ein Query innerhalb einer Transaktion sieht committete Daten + eigene,
//! noch uncommittete Writes (Read-your-own-writes), über dasselbe
//! Pending-Overlay (Entity + Index), mit derselben Predicate-Semantik wie
//! committed queries (inkl. Missing-Field und `Not`).

use std::cmp::Ordering;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore, Transaction};
use my_lsm_db::ordering;
use my_lsm_db::query::expression::eval;
use my_lsm_db::query::{Predicate, SortDir, eq, ge, gt, le, ne};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.below((hi - lo + 1) as u64) as i64)
    }
}

fn int(v: i64) -> Value {
    Value::Int(v)
}

fn entity(id: usize, rng: &mut Rng) -> Entity {
    let mut e = Entity::new();
    if rng.below(5) != 0 {
        e.insert("age", int(rng.int(-20, 120)));
    }
    if rng.below(5) != 0 {
        e.insert("score", int(rng.int(0, 1000)));
    }
    if rng.below(5) != 0 {
        e.insert("active", Value::Bool(rng.below(2) == 0));
    }
    if rng.below(5) != 0 {
        e.insert("name", Value::String(format!("user-{}", id)));
    }
    e
}

fn random_value(rng: &mut Rng, field: &str) -> Value {
    match field {
        "age" | "score" => int(rng.int(-50, 150)),
        "active" => Value::Bool(rng.below(2) == 0),
        _ => Value::String(format!("val-{}", rng.below(6))),
    }
}

fn random_atom(rng: &mut Rng, fields: &[&str]) -> Predicate {
    let f = fields[rng.below(fields.len() as u64) as usize];
    let v = random_value(rng, f);
    match rng.below(6) {
        0 => eq(f, v),
        1 => ne(f, v),
        2 => le(f, v),
        3 => gt(f, v),
        _ => ge(f, v),
    }
}

fn random_predicate(rng: &mut Rng, depth: u32) -> Predicate {
    let fields = ["age", "score", "active", "name"];
    if depth == 0 || rng.below(4) != 0 {
        return random_atom(rng, &fields);
    }
    match rng.below(3) {
        0 => random_predicate(rng, depth - 1).and(random_predicate(rng, depth - 1)),
        1 => random_predicate(rng, depth - 1).or(random_predicate(rng, depth - 1)),
        _ => random_predicate(rng, depth - 1).negate(),
    }
}

/// Oracle-Sortierung — identisch zur Executor-Regel.
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

/// Dummer Full-Scan-Oracle **über das Tx-Overlay**: scan_collection (sieht
/// committet + pending) + eval + sort + limit.
fn tx_oracle(
    tx: &mut Transaction,
    pred: Option<&Predicate>,
    sort: Option<(&str, SortDir)>,
    limit: Option<usize>,
) -> Vec<(String, Entity)> {
    let mut rows = tx.scan_collection("users").unwrap();
    if let Some(p) = pred {
        rows.retain(|(_, e)| eval(e, p).unwrap());
    }
    if let Some((f, d)) = sort {
        sort_rows(&mut rows, f, d);
    }
    if let Some(n) = limit {
        rows.truncate(n);
    }
    rows
}

fn tx_execute(
    tx: &mut Transaction,
    pred: Option<&Predicate>,
    sort: Option<(&str, SortDir)>,
    limit: Option<usize>,
) -> Vec<(String, Entity)> {
    let mut b = tx.query("users").unwrap();
    if let Some(p) = pred {
        b = b.filter(p.clone());
    }
    if let Some((f, d)) = sort {
        b = b.sort(f, d);
    }
    if let Some(n) = limit {
        b = b.limit(n);
    }
    tx.execute_query(b).unwrap()
}

fn sorted_by_id(mut rows: Vec<(String, Entity)>) -> Vec<(String, Entity)> {
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

#[test]
fn read_your_own_writes_in_tx_query() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    store
        .collection("users")
        .unwrap()
        .put("u1", &{
            let mut e = Entity::new();
            e.insert("age", int(30));
            e
        })
        .unwrap();
    store
        .collection("users")
        .unwrap()
        .put("u2", &{
            let mut e = Entity::new();
            e.insert("age", int(40));
            e
        })
        .unwrap();

    let mut tx = store.transaction().unwrap();
    // Update innerhalb der Tx: u1 age 30 → 60 (committed bleibt 30).
    tx.update("users", "u1", &{
        let mut e = Entity::new();
        e.insert("age", int(60));
        e
    })
    .unwrap();
    // Delete innerhalb der Tx: u2 verschwindet.
    tx.delete("users", "u2").unwrap();

    // Tx-Query sieht die eigenen Writes (Read-your-own-writes).
    let res = tx_execute(&mut tx, Some(&ge("age", int(50))), None, None);
    assert_eq!(
        sorted_by_id(res),
        sorted_by_id(vec![("u1".into(), {
            let mut e = Entity::new();
            e.insert("age", int(60));
            e
        })])
    );

    // Abort: nichts davon bleibt.
    tx.abort().unwrap();
    drop(tx);
    let committed = store.scan_collection("users").unwrap();
    let ids: Vec<String> = committed.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(ids, vec!["u1".to_string(), "u2".to_string()]);
}

#[test]
fn tx_query_matches_overlay_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut rng = Rng(0xC0FFEE);
        let mut col = store.collection("users").unwrap();
        for i in 0..60 {
            col.put(&format!("u{i}"), &entity(i, &mut rng)).unwrap();
        }
    }

    let mut tx = store.transaction().unwrap();
    {
        let mut rng = Rng(0xDEAD);
        for _ in 0..8 {
            let id = format!("u{}", rng.below(60));
            if rng.below(2) == 0 {
                tx.update("users", &id, &entity(999, &mut rng)).unwrap();
            } else {
                tx.delete("users", &id).unwrap();
            }
        }
    }

    // Verschiedene Predicate/Sort/Limit gegen den Full-Scan-Oracle über dem
    // Tx-Overlay vergleichen (inkl. Missing-Field + `Not` via `ne`).
    let cases: Vec<(Option<Predicate>, Option<(&str, SortDir)>, Option<usize>)> = vec![
        (Some(gt("age", int(30))), None, None),
        (Some(ne("age", int(10))), None, None),
        (Some(eq("active", Value::Bool(true))), None, None),
        (
            Some(random_predicate(&mut Rng(7), 2)),
            Some(("age", SortDir::Asc)),
            Some(10),
        ),
        (
            Some(random_predicate(&mut Rng(11), 2)),
            Some(("score", SortDir::Desc)),
            Some(7),
        ),
        (Some(random_predicate(&mut Rng(13), 3)), None, None),
        (None, Some(("age", SortDir::Asc)), Some(5)),
    ];

    for (pred, sort, limit) in cases {
        let expected = tx_oracle(&mut tx, pred.as_ref(), sort, limit);
        let got = tx_execute(&mut tx, pred.as_ref(), sort, limit);
        assert_eq!(
            sorted_by_id(got),
            sorted_by_id(expected),
            "mismatch for pred={pred:?} sort={sort:?} limit={limit:?}"
        );
    }
}

#[test]
fn tx_index_path_matches_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        let mut rng = Rng(0xABAB);
        for i in 0..50 {
            col.put(&format!("u{i}"), &entity(i, &mut rng)).unwrap();
        }
    }

    let mut tx = store.transaction().unwrap();
    {
        let mut rng = Rng(0xBEEF);
        for _ in 0..6 {
            let id = format!("u{}", rng.below(50));
            tx.update("users", &id, &entity(999, &mut rng)).unwrap();
        }
        tx.delete("users", "u3").unwrap();
    }

    // `age` ist indexiert → Planner nutzt IndexScan; das Ergebnis muss trotzdem
    // exakt dem Full-Scan-Oracle über dem Overlay entsprechen (Index ≠ Wahrheit).
    let pred = gt("age", int(0));
    let expected = tx_oracle(&mut tx, Some(&pred), Some(("age", SortDir::Asc)), None);
    let got = tx_execute(&mut tx, Some(&pred), Some(("age", SortDir::Asc)), None);
    assert_eq!(sorted_by_id(got), sorted_by_id(expected));
}

#[test]
fn tx_query_unknown_collection_no_schema_write() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let sp = dir.path().join("SCHEMA");

    let mut tx = store.transaction().unwrap();
    let b = tx.query("ghost").unwrap().filter(ge("age", int(1)));
    assert!(tx.execute_query(b).unwrap().is_empty());
    assert!(
        !sp.exists(),
        "tx query must not create/persist a SCHEMA file"
    );
    tx.abort().unwrap();
}

#[test]
fn abort_does_not_affect_committed_query() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        col.put("u1", &{
            let mut e = Entity::new();
            e.insert("age", int(30));
            e
        })
        .unwrap();
    }

    // Committed baseline.
    let base_b = store.query("users").unwrap().filter(gt("age", int(0)));
    let base = store.execute_query(base_b).unwrap();

    // Tx ändert + abort.
    let mut tx = store.transaction().unwrap();
    tx.update("users", "u1", &{
        let mut e = Entity::new();
        e.insert("age", int(99));
        e
    })
    .unwrap();
    tx.delete("users", "u1").unwrap();
    tx.abort().unwrap();
    drop(tx);

    // Committed Query unverändert.
    let after_b = store.query("users").unwrap().filter(gt("age", int(0)));
    let after = store.execute_query(after_b).unwrap();
    assert_eq!(base, after);
    assert_eq!(after.len(), 1);
}
