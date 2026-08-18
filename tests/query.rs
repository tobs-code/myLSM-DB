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
use my_lsm_db::ordering;
use my_lsm_db::query::expression::eval;
use my_lsm_db::query::{Predicate, SortDir, eq, ge, gt, le, lt, ne};

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

    let b = store
        .query("users")
        .unwrap()
        .filter(ge("age", int(30)))
        .filter(eq("city", Value::String("DE".into())))
        .sort("age", SortDir::Asc)
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
