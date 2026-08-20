//! v0.5 Query-Schicht: Full-Scan-Oracle + Random-Query-Test.
//!
//! Der Oracle-Ansatz: Eine "dumme" Query wird als Full-Scan + In-Memory-Eval
//! implementiert (dieselbe `eval` wie der Executor). Der Planner nutzt dagegen
//! Indizes/Fetch/Residual-Filter. Beide MÜSSEN dieselben Ergebnisse liefern.
//! Das validiert den korrekten Zugriffspfad (Index-Kandidaten ⊇ Treffer,
//! Residual-Filter exakt).

use std::cmp::Ordering;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::error::Error;
use my_lsm_db::ordering;
use my_lsm_db::query::expression::eval;
use my_lsm_db::query::{Aggregate, Predicate, SortDir, eq, ge, gt, le, lt, ne};

/// Einfacher deterministischer Zufallsgenerator (LCG), wie in den anderen Tests.
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
    // age/score/active/name: manche Entities lassen Felder weg → Missing-Field.
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
    // "city": häufig vorhanden, gelegentlich fehlend.
    if rng.below(4) != 0 {
        let cities = ["DE", "US", "AT", "NL"];
        e.insert("city", Value::String(cities[rng.below(4) as usize].into()));
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
        2 => lt(f, v),
        3 => le(f, v),
        4 => gt(f, v),
        _ => ge(f, v),
    }
}

/// Erzeugt einen zufälligen Prädikat-Baum (And/Or/Not + Atome).
fn random_predicate(rng: &mut Rng, depth: u32) -> Predicate {
    let fields = ["age", "score", "active", "name", "city"];
    if depth == 0 || rng.below(4) != 0 {
        return random_atom(rng, &fields);
    }
    match rng.below(3) {
        0 => random_predicate(rng, depth - 1).and(random_predicate(rng, depth - 1)),
        1 => random_predicate(rng, depth - 1).or(random_predicate(rng, depth - 1)),
        _ => random_predicate(rng, depth - 1).negate(),
    }
}

/// Oracle-Sortierung — identisch zur Executor-Regel: Wert via `value_cmp`,
/// fehlendes Feld ist Asc-kleinst (Desc-größt), Tie-Breaker Entity-ID (Asc).
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
    sort: Option<(&str, SortDir)>,
    limit: Option<usize>,
) -> Vec<(String, Entity)> {
    let mut rows = store.scan_collection("users").unwrap();
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

fn execute(
    store: &mut EntityStore,
    pred: Option<&Predicate>,
    sort: Option<(&str, SortDir)>,
    limit: Option<usize>,
) -> Vec<(String, Entity)> {
    let mut b = store.query("users").unwrap();
    if let Some(p) = pred {
        b = b.filter(p.clone());
    }
    if let Some((f, d)) = sort {
        b = b.sort(f, d);
    }
    if let Some(n) = limit {
        b = b.limit(n);
    }
    store.execute_query(b).unwrap()
}

fn sorted_by_id(mut rows: Vec<(String, Entity)>) -> Vec<(String, Entity)> {
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// Führt eine Query mit Projektion aus (Result-Form), vergleichbar mit
/// `execute`, aber mit `.project(fields)`.
fn execute_proj(
    store: &mut EntityStore,
    pred: Option<&Predicate>,
    sort: Option<(&str, SortDir)>,
    limit: Option<usize>,
    fields: &[&str],
) -> Vec<(String, Entity)> {
    let mut b = store.query("users").unwrap();
    if let Some(p) = pred {
        b = b.filter(p.clone());
    }
    if let Some((f, d)) = sort {
        b = b.sort(f, d);
    }
    if let Some(n) = limit {
        b = b.limit(n);
    }
    b = b.project(fields);
    store.execute_query(b).unwrap()
}

/// Führt eine Query mit Aggregation aus (Terminal-Schritt).
fn execute_agg(
    store: &mut EntityStore,
    pred: Option<&Predicate>,
    sort: Option<(&str, SortDir)>,
    limit: Option<usize>,
    agg: &Aggregate,
) -> Option<Value> {
    let mut b = store.query("users").unwrap();
    if let Some(p) = pred {
        b = b.filter(p.clone());
    }
    if let Some((f, d)) = sort {
        b = b.sort(f, d);
    }
    if let Some(n) = limit {
        b = b.limit(n);
    }
    b = b.aggregate(agg.clone());
    store.execute_aggregate(b).unwrap()
}

/// Naive Oracle-Projektion: reduziert jede Zeile auf die angeforderten Felder
/// (in Anfrage-Reihenfolge); fehlende Felder werden weggelassen, `Null`
/// bleibt erhalten.
fn naive_project(rows: &[(String, Entity)], fields: &[&str]) -> Vec<(String, Entity)> {
    rows.iter()
        .map(|(id, e)| {
            let mut pe = Entity::new();
            for &f in fields {
                if let Some(v) = e.field(f) {
                    pe.insert(f, v.clone());
                }
            }
            (id.clone(), pe)
        })
        .collect()
}

/// Naive Oracle-Aggregation: unabhängige (sammelnde) Neuimplementierung der
/// v0.8-Semantik. NULL/absent/non-numeric werden übersprungen; `Sum`
/// akkumuliert in `i128` und sättigt auf `i64` (bzw. `Float64` bei Typmischung);
/// `Avg` ist `Float64`; `Min`/`Max` bleiben `Int64` bzw. werden bei Typmischung
/// zu `Float64` promoviert; nicht-endliche Floats werden übersprungen.
fn naive_aggregate(rows: &[(String, Entity)], agg: &Aggregate) -> Option<Value> {
    match agg {
        Aggregate::Count => Some(Value::Int(rows.len() as i64)),
        Aggregate::Sum(f) | Aggregate::Avg(f) | Aggregate::Min(f) | Aggregate::Max(f) => {
            let mut vals: Vec<Value> = Vec::new();
            for (_, e) in rows {
                match e.field(f) {
                    None | Some(Value::Null) => {}
                    Some(Value::Int(i)) => vals.push(Value::Int(*i)),
                    Some(Value::Float(x)) if x.is_finite() => vals.push(Value::Float(*x)),
                    Some(_) => {}
                }
            }
            if vals.is_empty() {
                return None;
            }
            match agg {
                Aggregate::Sum(_) => {
                    let mut has_float = false;
                    let mut isum: i128 = 0;
                    let mut fsum = 0.0;
                    for v in &vals {
                        match v {
                            Value::Int(i) => {
                                isum = isum.saturating_add(*i as i128);
                                fsum += *i as f64;
                            }
                            Value::Float(x) => {
                                has_float = true;
                                fsum += *x;
                            }
                            _ => {}
                        }
                    }
                    if has_float {
                        Some(Value::Float(fsum))
                    } else {
                        let s = if isum > i64::MAX as i128 {
                            i64::MAX
                        } else if isum < i64::MIN as i128 {
                            i64::MIN
                        } else {
                            isum as i64
                        };
                        Some(Value::Int(s))
                    }
                }
                Aggregate::Avg(_) => {
                    let mut fsum = 0.0;
                    for v in &vals {
                        fsum += match v {
                            Value::Int(i) => *i as f64,
                            Value::Float(x) => *x,
                            _ => 0.0,
                        };
                    }
                    Some(Value::Float(fsum / vals.len() as f64))
                }
                Aggregate::Min(_) | Aggregate::Max(_) => {
                    let is_min = matches!(agg, Aggregate::Min(_));
                    let mut has_float = false;
                    let mut iopt: Option<i64> = None;
                    let mut fopt: Option<f64> = None;
                    for v in &vals {
                        match v {
                            Value::Int(i) => {
                                iopt = Some(match iopt {
                                    None => *i,
                                    Some(c) => {
                                        if is_min {
                                            c.min(*i)
                                        } else {
                                            c.max(*i)
                                        }
                                    }
                                });
                                let f = *i as f64;
                                fopt = Some(match fopt {
                                    None => f,
                                    Some(c) => {
                                        if is_min {
                                            c.min(f)
                                        } else {
                                            c.max(f)
                                        }
                                    }
                                });
                            }
                            Value::Float(x) => {
                                has_float = true;
                                fopt = Some(match fopt {
                                    None => *x,
                                    Some(c) => {
                                        if is_min {
                                            c.min(*x)
                                        } else {
                                            c.max(*x)
                                        }
                                    }
                                });
                            }
                            _ => {}
                        }
                    }
                    if has_float {
                        fopt.map(Value::Float)
                    } else {
                        iopt.map(Value::Int)
                    }
                }
                _ => None,
            }
        }
    }
}

#[test]
fn explain_prints_tree() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
    }
    store
        .collection("users")
        .unwrap()
        .put("u1", &{
            let mut e = Entity::new();
            e.insert("age", int(30));
            e.insert("city", Value::String("DE".into()));
            e
        })
        .unwrap();

    // Fallback-Fall: `name` ist nicht indexiert → Sort bleibt (kein
    // IndexOrderScan, da die Enablement-Regel einen READY-Index verlangt).
    let b = store
        .query("users")
        .unwrap()
        .filter(ge("age", int(30)))
        .filter(eq("city", Value::String("DE".into())))
        .sort("name", SortDir::Asc)
        .limit(10);
    let s = store.explain_query(&b).unwrap();
    assert!(s.contains("IndexScan"), "expected index scan, got:\n{s}");
    assert!(s.contains("age"), "expected field age, got:\n{s}");
    assert!(s.contains("Filter"), "expected residual filter, got:\n{s}");
    assert!(s.contains("Sort"), "expected sort, got:\n{s}");
    assert!(s.contains("Limit"), "expected limit, got:\n{s}");
    println!("{s}");
}

/// Ein Query-Fall für den Oracle-Test.
type QueryCase = (
    Option<Predicate>,
    Option<(&'static str, SortDir)>,
    Option<usize>,
);

#[test]
fn basic_queries_match_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut rng = Rng(0xC0FFEE);
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        col.create_index("city").unwrap();
        for i in 0..200 {
            col.put(&format!("u{i}"), &entity(i, &mut rng)).unwrap();
        }
    }

    // Definierte Fälle: eq, range, missing-field, OR.
    let cases: Vec<QueryCase> = vec![
        (Some(eq("age", int(30))), None, None),
        (Some(ge("age", int(80))), None, None),
        (Some(gt("age", int(50)).and(lt("age", int(70)))), None, None),
        // Missing-Field: age > 1000 → nur Entities MIT age und age>1000.
        (Some(gt("age", int(1000))), None, None),
        // Not(Eq) auf fehlendem Feld: NOT(age=30) muss Entities OHNE age enthalten.
        (Some(eq("age", int(30)).negate()), None, None),
        (Some(ne("age", int(30))), None, None),
        // OR über zwei indizierte Felder.
        (
            Some(eq("age", int(30)).or(eq("city", Value::String("DE".into())))),
            None,
            None,
        ),
        // Sort + Limit.
        (
            Some(ge("age", int(0))),
            Some(("age", SortDir::Asc)),
            Some(5),
        ),
        (
            Some(ge("age", int(0))),
            Some(("age", SortDir::Desc)),
            Some(5),
        ),
    ];

    for (pred, sort, limit) in cases {
        let exp = oracle(&mut store, pred.as_ref(), sort, limit);
        let got = execute(&mut store, pred.as_ref(), sort, limit);
        if sort.is_some() {
            assert_eq!(got, exp, "sorted query mismatch");
        } else {
            assert_eq!(sorted_by_id(got), sorted_by_id(exp), "set query mismatch");
        }
    }
}

#[test]
fn random_queries_match_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut rng = Rng(0x5EED_2025);

    {
        let mut col = store.collection("users").unwrap();
        // Zufällig ein Teil der Felder indizieren.
        if rng.below(2) == 0 {
            col.create_index("age").unwrap();
        }
        if rng.below(2) == 0 {
            col.create_index("city").unwrap();
        }
        for i in 0..300 {
            col.put(&format!("u{i}"), &entity(i, &mut rng)).unwrap();
        }
    }

    for q in 0..200 {
        let depth = 1 + rng.below(2) as u32;
        let pred = if rng.below(2) == 0 {
            None
        } else {
            Some(random_predicate(&mut rng, depth))
        };
        // Limit nur zusammen mit Sort (undefinierte Reihenfolge sonst nicht vergleichbar).
        let sort = if rng.below(2) == 0 {
            None
        } else {
            let field = ["age", "score", "city"][rng.below(3) as usize];
            let dir = if rng.below(2) == 0 {
                SortDir::Asc
            } else {
                SortDir::Desc
            };
            Some((field, dir))
        };
        let limit = if sort.is_some() && rng.below(2) == 0 {
            Some(1 + rng.below(50) as usize)
        } else {
            None
        };

        let exp = oracle(&mut store, pred.as_ref(), sort, limit);
        let got = execute(&mut store, pred.as_ref(), sort, limit);
        let tag = format!("q{q}: pred={:?} sort={:?} limit={:?}", pred, sort, limit);
        if sort.is_some() {
            assert_eq!(got, exp, "sorted mismatch: {tag}");
        } else {
            assert_eq!(sorted_by_id(got), sorted_by_id(exp), "set mismatch: {tag}");
        }
    }
}

#[test]
fn missing_field_ne_vs_not_eq_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        // u1: hat age=30; u2: hat age=40; u3: KEIN age, aber "name" (existiert im
        // Store, Feld age fehlt → Missing-Field-Semantik).
        let mut e = Entity::new();
        e.insert("age", int(30));
        col.put("u1", &e).unwrap();
        let mut e = Entity::new();
        e.insert("age", int(40));
        col.put("u2", &e).unwrap();
        let mut e = Entity::new();
        e.insert("name", Value::String("u3".into()));
        col.put("u3", &e).unwrap();
    }

    let a = execute(&mut store, Some(&eq("age", int(30)).negate()), None, None);
    let b = execute(&mut store, Some(&ne("age", int(30))), None, None);
    let mut a = sorted_by_id(a);
    let b = sorted_by_id(b);
    assert_eq!(a, b, "NOT(Eq) must equal Ne, incl. missing field");
    let ids: Vec<String> = a.drain(..).map(|(id, _)| id).collect();
    assert!(ids.contains(&"u2".to_string()));
    assert!(
        ids.contains(&"u3".to_string()),
        "missing-field entity u3 must match Ne/NOT(Eq)"
    );
    assert!(!ids.contains(&"u1".to_string()));
}

// ===========================================================================
// v0.8 — Projektion + Aggregation (siehe design-v0.8-query.md)
// ===========================================================================

#[test]
fn projection_basic_order_and_missing() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();

    let mut e1 = Entity::new();
    e1.insert("name", Value::String("alice".into()));
    e1.insert("age", int(30));
    e1.insert("active", Value::Bool(true));
    col.put("u1", &e1).unwrap();

    let mut e2 = Entity::new();
    e2.insert("name", Value::String("bob".into()));
    e2.insert("nick", Value::Null);
    col.put("u2", &e2).unwrap();

    let got = execute_proj(&mut store, None, None, None, &["name", "age", "active", "missing"]);

    let u1 = got.iter().find(|(id, _)| id == "u1").unwrap().1.clone();
    assert_eq!(u1.field("name"), Some(&Value::String("alice".into())));
    assert_eq!(u1.field("age"), Some(&int(30)));
    assert_eq!(u1.field("active"), Some(&Value::Bool(true)));
    assert!(u1.field("missing").is_none());
    // Anfrage-Reihenfolge erhalten.
    assert_eq!(
        u1.fields.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        vec!["name", "age", "active"]
    );

    // u2 hat nur "name"; alle anderen (auch present-Null "nick") sind nicht
    // angefordert → nur das eine Feld.
    let u2 = got.iter().find(|(id, _)| id == "u2").unwrap().1.clone();
    assert_eq!(u2.fields, vec![("name".into(), Value::String("bob".into()))]);
}

#[test]
fn projection_unknown_field_omitted() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();
    let mut e = Entity::new();
    e.insert("name", Value::String("x".into()));
    col.put("u1", &e).unwrap();

    let got = execute_proj(&mut store, None, None, None, &["nonexistent"]);
    assert_eq!(got.len(), 1);
    assert!(got[0].1.fields.is_empty());
}

#[test]
fn empty_projection_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();
    let mut e = Entity::new();
    e.insert("name", Value::String("x".into()));
    col.put("u1", &e).unwrap();

    let b = store.query("users").unwrap().project(&[]);
    let res = store.execute_query(b);
    assert!(matches!(res, Err(Error::InvalidArgument(_))));
}

#[test]
fn projection_and_aggregation_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();
    let mut e = Entity::new();
    e.insert("age", int(10));
    col.put("u1", &e).unwrap();

    // Aggregation gesetzt, aber execute_query → Fehler.
    let b = store.query("users").unwrap().aggregate(Aggregate::Count);
    assert!(matches!(store.execute_query(b), Err(Error::InvalidArgument(_))));

    // Projektion gesetzt, aber execute_aggregate → Fehler.
    let b = store.query("users").unwrap().project(&["age"]);
    assert!(matches!(
        store.execute_aggregate(b),
        Err(Error::InvalidArgument(_))
    ));

    // Beides gesetzt → beide Aufrufe fehlerhaft.
    let b = store
        .query("users")
        .unwrap()
        .project(&["age"])
        .aggregate(Aggregate::Count);
    assert!(matches!(
        store.execute_query(b.clone()),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        store.execute_aggregate(b),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn aggregation_count_over_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();
    for i in 0..20 {
        let mut e = Entity::new();
        e.insert("age", int(i as i64));
        col.put(&format!("u{i}"), &e).unwrap();
    }
    let got = execute_agg(
        &mut store,
        Some(&ge("age", int(0))),
        Some(("age", SortDir::Asc)),
        Some(5),
        &Aggregate::Count,
    );
    assert_eq!(got, Some(Value::Int(5)));
    let got = execute_agg(&mut store, None, None, None, &Aggregate::Count);
    assert_eq!(got, Some(Value::Int(20)));
}

#[test]
fn aggregation_sum_saturates_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();
    for _ in 0..2 {
        let mut e = Entity::new();
        e.insert("age", Value::Int(i64::MAX));
        col.put(&format!("u{}", rng_id()), &e).unwrap();
    }
    let got = execute_agg(&mut store, None, None, None, &Aggregate::Sum("age".into()));
    assert_eq!(got, Some(Value::Int(i64::MAX)), "sum must saturate at i64::MAX");

    let dir2 = tempfile::tempdir().unwrap();
    let mut s2 = EntityStore::open(dir2.path()).unwrap();
    let mut c = s2.collection("users").unwrap();
    for _ in 0..2 {
        let mut e = Entity::new();
        e.insert("age", Value::Int(i64::MIN));
        c.put(&format!("u{}", rng_id()), &e).unwrap();
    }
    let got = execute_agg(&mut s2, None, None, None, &Aggregate::Sum("age".into()));
    assert_eq!(got, Some(Value::Int(i64::MIN)), "sum must saturate at i64::MIN");
}

// Kleiner Hilfszähler, um eindeutige IDs in Schleifen zu erzeugen.
fn rng_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering as A};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, A::SeqCst)
}

#[test]
fn aggregation_skips_null_absent_nonnumeric() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();

    let mut e = Entity::new();
    e.insert("age", int(10));
    col.put("u1", &e).unwrap();
    let mut e = Entity::new();
    e.insert("age", Value::Null);
    col.put("u2", &e).unwrap();
    let mut e = Entity::new();
    e.insert("name", Value::String("x".into()));
    col.put("u3", &e).unwrap();
    let mut e = Entity::new();
    e.insert("age", Value::String("oops".into()));
    col.put("u4", &e).unwrap();

    // Nur u1 (Int 10) zählt; Null/absent/String werden übersprungen.
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Sum("age".into())),
        Some(Value::Int(10))
    );
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Avg("age".into())),
        Some(Value::Float(10.0))
    );
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Min("age".into())),
        Some(Value::Int(10))
    );
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Max("age".into())),
        Some(Value::Int(10))
    );
    // Alle vier Zeilen existieren → count = 4.
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Count),
        Some(Value::Int(4))
    );
}

#[test]
fn aggregation_float_promotion_and_avg() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();
    let mut e = Entity::new();
    e.insert("score", int(10));
    col.put("u1", &e).unwrap();
    let mut e = Entity::new();
    e.insert("score", Value::Float(5.5));
    col.put("u2", &e).unwrap();
    let mut e = Entity::new();
    e.insert("score", int(20));
    col.put("u3", &e).unwrap();

    // Ein Float vorhanden → Summe/Limits werden zu Float64 promoviert.
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Sum("score".into())),
        Some(Value::Float(35.5))
    );
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Avg("score".into())),
        Some(Value::Float(35.5 / 3.0))
    );
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Min("score".into())),
        Some(Value::Float(5.5))
    );
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Max("score".into())),
        Some(Value::Float(20.0))
    );

    // Rein ganzzahlig → Int64.
    let dir2 = tempfile::tempdir().unwrap();
    let mut s2 = EntityStore::open(dir2.path()).unwrap();
    let mut c = s2.collection("users").unwrap();
    for v in [1i64, 2, 3] {
        let mut e = Entity::new();
        e.insert("score", int(v));
        c.put(&format!("u{v}"), &e).unwrap();
    }
    assert_eq!(
        execute_agg(&mut s2, None, None, None, &Aggregate::Sum("score".into())),
        Some(Value::Int(6))
    );
    assert_eq!(
        execute_agg(&mut s2, None, None, None, &Aggregate::Min("score".into())),
        Some(Value::Int(1))
    );
}

#[test]
fn aggregation_nan_inf_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();
    let mut e = Entity::new();
    e.insert("score", Value::Float(f64::NAN));
    col.put("u1", &e).unwrap();
    let mut e = Entity::new();
    e.insert("score", Value::Float(f64::INFINITY));
    col.put("u2", &e).unwrap();
    let mut e = Entity::new();
    e.insert("score", int(7));
    col.put("u3", &e).unwrap();

    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Sum("score".into())),
        Some(Value::Int(7))
    );
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Min("score".into())),
        Some(Value::Int(7))
    );
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Max("score".into())),
        Some(Value::Int(7))
    );
    assert_eq!(
        execute_agg(&mut store, None, None, None, &Aggregate::Avg("score".into())),
        Some(Value::Float(7.0))
    );
    // Nur NaN + Inf → alle numerischen Werte übersprungen.
    let dir2 = tempfile::tempdir().unwrap();
    let mut s2 = EntityStore::open(dir2.path()).unwrap();
    let mut c = s2.collection("users").unwrap();
    let mut e = Entity::new();
    e.insert("score", Value::Float(f64::NAN));
    c.put("u1", &e).unwrap();
    let mut e = Entity::new();
    e.insert("score", Value::Float(f64::NEG_INFINITY));
    c.put("u2", &e).unwrap();
    assert_eq!(
        execute_agg(&mut s2, None, None, None, &Aggregate::Sum("score".into())),
        None
    );
    assert_eq!(
        execute_agg(&mut s2, None, None, None, &Aggregate::Count),
        Some(Value::Int(2))
    );
}

#[test]
fn aggregation_over_index_topk_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();
    col.create_index("age").unwrap();
    for i in 0..100 {
        let mut e = Entity::new();
        e.insert("age", int(i as i64));
        col.put(&format!("u{i}"), &e).unwrap();
    }
    for n in [1usize, 3, 10, 50] {
        let count = execute_agg(
            &mut store,
            Some(&ge("age", int(0))),
            Some(("age", SortDir::Asc)),
            Some(n),
            &Aggregate::Count,
        );
        assert_eq!(count, Some(Value::Int(n as i64)), "count over topk n={n}");
        let max = execute_agg(
            &mut store,
            Some(&ge("age", int(0))),
            Some(("age", SortDir::Asc)),
            Some(n),
            &Aggregate::Max("age".into()),
        );
        assert_eq!(max, Some(Value::Int((n - 1) as i64)), "max over topk n={n}");
    }
}

#[test]
fn random_projection_matches_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut rng = Rng(0xAB_C0DE);
    {
        let mut col = store.collection("users").unwrap();
        if rng.below(2) == 0 {
            col.create_index("age").unwrap();
        }
        if rng.below(2) == 0 {
            col.create_index("city").unwrap();
        }
        for i in 0..250 {
            col.put(&format!("u{i}"), &entity(i, &mut rng)).unwrap();
        }
    }
    let all_fields = ["name", "age", "score", "active", "city"];
    for q in 0..150 {
        let pred = if rng.below(2) == 0 {
            None
        } else {
            let depth = 1 + rng.below(2) as u32;
            Some(random_predicate(&mut rng, depth))
        };
        let sort = if rng.below(2) == 0 {
            None
        } else {
            let field = all_fields[rng.below(all_fields.len() as u64) as usize];
            let dir = if rng.below(2) == 0 {
                SortDir::Asc
            } else {
                SortDir::Desc
            };
            Some((field, dir))
        };
        let limit = if sort.is_some() && rng.below(2) == 0 {
            Some(1 + rng.below(40) as usize)
        } else {
            None
        };
        // Nichtleere Projektions-Teilmenge.
        let k = 1 + rng.below(all_fields.len() as u64) as usize;
        let mut proj: Vec<&str> = all_fields.to_vec();
        proj.rotate_left(rng.below(all_fields.len() as u64) as usize);
        let proj = &proj[..k];

        let rows = oracle(&mut store, pred.as_ref(), sort, limit);
        let exp = naive_project(&rows, proj);
        let got = execute_proj(&mut store, pred.as_ref(), sort, limit, proj);
        let tag = format!(
            "q{q}: pred={:?} sort={:?} limit={:?} proj={:?}",
            pred, sort, limit, proj
        );
        if sort.is_some() {
            assert_eq!(got, exp, "proj sorted mismatch: {tag}");
        } else {
            assert_eq!(sorted_by_id(got), sorted_by_id(exp), "proj set mismatch: {tag}");
        }
    }
}

#[test]
fn random_aggregation_matches_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut rng = Rng(0xBA_DF00);
    {
        let mut col = store.collection("users").unwrap();
        if rng.below(2) == 0 {
            col.create_index("age").unwrap();
        }
        if rng.below(2) == 0 {
            col.create_index("city").unwrap();
        }
        for i in 0..250 {
            col.put(&format!("u{i}"), &entity(i, &mut rng)).unwrap();
        }
    }
    let num_fields = ["age", "score"];
    for q in 0..200 {
        let pred = if rng.below(2) == 0 {
            None
        } else {
            let depth = 1 + rng.below(2) as u32;
            Some(random_predicate(&mut rng, depth))
        };
        let sort = if rng.below(2) == 0 {
            None
        } else {
            let field = ["age", "score", "city"][rng.below(3) as usize];
            let dir = if rng.below(2) == 0 {
                SortDir::Asc
            } else {
                SortDir::Desc
            };
            Some((field, dir))
        };
        let limit = if sort.is_some() && rng.below(2) == 0 {
            Some(1 + rng.below(40) as usize)
        } else {
            None
        };
        let pick = rng.below(5);
        let agg = match pick {
            0 => Aggregate::Count,
            1 => Aggregate::Sum(num_fields[rng.below(2) as usize].into()),
            2 => Aggregate::Avg(num_fields[rng.below(2) as usize].into()),
            3 => Aggregate::Min(num_fields[rng.below(2) as usize].into()),
            _ => Aggregate::Max(num_fields[rng.below(2) as usize].into()),
        };
        let rows = oracle(&mut store, pred.as_ref(), sort, limit);
        let exp = naive_aggregate(&rows, &agg);
        let got = execute_agg(&mut store, pred.as_ref(), sort, limit, &agg);
        let tag = format!(
            "q{q}: pred={:?} sort={:?} limit={:?} agg={:?}",
            pred, sort, limit, agg
        );
        assert_eq!(got, exp, "agg mismatch: {tag}");
    }
}
