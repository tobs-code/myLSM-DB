//! v1.2 CAS + Partial Entity Update — unabhängige Oracle-Regression.
//!
//! Deckt den Spezifikationsvertrag ab: `Error::Conflict`, wertebasierte
//! Vergleichsbasis (`Expected::{Any,Absent,Entity,Field}`), Partial-Op
//! (`Patch::{Set,Remove,Increment}`), identische Semantik Direct ≡ In-Tx,
//! Index-/Hint-Aktualisierung ausschliesslich über `core_put_entity` und
//! atomare WAL/Commit-Semantik (kein halber Zustand).

use tempfile::tempdir;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore, Expected, Patch};
use my_lsm_db::error::{ConflictReason, Error};
use my_lsm_db::index::FindOp;
use my_lsm_db::Options;

fn opts() -> Options {
    Options::default()
}

fn entity(pairs: &[(&str, Value)]) -> Entity {
    let mut e = Entity::new();
    for (k, v) in pairs {
        e.insert(*k, v.clone());
    }
    e
}

// ---------------------------------------------------------------------------
// Invariante: CAS-Hit → genau eine Mutation (nur das gepatchte Feld ändert sich)
// ---------------------------------------------------------------------------
#[test]
fn cas_hit_exactly_one_mutation() {
    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("alice".into())), ("age", Value::Int(30)), ("city", Value::String("berlin".into()))])).unwrap();
    drop(coll);

    let before = {
        let mut c = store.collection("users").unwrap();
        c.get("u1").unwrap().unwrap()
    };
    let res = store
        .cas_update("users", "u1", &Expected::Entity(before.clone()), &[Patch::Set("age".into(), Value::Int(31))])
        .unwrap();
    assert_eq!(res.field("age"), Some(&Value::Int(31)));
    assert_eq!(res.field("name"), Some(&Value::String("alice".into())));
    assert_eq!(res.field("city"), Some(&Value::String("berlin".into())));
    assert_eq!(res.fields.len(), 3);

    let mut c = store.collection("users").unwrap();
    let got = c.get("u1").unwrap().unwrap();
    assert_eq!(got.field("age"), Some(&Value::Int(31)));
    assert_eq!(got.field("name"), Some(&Value::String("alice".into())));
    assert_eq!(got.field("city"), Some(&Value::String("berlin".into())));
    assert_eq!(got.fields.len(), 3);
}

// ---------------------------------------------------------------------------
// Invariante: CAS-Miss → exakt unveränderter Zustand
// ---------------------------------------------------------------------------
#[test]
fn cas_miss_unchanged() {
    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("alice".into())), ("age", Value::Int(30))])).unwrap();
    drop(coll);

    let mut wrong = entity(&[("name", Value::String("alice".into())), ("age", Value::Int(30))]);
    wrong.fields.iter_mut().find(|(n, _)| n == "age").unwrap().1 = Value::Int(99);
    let err = store
        .cas_update("users", "u1", &Expected::Entity(wrong), &[Patch::Set("age".into(), Value::Int(31))])
        .unwrap_err();
    assert!(matches!(err, Error::Conflict { reason: ConflictReason::ExpectedValueMismatch, .. }));

    let mut c = store.collection("users").unwrap();
    let got = c.get("u1").unwrap().unwrap();
    assert_eq!(got.field("age"), Some(&Value::Int(30)));
    assert_eq!(got.field("name"), Some(&Value::String("alice".into())));
}

// ---------------------------------------------------------------------------
// Invariante: Absent → nur wenn tatsächlich fehlend
// ---------------------------------------------------------------------------
#[test]
fn absent_only_when_missing() {
    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("alice".into()))])).unwrap();
    drop(coll);

    let err = store
        .cas_update("users", "u1", &Expected::Absent, &[Patch::Set("x".into(), Value::Int(1))])
        .unwrap_err();
    assert!(matches!(err, Error::Conflict { reason: ConflictReason::ExpectedAbsentButExists, .. }));

    let res = store
        .cas_update("users", "u2", &Expected::Absent, &[Patch::Set("name".into(), Value::String("bob".into()))])
        .unwrap();
    assert_eq!(res.field("name"), Some(&Value::String("bob".into())));

    let mut c = store.collection("users").unwrap();
    assert!(c.get("u1").unwrap().is_some());
    assert!(c.get("u2").unwrap().is_some());
}

// ---------------------------------------------------------------------------
// Invariante: Null ≠ Absent (present-null vs. entfernt)
// ---------------------------------------------------------------------------
#[test]
fn null_distinct_from_absent() {
    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("alice".into()))])).unwrap();
    drop(coll);

    // Set auf Null → present, aber null.
    store
        .cas_update("users", "u1", &Expected::Any, &[Patch::Set("nick".into(), Value::Null)])
        .unwrap();
    let mut c = store.collection("users").unwrap();
    let g = c.get("u1").unwrap().unwrap();
    assert_eq!(g.field("nick"), Some(&Value::Null));
    drop(c);

    // Remove → danach absent (nicht Null). Erwartung muss den Null-Zustand matchen.
    let cur = entity(&[("name", Value::String("alice".into())), ("nick", Value::Null)]);
    store
        .cas_update("users", "u1", &Expected::Entity(cur), &[Patch::Remove("nick".into())])
        .unwrap();
    let mut c = store.collection("users").unwrap();
    let g2 = c.get("u1").unwrap().unwrap();
    assert_eq!(g2.field("nick"), None);
    assert_eq!(g2.fields.len(), 1);
}

// ---------------------------------------------------------------------------
// Invariante: Increment → nur numerische Werte (Int/Float), sonst InvalidArgument
// ---------------------------------------------------------------------------
#[test]
fn increment_only_numeric() {
    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put(
        "u1",
        &entity(&[
            ("age", Value::Int(10)),
            ("score", Value::Float(1.5)),
            ("name", Value::String("alice".into())),
        ]),
    )
    .unwrap();
    drop(coll);

    let r = store.cas_update("users", "u1", &Expected::Any, &[Patch::Increment("age".into(), Value::Int(5))]).unwrap();
    assert_eq!(r.field("age"), Some(&Value::Int(15)));

    let r2 = store.cas_update("users", "u1", &Expected::Any, &[Patch::Increment("score".into(), Value::Float(0.5))]).unwrap();
    assert_eq!(r2.field("score"), Some(&Value::Float(2.0)));

    // Int-Überlauf wrappt (wrapping_add) — kein Panic.
    let r3 = store.cas_update("users", "u1", &Expected::Any, &[Patch::Increment("age".into(), Value::Int(i64::MAX))]).unwrap();
    assert_eq!(r3.field("age"), Some(&Value::Int(15i64.wrapping_add(i64::MAX))));

    assert!(matches!(
        store.cas_update("users", "u1", &Expected::Any, &[Patch::Increment("name".into(), Value::Int(1))]).unwrap_err(),
        Error::InvalidArgument(_)
    ));
    assert!(matches!(
        store.cas_update("users", "u1", &Expected::Any, &[Patch::Increment("missing".into(), Value::Int(1))]).unwrap_err(),
        Error::InvalidArgument(_)
    ));
    assert!(matches!(
        store.cas_update("users", "u1", &Expected::Any, &[Patch::Increment("age".into(), Value::Float(1.0))]).unwrap_err(),
        Error::InvalidArgument(_)
    ));
}

// ---------------------------------------------------------------------------
// Invariante: Remove → Feld tatsächlich entfernt
// ---------------------------------------------------------------------------
#[test]
fn remove_field_removed() {
    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("alice".into())), ("age", Value::Int(30))])).unwrap();
    drop(coll);

    store.cas_update("users", "u1", &Expected::Any, &[Patch::Remove("age".into())]).unwrap();
    let mut c = store.collection("users").unwrap();
    let g = c.get("u1").unwrap().unwrap();
    assert_eq!(g.field("age"), None);
    assert_eq!(g.field("name"), Some(&Value::String("alice".into())));
    assert_eq!(g.fields.len(), 1);
}

// ---------------------------------------------------------------------------
// Invariante: Index-Feld bleibt nach CAS korrekt (Set + Increment)
// ---------------------------------------------------------------------------
#[test]
fn index_field_stays_correct() {
    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("alice".into())), ("age", Value::Int(30))])).unwrap();
    coll.create_index("age").unwrap();
    drop(coll);

    store.cas_update("users", "u1", &Expected::Any, &[Patch::Set("age".into(), Value::Int(31))]).unwrap();
    let mut c = store.collection("users").unwrap();
    let old = c.find("age", FindOp::Eq(Value::Int(30))).unwrap();
    assert!(!old.contains(&"u1".to_string()), "alter Index-Eintrag muss weg sein: {old:?}");
    let new = c.find("age", FindOp::Eq(Value::Int(31))).unwrap();
    assert_eq!(new, vec!["u1".to_string()]);
    drop(c);

    store.cas_update("users", "u1", &Expected::Any, &[Patch::Increment("age".into(), Value::Int(1))]).unwrap();
    let mut c = store.collection("users").unwrap();
    let inc = c.find("age", FindOp::Eq(Value::Int(32))).unwrap();
    assert_eq!(inc, vec!["u1".to_string()]);
}

// ---------------------------------------------------------------------------
// Invariante: Direct ≡ Tx → identisches Ergebnis
// ---------------------------------------------------------------------------
#[test]
fn direct_equiv_tx() {
    let ops = vec![
        (Expected::Any, vec![Patch::Increment("age".into(), Value::Int(4)), Patch::Set("name".into(), Value::String("a2".into()))]),
        (Expected::Field("name".into(), Value::String("a2".into())), vec![Patch::Remove("name".into())]),
    ];

    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("a".into())), ("age", Value::Int(1))])).unwrap();
    drop(coll);
    for (exp, patches) in &ops {
        store.cas_update("users", "u1", exp, patches).unwrap();
    }
    let direct = {
        let mut c = store.collection("users").unwrap();
        c.get("u1").unwrap().unwrap()
    };

    let dir2 = tempdir().unwrap();
    let mut store2 = EntityStore::open_with(dir2.path(), opts()).unwrap();
    let mut coll = store2.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("a".into())), ("age", Value::Int(1))])).unwrap();
    drop(coll);
    let mut tx = store2.transaction().unwrap();
    for (exp, patches) in &ops {
        tx.cas_update("users", "u1", exp, patches).unwrap();
    }
    tx.commit().unwrap();
    drop(tx);
    let txr = {
        let mut c = store2.collection("users").unwrap();
        c.get("u1").unwrap().unwrap()
    };

    assert_eq!(direct, txr, "Direct- und Transaction-CAS müssen identische Entität liefern");
}

// ---------------------------------------------------------------------------
// Invariante: Expected::Field Match/Mismatch
// ---------------------------------------------------------------------------
#[test]
fn expected_field_mismatch() {
    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("a".into())), ("age", Value::Int(30))])).unwrap();
    drop(coll);

    let err = store
        .cas_update("users", "u1", &Expected::Field("age".into(), Value::Int(31)), &[Patch::Set("age".into(), Value::Int(31))])
        .unwrap_err();
    assert!(matches!(err, Error::Conflict { reason: ConflictReason::ExpectedFieldMismatch, .. }));

    store
        .cas_update("users", "u1", &Expected::Field("age".into(), Value::Int(30)), &[Patch::Set("age".into(), Value::Int(31))])
        .unwrap();
}

// ---------------------------------------------------------------------------
// Invariante: Crash vor/bei Commit → kein halber Zustand (WAL-Replay)
// ---------------------------------------------------------------------------
#[test]
fn crash_commit_atomic() {
    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("a".into()))])).unwrap();
    drop(coll);

    let mut tx = store.transaction().unwrap();
    tx.cas_update(
        "users",
        "u1",
        &Expected::Any,
        &[
            Patch::Set("f1".into(), Value::Int(1)),
            Patch::Set("f2".into(), Value::Int(2)),
            Patch::Set("f3".into(), Value::Int(3)),
        ],
    )
    .unwrap();
    tx.commit().unwrap();
    drop(tx);
    // Simulierter Crash: Drop ohne explizites close/flush (Commit fsync't WAL).
    drop(store);

    let mut store2 = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut c = store2.collection("users").unwrap();
    let g = c.get("u1").unwrap().unwrap();
    assert_eq!(g.field("f1"), Some(&Value::Int(1)));
    assert_eq!(g.field("f2"), Some(&Value::Int(2)));
    assert_eq!(g.field("f3"), Some(&Value::Int(3)));
    assert_eq!(g.field("name"), Some(&Value::String("a".into())));
    assert_eq!(g.fields.len(), 4);
}

// ---------------------------------------------------------------------------
// Invariante: CAS-Miss innerhalb einer Transaktion verfälscht keine anderen
// (erfolgreichen) Mutationen und bricht den Commit nicht ab.
// ---------------------------------------------------------------------------
#[test]
fn cas_conflict_in_tx_no_partial() {
    let dir = tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts()).unwrap();
    let mut coll = store.collection("users").unwrap();
    coll.put("u1", &entity(&[("name", Value::String("a".into())), ("age", Value::Int(30))])).unwrap();
    coll.put("u2", &entity(&[("name", Value::String("b".into()))])).unwrap();
    drop(coll);

    let mut tx = store.transaction().unwrap();
    tx.cas_update("users", "u2", &Expected::Any, &[Patch::Set("name".into(), Value::String("b2".into()))]).unwrap();
    let mut wrong = entity(&[("name", Value::String("a".into())), ("age", Value::Int(30))]);
    wrong.fields.iter_mut().find(|(n, _)| n == "age").unwrap().1 = Value::Int(99);
    let err = tx.cas_update("users", "u1", &Expected::Entity(wrong), &[Patch::Set("age".into(), Value::Int(31))]).unwrap_err();
    assert!(matches!(err, Error::Conflict { .. }));
    tx.commit().unwrap();
    drop(tx);

    let mut c = store.collection("users").unwrap();
    let u1 = c.get("u1").unwrap().unwrap();
    assert_eq!(u1.field("age"), Some(&Value::Int(30)));
    let u2 = c.get("u2").unwrap().unwrap();
    assert_eq!(u2.field("name"), Some(&Value::String("b2".into())));
}
