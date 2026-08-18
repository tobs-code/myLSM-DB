//! v0.2-Smoke-Tests: der Entity-Layer über der dummen KV-Engine.

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};

fn user() -> Entity {
    let mut e = Entity::new();
    e.insert("name", Value::String("Tobias".into()));
    e.insert("age", Value::Int(31));
    e.insert("active", Value::Bool(true));
    e
}

#[test]
fn smoke_put_get() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();

    let u = user();
    store.collection("users").unwrap().put("123", &u).unwrap();

    let got = store
        .collection("users")
        .unwrap()
        .get("123")
        .unwrap()
        .expect("user exists");
    assert_eq!(got["name"], Value::String("Tobias".into()));
    assert_eq!(got["age"], Value::Int(31));
    assert_eq!(got["active"], Value::Bool(true));
}

#[test]
fn smoke_survives_flush_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = EntityStore::open(dir.path()).unwrap();
        store
            .collection("users")
            .unwrap()
            .put("123", &user())
            .unwrap();
        // close() flusht die MemTable in eine SSTable + persistiert das Schema.
        store.close().unwrap();
    }
    // Neu öffnen: Feld-IDs und Daten müssen identisch lesbar sein.
    let mut store = EntityStore::open(dir.path()).unwrap();
    let got = store
        .collection("users")
        .unwrap()
        .get("123")
        .unwrap()
        .expect("user survives reopen");
    assert_eq!(got["name"], Value::String("Tobias".into()));
    assert_eq!(got["age"], Value::Int(31));
}
