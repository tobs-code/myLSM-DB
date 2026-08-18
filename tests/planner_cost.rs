//! v0.6 Teil 2: Cost-based Index-Wahl im Planner.
//!
//! `pick_index_field_cost` wählt pro AND-Klausel das günstigste Feld:
//! `cost = BASE_CARDINALITY * selectivity(shape)` mit `Eq < Between < OneSided`,
//! bei gleicher Selektivität das enger gebundene Literal, sonst lex kleinstes
//! Feldname. Korrektheit bleibt unverändert — die nicht gewählten indexierbaren
//! Felder und alle negierten/nicht-indexierbaren Literale landen im Residual.

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::query::expression::eval;
use my_lsm_db::query::{Predicate, eq, gt, lt};

fn int(v: i64) -> Value {
    Value::Int(v)
}

fn explain(store: &mut EntityStore, pred: &Predicate) -> String {
    let b = store.query("users").unwrap().filter(pred.clone());
    store.explain_query(&b).unwrap()
}

/// Legt eine Collection mit Indizes auf age/score/salary an.
fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        col.create_index("score").unwrap();
        col.create_index("salary").unwrap();
    }
    drop(store);
    dir
}

#[test]
fn plan_selection_is_deterministic() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let b = store
        .query("users")
        .unwrap()
        .filter(gt("age", int(10)).and(gt("score", int(100))));
    let e1 = store.explain_query(&b).unwrap();
    let e2 = store.explain_query(&b).unwrap();
    assert_eq!(e1, e2, "explain must be deterministic across calls");
}

#[test]
fn eq_field_beats_range_field() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let s = explain(&mut store, &eq("score", int(50)).and(gt("age", int(10))));
    assert!(s.contains("field: score"), "expected score chosen:\n{s}");
    assert!(!s.contains("field: age"), "unexpected age scan:\n{s}");
}

#[test]
fn between_beats_one_sided_range() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    // age ist ein Between (0.25), score nur einseitig (0.5) → age.
    let s = explain(
        &mut store,
        &gt("age", int(10))
            .and(lt("age", int(50)))
            .and(gt("score", int(100))),
    );
    assert!(s.contains("field: age"), "expected age chosen:\n{s}");
    assert!(!s.contains("field: score"), "unexpected score scan:\n{s}");
}

#[test]
fn tighter_one_sided_bound_wins() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    // Beide einseitig: age > 60 ist enger als salary > 10 → age.
    let s1 = explain(&mut store, &gt("age", int(60)).and(gt("salary", int(10))));
    assert!(s1.contains("field: age"), "expected age chosen:\n{s1}");
    // Umgekehrt: salary > 60 enger als age > 10 → salary.
    let s2 = explain(&mut store, &gt("age", int(10)).and(gt("salary", int(60))));
    assert!(
        s2.contains("field: salary"),
        "expected salary chosen:\n{s2}"
    );
}

#[test]
fn lex_field_name_is_final_tie_break() {
    let dir = setup();
    let mut store = EntityStore::open(dir.path()).unwrap();
    // Gleiche Selektivität UND gleiche Bounds (50 vs 50) → lex kleinstes Feld.
    let s = explain(&mut store, &gt("age", int(50)).and(gt("salary", int(50))));
    assert!(s.contains("field: age"), "expected age chosen:\n{s}");
}

/// Full-Scan-Oracle (dieselbe `eval` wie der Executor), über den Overlay.
fn oracle(store: &mut EntityStore, pred: &Predicate) -> Vec<(String, Entity)> {
    let mut rows = store.scan_collection("users").unwrap();
    rows.retain(|(_, e)| eval(e, pred).unwrap());
    rows
}

fn sorted_by_id(mut rows: Vec<(String, Entity)>) -> Vec<(String, Entity)> {
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

#[test]
fn cost_choice_matches_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        col.create_index("score").unwrap();
        col.create_index("salary").unwrap();
        for i in 0..200 {
            let mut e = Entity::new();
            if i % 5 != 0 {
                e.insert("age", int((i as i64 % 100) - 20));
            }
            if i % 7 != 0 {
                e.insert("score", int((i as i64 * 3) % 1000));
            }
            if i % 11 != 0 {
                e.insert("salary", int((i as i64 * 7) % 1000));
            }
            col.put(&format!("u{i}"), &e).unwrap();
        }
    }

    // Fälle, bei denen das Cost-Modell verschiedene Felder wählt.
    let cases: Vec<Predicate> = vec![
        gt("age", int(30)).and(gt("salary", int(500))),
        gt("age", int(10))
            .and(lt("age", int(70)))
            .and(gt("score", int(200))),
        eq("score", int(300)).and(gt("age", int(0))),
        gt("age", int(60)).and(gt("salary", int(10))),
        gt("age", int(10)).and(gt("salary", int(60))),
        gt("score", int(100)).and(gt("salary", int(100))),
    ];
    for pred in &cases {
        let b = store.query("users").unwrap().filter(pred.clone());
        let got = store.execute_query(b).unwrap();
        assert_eq!(
            sorted_by_id(got),
            sorted_by_id(oracle(&mut store, pred)),
            "cost-based choice broke oracle equivalence for {pred:?}"
        );
    }
}
