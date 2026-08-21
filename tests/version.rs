//! v0.9 Format-Versionierung: Regression der Fehlerklassen.
//!
//! Trennung der Fehlerklassen:
//!   unbekannte / neue Version            -> UnsupportedFormatVersion
//!   kaputter Versionsmarker             -> InvalidFormat
//!   kaputte Daten bei gültiger Version  -> Corrupt

use std::fs;

use my_lsm_db::Database;
use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::error::Error;
use my_lsm_db::version::FORMAT_VERSION;

/// Erzeugt eine vollständig auf Platte liegende Datenbank (MANIFEST, SSTable,
/// SCHEMA, VERSION) mit einer Entität.
fn make_db(dir: &std::path::Path) {
    let mut store = EntityStore::open(dir).unwrap();
    {
        let mut coll = store.collection("users").unwrap();
        let mut e = Entity::new();
        e.insert("name", Value::String("alice".into()));
        coll.put("u1", &e).unwrap();
    }
    store.flush().unwrap();
    store.close().unwrap();
}

#[test]
fn legacy_db_without_version_opens_as_v1() {
    let dir = tempfile::tempdir().unwrap();
    make_db(dir.path());
    // Simuliere eine vor-v0.9 (Legacy-)Datenbank: VERSION entfernen.
    fs::remove_file(dir.path().join("VERSION")).unwrap();
    assert!(dir.path().join("MANIFEST").exists());

    let db = Database::open(dir.path()).unwrap();
    assert_eq!(db.format_version(), 1);
}

#[test]
fn current_db_with_v1_opens() {
    let dir = tempfile::tempdir().unwrap();
    make_db(dir.path());
    assert!(dir.path().join("VERSION").exists());

    let db = Database::open(dir.path()).unwrap();
    assert_eq!(db.format_version(), FORMAT_VERSION);
}

#[test]
fn explicit_version_equal_current_opens() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("VERSION"), "V 1\n").unwrap();
    // MANIFEST vorhanden, damit nicht als frische DB neu geschrieben wird.
    fs::write(dir.path().join("MANIFEST"), "").unwrap();

    let db = Database::open(dir.path()).unwrap();
    assert_eq!(db.format_version(), 1);
}

#[test]
fn version_greater_than_current_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("VERSION"), "V 2\n").unwrap();

    let res = Database::open(dir.path());
    assert!(matches!(
        res,
        Err(Error::UnsupportedFormatVersion { found: 2, .. })
    ));
}

#[test]
fn invalid_version_marker_is_invalid_format() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("VERSION"), "garbage\n").unwrap();

    let res = Database::open(dir.path());
    assert!(matches!(res, Err(Error::InvalidFormat(_))));
}

#[test]
fn version_greater_than_current_is_not_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("VERSION"), "V 2\n").unwrap();
    // Die zentrale Invariante: eine *neuere* Datenbank darf NICHT als
    // Korruption (oder gar stilles altes Format) interpretiert werden.
    let res = Database::open(dir.path());
    assert!(!matches!(res, Err(Error::Corrupt(_))));
    assert!(matches!(res, Err(Error::UnsupportedFormatVersion { .. })));
}

#[test]
fn corrupt_data_with_valid_version_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    make_db(dir.path());
    // VERSION bleibt gültig (v1); wir zerstören die SSTables.
    let ssts: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |x| x == "sst"))
        .collect();
    assert!(!ssts.is_empty(), "erwartete mindestens eine SSTable");
    for p in ssts {
        fs::remove_file(p).unwrap();
    }

    let res = Database::open(dir.path());
    assert!(matches!(res, Err(Error::Corrupt(_))));
}
