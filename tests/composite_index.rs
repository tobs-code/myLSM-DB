//! Oracle-Tests für Composite-Indexe (v1.3).
//!
//! Strategie: Für jede Query wird das Ergebnis des geplanten Pfads (evtl.
//! CompositeIndexScan) gegen eine naive Vollscan-Auswertung (`eval`) verglichen.
//! Stimmen beide überein, ist der Index korrekt — inkl. NULL-vs-absent,
//! gemischter Typen, Duplikaten, Mutationen und Parallelität zu
//! Single-Field-Indizes. Zusätzlich werden einzelne Fälle gegen eine explizit
//! erwartete Entity-Menge geprüft.

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::error::Result;
use my_lsm_db::index::{Bound, FindOp};
use my_lsm_db::query::expression::eval;
use my_lsm_db::query::{Aggregate, Predicate, SortDir, eq, ge, gt, le, ne};

fn e(fields: &[(&str, Value)]) -> Entity {
    let mut x = Entity::new();
    for (n, v) in fields {
        x.insert(*n, v.clone());
    }
    x
}

fn insert(store: &mut EntityStore, id: &str, fields: &[(&str, Value)]) {
    store
        .collection("users")
        .unwrap()
        .put(id, &e(fields))
        .unwrap();
}

/// Führt eine Query über den Planner aus und liefert sortierte Entity-IDs.
fn run_query(store: &mut EntityStore, pred: Option<Predicate>) -> Vec<String> {
    let mut b = store.query("users").unwrap();
    if let Some(p) = pred {
        b = b.filter(p);
    }
    let rows = store.execute_query(b).unwrap();
    let mut ids: Vec<String> = rows.into_iter().map(|(id, _)| id).collect();
    ids.sort();
    ids
}

/// Naive Referenz: Vollscan + `eval`.
fn naive_ids(store: &mut EntityStore, pred: Option<Predicate>) -> Vec<String> {
    let all = store.scan_collection("users").unwrap();
    let mut ids: Vec<String> = all
        .into_iter()
        .filter(|(_, ent)| match &pred {
            None => true,
            Some(p) => eval(ent, p).unwrap_or(false),
        })
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    ids
}

fn assert_plan_eq_naive(store: &mut EntityStore, pred: Option<Predicate>) {
    let p2 = pred.clone();
    let got = run_query(store, p2.clone());
    let exp = naive_ids(store, p2);
    assert_eq!(got, exp, "planner != naive for pred {:?}", pred);
}

#[test]
fn composite_oracle() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();

    // Datenmodell: last, first, country, age, name, team, score, kind, val.
    insert(&mut store, "u1", &[
        ("last", Value::String("Smith".into())),
        ("first", Value::String("John".into())),
        ("country", Value::String("DE".into())),
        ("age", Value::Int(30)),
        ("name", Value::String("Alice".into())),
        ("team", Value::String("T1".into())),
        ("score", Value::Int(10)),
        ("kind", Value::String("a".into())),
        ("val", Value::Int(5)),
    ]);
    insert(&mut store, "u2", &[
        ("last", Value::String("Smith".into())),
        ("first", Value::String("John".into())),
        ("country", Value::String("DE".into())),
        ("age", Value::Int(40)),
        ("name", Value::String("Bob".into())),
        ("team", Value::String("T1".into())),
        ("score", Value::Null), // present NULL
        ("kind", Value::String("a".into())),
        ("val", Value::Int(5)),
    ]);
    insert(&mut store, "u3", &[
        ("last", Value::String("Smith".into())),
        ("first", Value::String("Jane".into())),
        ("country", Value::String("US".into())),
        ("age", Value::Int(25)),
        ("name", Value::String("Carol".into())),
        ("team", Value::String("T2".into())),
        ("score", Value::Int(20)),
        ("kind", Value::String("b".into())),
        ("val", Value::String("x".into())),
    ]);
    insert(&mut store, "u4", &[
        ("last", Value::String("Doe".into())),
        ("first", Value::String("John".into())),
        ("country", Value::String("DE".into())),
        ("age", Value::Int(35)),
        ("name", Value::String("Alice".into())),
        ("team", Value::String("T1".into())),
        // score ABSENT
        ("kind", Value::String("a".into())),
        ("val", Value::Int(5)),
    ]);
    insert(&mut store, "u5", &[
        ("last", Value::String("Doe".into())),
        ("first", Value::String("Jane".into())),
        ("country", Value::String("US".into())),
        ("age", Value::Int(50)),
        ("name", Value::String("Bob".into())),
        ("team", Value::String("T2".into())),
        ("score", Value::Int(10)),
        ("kind", Value::String("b".into())),
        ("val", Value::String("y".into())),
    ]);
    insert(&mut store, "u6", &[
        ("last", Value::String("Smith".into())),
        ("first", Value::String("Jane".into())),
        ("country", Value::String("DE".into())),
        ("age", Value::Int(30)),
        ("name", Value::String("Carol".into())),
        ("team", Value::String("T1".into())),
        ("score", Value::Null),
        ("kind", Value::String("a".into())),
        ("val", Value::Int(5)),
    ]);
    insert(&mut store, "u7", &[
        ("last", Value::String("Smith".into())),
        ("first", Value::String("Jane".into())),
        ("country", Value::String("DE".into())),
        ("age", Value::Int(30)),
        ("name", Value::String("Alice".into())),
        ("team", Value::String("T2".into())),
        ("score", Value::Int(10)),
        ("kind", Value::String("a".into())),
        ("val", Value::Int(5)),
    ]);

    // Indizes anlegen (rebuild über bestehende Daten).
    {
        let mut col = store.collection("users").unwrap();
        col.create_composite_index(&["last", "first"]).unwrap();
        col.create_composite_index(&["country", "age", "name"]).unwrap();
        col.create_composite_index(&["team", "score"]).unwrap();
        col.create_composite_index(&["kind", "val"]).unwrap();
        // Parallele Single-Field-Indizes.
        col.create_index("age").unwrap();
        col.create_index("first").unwrap();
    }

    // --- Oracle: planner ≡ naive für diverse Prädikate ---
    assert_plan_eq_naive(&mut store, None);
    assert_plan_eq_naive(&mut store, Some(eq("last", Value::String("Smith".into()))));
    assert_plan_eq_naive(&mut store, Some(
        eq("last", Value::String("Smith".into()))
            .and(eq("first", Value::String("John".into()))),
    ));
    assert_plan_eq_naive(&mut store, Some(
        eq("country", Value::String("DE".into()))
            .and(gt("age", Value::Int(30))),
    ));
    assert_plan_eq_naive(&mut store, Some(
        eq("country", Value::String("DE".into()))
            .and(eq("age", Value::Int(30)))
            .and(eq("name", Value::String("Alice".into()))),
    ));
    // Range-Only auf letzter Composite-Komponente (country eq, age range).
    assert_plan_eq_naive(&mut store, Some(
        eq("country", Value::String("DE".into()))
            .and(ge("age", Value::Int(30)))
            .and(le("age", Value::Int(40))),
    ));
    // NULL vs absent: team=T1 AND score=NULL.
    assert_plan_eq_naive(&mut store, Some(
        eq("team", Value::String("T1".into()))
            .and(eq("score", Value::Null)),
    ));
    // Mixed types in val-Komponente.
    assert_plan_eq_naive(&mut store, Some(
        eq("kind", Value::String("a".into()))
            .and(eq("val", Value::Int(5))),
    ));
    // Prädikat, das der Composite-Präfixregel nicht genügt (erste Komponente
    // ungebunden) → fällt auf Single-Field-Index / FullScan zurück.
    assert_plan_eq_naive(&mut store, Some(eq("first", Value::String("John".into()))));
    assert_plan_eq_naive(&mut store, Some(eq("age", Value::Int(30))));
    // Ungleichheit (Residual-Filter über Index-Kandidaten).
    assert_plan_eq_naive(&mut store, Some(
        eq("last", Value::String("Smith".into()))
            .and(ne("first", Value::String("John".into()))),
    ));

    // --- Explizite Erwartungsmengen (direktes find_composite) ---
    // 2-Feld-Equality liefert Duplikat-Tupel (u1, u2).
    let ids = store.collection("users").unwrap().find_composite(&["last", "first"], &[
        (0, Bound::Inclusive(Value::String("Smith".into())), Bound::Inclusive(Value::String("Smith".into()))),
        (1, Bound::Inclusive(Value::String("John".into())), Bound::Inclusive(Value::String("John".into()))),
    ]).unwrap();
    let mut ids = ids;
    ids.sort();
    assert_eq!(ids, vec!["u1".to_string(), "u2".to_string()]);

    // NULL (present) wird gefunden, absent nicht: team=T1 AND score=NULL.
    let ids = store.collection("users").unwrap().find_composite(&["team", "score"], &[
        (0, Bound::Inclusive(Value::String("T1".into())), Bound::Inclusive(Value::String("T1".into()))),
        (1, Bound::Inclusive(Value::Null), Bound::Inclusive(Value::Null)),
    ]).unwrap();
    let mut ids = ids;
    ids.sort();
    assert_eq!(ids, vec!["u2".to_string(), "u6".to_string()]);

    // --- Mutation: Update eines Index-Feldes ---
    // u1: last Smith→Changed. Danach darf (Smith,John) u1 nicht mehr liefern.
    store.collection("users").unwrap().put("u1", &e(&[
        ("last", Value::String("Changed".into())),
        ("first", Value::String("John".into())),
        ("country", Value::String("DE".into())),
        ("age", Value::Int(30)),
        ("name", Value::String("Alice".into())),
        ("team", Value::String("T1".into())),
        ("score", Value::Int(10)),
        ("kind", Value::String("a".into())),
        ("val", Value::Int(5)),
    ])).unwrap();
    assert_plan_eq_naive(&mut store, Some(
        eq("last", Value::String("Smith".into()))
            .and(eq("first", Value::String("John".into()))),
    ));
    let ids = store.collection("users").unwrap().find_composite(&["last", "first"], &[
        (0, Bound::Inclusive(Value::String("Changed".into())), Bound::Inclusive(Value::String("Changed".into()))),
        (1, Bound::Inclusive(Value::String("John".into())), Bound::Inclusive(Value::String("John".into()))),
    ]).unwrap();
    let mut ids = ids;
    ids.sort();
    assert_eq!(ids, vec!["u1".to_string()]);

    // --- Mutation: Delete / Reinsert ---
    store.collection("users").unwrap().delete("u3").unwrap();
    assert_plan_eq_naive(&mut store, Some(
        eq("last", Value::String("Smith".into()))
            .and(eq("first", Value::String("Jane".into()))),
    ));
    // Wieder einfügen.
    store.collection("users").unwrap().put("u3", &e(&[
        ("last", Value::String("Smith".into())),
        ("first", Value::String("Jane".into())),
        ("country", Value::String("US".into())),
        ("age", Value::Int(25)),
        ("name", Value::String("Carol".into())),
        ("team", Value::String("T2".into())),
        ("score", Value::Int(20)),
        ("kind", Value::String("b".into())),
        ("val", Value::String("x".into())),
    ])).unwrap();
    assert_plan_eq_naive(&mut store, Some(
        eq("last", Value::String("Smith".into()))
            .and(eq("first", Value::String("Jane".into()))),
    ));

    // --- ORDER BY über Composite-Komponente (IndexOrderScan) bleibt korrekt ---
    let mut b = store.query("users").unwrap();
    b = b
        .filter(
            eq("country", Value::String("DE".into()))
                .and(gt("age", Value::Int(0))),
        )
        .sort("age", SortDir::Asc)
        .limit(100);
    let rows = store.execute_query(b).unwrap();
    assert!(!rows.is_empty());
    let ages: Vec<i64> = rows
        .iter()
        .map(|(_, ent)| ent.field("age").cloned().unwrap())
        .map(|v| match v {
            Value::Int(i) => i,
            _ => panic!("age not int"),
        })
        .collect();
    let mut sorted = ages.clone();
    sorted.sort();
    assert_eq!(ages, sorted, "ORDER BY age must be sorted");

    // --- Aggregation über gefilterte Composite-Ergebnismenge ---
    let mut b = store.query("users").unwrap();
    b = b
        .filter(eq("last", Value::String("Smith".into())))
        .aggregate(Aggregate::Count);
    let c = store.execute_aggregate(b).unwrap();
    assert_eq!(c, Some(Value::Int(4))); // u2,u3,u6,u7 (u1 wurde zu "Changed" mutiert)

    Ok(())
}

/// Stellt sicher, dass ein Composite-Index nach Reopen (Schema-Persistenz +
/// ggf. Recovery) weiterhin korrekt funktioniert.
#[test]
fn composite_survives_reopen() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = EntityStore::open(dir.path()).unwrap();
        store.collection("users").unwrap().put("u1", &e(&[
            ("last", Value::String("Smith".into())),
            ("first", Value::String("John".into())),
            ("age", Value::Int(30)),
        ])).unwrap();
        store
            .collection("users")
            .unwrap()
            .create_composite_index(&["last", "first"])
            .unwrap();
    }
    // Reopen: Index-Definition muss geladen werden, Keys bleiben lesbar.
    let mut store = EntityStore::open(dir.path()).unwrap();
    let ids = store.collection("users").unwrap().find_composite(&["last", "first"], &[
        (0, Bound::Inclusive(Value::String("Smith".into())), Bound::Inclusive(Value::String("Smith".into()))),
        (1, Bound::Inclusive(Value::String("John".into())), Bound::Inclusive(Value::String("John".into()))),
    ]).unwrap();
    assert_eq!(ids, vec!["u1".to_string()]);
    Ok(())
}

/// Legacypfad: ein v1.2-Single-Field-Index (Schema im neuen Format) bleibt
/// nutzbar und liefert korrekte Ergebnisse.
#[test]
fn legacy_single_field_still_works() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    store.collection("users").unwrap().put(
        "u1",
        &e(&[("age", Value::Int(30)), ("name", Value::String("A".into()))]),
    ).unwrap();
    store.collection("users").unwrap().put(
        "u2",
        &e(&[("age", Value::Int(31)), ("name", Value::String("B".into()))]),
    ).unwrap();
    store.collection("users").unwrap().create_index("age").unwrap();
    let ids = store
        .collection("users")
        .unwrap()
        .find("age", FindOp::Eq(Value::Int(30)))
        .unwrap();
    let mut ids = ids;
    ids.sort();
    assert_eq!(ids, vec!["u1".to_string()]);
    Ok(())
}
