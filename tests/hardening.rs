//! v0.5.2-Regressionstests: API-/Semantik-Härtung.
//!
//! 1. Nicht-UTF-8-Entity-ID beim Schreiben -> `InvalidArgument`.
//! 2. Im Speicher korrupte (nicht-UTF-8) Entity-ID -> `InvalidFormat`.
//! 3. Lese-Operationen auf unbekannter Collection sind leer und mutieren das
//!    Schema NICHT (kein `SCHEMA`-File, keine Einträge).
//! 4. Persistenz-Korruptionsfälle bleiben im `InvalidFormat`-Bereich.

use std::path::Path;

use my_lsm_db::codec::{self, Value};
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::error::Error;
use my_lsm_db::index::FindOp;
use my_lsm_db::{Database, keycodec};

fn user() -> Entity {
    let mut e = Entity::new();
    e.insert("name", Value::String("Tobias".into()));
    e
}

fn schema_path(dir: &Path) -> std::path::PathBuf {
    dir.join("SCHEMA")
}

fn is_invalid_format<T>(r: Result<T, Error>) -> bool {
    matches!(r, Err(Error::InvalidFormat(_)))
}

fn is_invalid_argument<T>(r: Result<T, Error>) -> bool {
    matches!(r, Err(Error::InvalidArgument(_)))
}

#[test]
fn write_rejects_non_utf8_entity_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let bad: &[u8] = &[0xff, 0xfe];

    assert!(is_invalid_argument(store.put_entity(0, bad, &user())));
    assert!(is_invalid_argument(store.delete_entity(0, bad)));
}

#[test]
fn corrupt_stored_id_is_invalid_format() {
    let dir = tempfile::tempdir().unwrap();
    {
        // Collection "users" -> cid 0, Feld "name" -> fid 0 anlegen + persistieren.
        let mut store = EntityStore::open(dir.path()).unwrap();
        store
            .collection("users")
            .unwrap()
            .put("1", &user())
            .unwrap();
        store.close().unwrap();
    }
    {
        // Korrupten Key direkt in die KV-Engine injizieren (cid 0, fid 0,
        // nicht-UTF-8-Entity-ID) — wie ein korrupter/althergebrachter Write.
        let mut db = Database::open(dir.path()).unwrap();
        let key = keycodec::encode_entity_key(0, &[0xff, 0xff], 0);
        db.put(&key, &codec::encode(&Value::String("x".into())))
            .unwrap();
        db.flush().unwrap();
        db.close().unwrap();
    }
    let mut store = EntityStore::open(dir.path()).unwrap();
    match store.scan_collection("users") {
        Ok(_) => panic!("corrupt id must surface as InvalidFormat, got Ok"),
        Err(Error::InvalidFormat(_)) => {}
        Err(other) => panic!("corrupt id must be InvalidFormat, got {other:?}"),
    }
}

#[test]
fn reads_on_unknown_collection_are_empty_and_do_not_mutate_schema() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let sp = schema_path(dir.path());

    assert!(store.scan_collection("ghost").unwrap().is_empty());
    assert!(!sp.exists(), "a read must not create/persist a SCHEMA file");

    let mut tx = store.transaction().unwrap();
    assert!(tx.get("ghost", "x").unwrap().is_none());
    assert!(tx.scan_collection("ghost").unwrap().is_empty());
    assert!(
        tx.find("ghost", "name", FindOp::Eq(Value::String("Tobias".into())))
            .unwrap()
            .is_empty()
    );
    tx.commit().unwrap();

    assert!(
        !sp.exists(),
        "transaction reads must not create/persist a SCHEMA file"
    );
}

#[test]
fn reads_on_unknown_collection_leave_existing_schema_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    store
        .collection("users")
        .unwrap()
        .put("1", &user())
        .unwrap();
    store.close().unwrap();
    let sp = schema_path(dir.path());
    let before = std::fs::read(&sp).unwrap();

    let mut store = EntityStore::open(dir.path()).unwrap();
    assert!(store.scan_collection("ghost").unwrap().is_empty());
    let mut tx = store.transaction().unwrap();
    assert!(tx.get("ghost", "x").unwrap().is_none());
    tx.commit().unwrap();

    let after = std::fs::read(&sp).unwrap();
    assert_eq!(before, after, "schema bytes must not change after reads");
}

#[test]
fn persistence_corruption_remains_invalid_format() {
    // Direkte Value-Encoding-Korruption -> InvalidFormat (nicht InvalidArgument).
    assert!(is_invalid_format(codec::decode(&[0x7f])));
}
