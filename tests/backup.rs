//! v1.1 Backup/Restore Regression-Fixtures (Phase H, Zweig A).
//!
//! Deckt den Spezifikationsvertrag ab: konsistenter Backup-Root, keine
//! Orphans/`.wal`/`*.tmp`, kein stilles Überschreiben, Versionsprüfung,
//! Wiederherstellbarkeit inkl. Pending-Writes und Post-Restore-Mutation.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::error::Error as DbError;
use my_lsm_db::index::FindOp;
use my_lsm_db::Database;
use my_lsm_db::Options;

/// Pfad zum gebauten `lsm-admin`-Binary (wie in tests/cli.rs).
fn bin() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let profile_dir = exe
        .parent()
        .and_then(|p| p.parent())
        .expect("profile dir from current_exe");
    let name = if cfg!(windows) { "lsm-admin.exe" } else { "lsm-admin" };
    profile_dir.join(name)
}

/// Erzeugt eine DB mit mehreren Levels/Tabellen (L0 + L1-Segmente).
fn make_data_db(dir: &Path) {
    let opts = Options {
        memtable_limit: 256,
        l0_compact_threshold: 2,
        segment_max_records: 2,
    };
    let mut store = EntityStore::open_with(dir, opts).unwrap();
    for round in 0..3u32 {
        let mut coll = store.collection("users").unwrap();
        for i in 0..2u32 {
            let mut e = Entity::new();
            e.insert("name", Value::String(format!("u-{round}-{i}")));
            e.insert("age", Value::Int((round * 10 + i) as i64));
            coll.put(&format!("e{round}_{i}"), &e).unwrap();
        }
        drop(coll);
        store.flush().unwrap();
    }
    store.close().unwrap();
}

/// Erzeugt eine DB mit Secondary-Index auf `age` (bestehende API:
/// `CollectionHandle::create_index`).
fn make_indexed_db(dir: &Path) {
    let opts = Options {
        memtable_limit: 256,
        l0_compact_threshold: 2,
        segment_max_records: 2,
    };
    let mut store = EntityStore::open_with(dir, opts).unwrap();
    let mut coll = store.collection("users").unwrap();
    for i in 0..6u32 {
        let mut e = Entity::new();
        e.insert("name", Value::String(format!("u{i}")));
        e.insert("age", Value::Int(i as i64));
        coll.put(&format!("e{i}"), &e).unwrap();
    }
    coll.create_index("age").unwrap();
    drop(coll);
    store.flush().unwrap();
    store.close().unwrap();
}

fn collect(dir: &Path) -> BTreeMap<String, Entity> {
    let mut store = EntityStore::open(dir).unwrap();
    store.scan_collection("users").unwrap().into_iter().collect()
}

fn index_find(dir: &Path, field: &str, op: FindOp) -> Vec<String> {
    let mut store = EntityStore::open(dir).unwrap();
    let mut coll = store.collection("users").unwrap();
    let mut v = coll.find(field, op).unwrap();
    v.sort();
    v
}

fn list_files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    v.sort();
    v
}

/// Backup über die Bibliothek: öffnet die DB exklusiv (mut), flushes
/// pending Writes und kopiert den Backup-Root.
fn backup(src: &Path, bak: &Path) {
    let mut db = Database::open(src).unwrap();
    db.backup(bak).unwrap();
    db.close().unwrap();
}

// 1. Backup einer leeren DB -> Restore öffnet.
#[test]
fn backup_empty_db_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let dest = tmp.path().join("dest");

    EntityStore::open(&src).unwrap().close().unwrap();

    backup(&src, &bak);
    Database::restore(&bak, &dest).unwrap();

    // Wieder öffnen muss gelingen.
    EntityStore::open(&dest).unwrap().close().unwrap();
}

// 2. Backup mit KV-/Entity-Daten -> alle Daten identisch.
#[test]
fn backup_with_data_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let dest = tmp.path().join("dest");

    make_data_db(&src);
    let before = collect(&src);

    backup(&src, &bak);
    Database::restore(&bak, &dest).unwrap();

    let after = collect(&dest);
    assert_eq!(before, after);
}

// 3. Backup mit Entities + Index -> Index-Queries identisch.
#[test]
fn backup_with_index_queries_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let dest = tmp.path().join("dest");

    make_indexed_db(&src);
    let before = index_find(&src, "age", FindOp::Gte(Value::Int(0)));

    backup(&src, &bak);
    Database::restore(&bak, &dest).unwrap();

    let after = index_find(&dest, "age", FindOp::Gte(Value::Int(0)));
    assert_eq!(before, after);
}

// 4. Backup nach Compaction -> identisch.
#[test]
fn backup_after_compaction_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let dest = tmp.path().join("dest");

    make_data_db(&src);
    {
        let mut db = Database::open(&src).unwrap();
        db.compact_full().unwrap();
        db.close().unwrap();
    }
    let before = collect(&src);

    backup(&src, &bak);
    Database::restore(&bak, &dest).unwrap();

    let after = collect(&dest);
    assert_eq!(before, after);
}

// 5. Backup mit Pending Direct-Writes -> backup macht sie persistent.
#[test]
fn backup_flushes_pending_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let dest = tmp.path().join("dest");

    let mut store = EntityStore::open(&src).unwrap();
    // Schreiben, ABER nicht flushen/schließen (Pending im MemTable).
    let mut coll = store.collection("users").unwrap();
    for i in 0..3u32 {
        let mut e = Entity::new();
        e.insert("name", Value::String(format!("pending-{i}")));
        e.insert("age", Value::Int(i as i64));
        coll.put(&format!("e{i}"), &e).unwrap();
    }
    drop(coll);
    // Backup über die EntityStore-API (delegiert, flushes intern).
    store.backup(&bak).unwrap();
    store.close().unwrap();

    Database::restore(&bak, &dest).unwrap();
    let after = collect(&dest);
    assert_eq!(after.len(), 3);
    for i in 0..3u32 {
        let e = after.get(&format!("e{i}")).expect("pending entity present");
        assert_eq!(e.field("name"), Some(&Value::String(format!("pending-{i}"))));
    }
}

// 6. Backup enthält keine Orphans -> Restore bleibt sauber.
#[test]
fn backup_excludes_orphans() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let dest = tmp.path().join("dest");

    make_data_db(&src);
    let before = collect(&src);

    // Orphan-Datei im Quellverzeichnis ablegen (nicht im Manifest referenziert).
    fs::write(src.join("123456.sst"), b"not a real sstable").unwrap();

    backup(&src, &bak);
    // Backup-Root darf den Orphan nicht enthalten.
    assert!(!list_files(&bak).contains(&"123456.sst".to_string()));

    Database::restore(&bak, &dest).unwrap();
    // Ziel enthält ebenfalls keinen Orphan.
    assert!(!list_files(&dest).contains(&"123456.sst".to_string()));

    // Wiederhergestellte DB ist sauber und vollständig.
    let after = collect(&dest);
    assert_eq!(before, after);
}

// 7. Restore in existierenden Zielpfad -> definierter Fehler.
#[test]
fn restore_into_existing_target_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let existing = tmp.path().join("existing");

    make_data_db(&src);
    backup(&src, &bak);

    // Eine bestehende (nicht leere) Ziel-DB anlegen.
    make_data_db(&existing);

    let res = Database::restore(&bak, &existing);
    assert!(res.is_err(), "restore darf nicht stillschweigend überschreiben");
}

// 8. Beschädigtes/unvollständiges Backup -> open() scheitert.
#[test]
fn restore_corrupted_backup_fails_open() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let dest = tmp.path().join("dest");

    make_data_db(&src);
    backup(&src, &bak);

    // Eine referenzierte SSTable aus dem Backup entfernen (unvollständig).
    let orphan = list_files(&bak)
        .into_iter()
        .find(|f| f.ends_with(".sst"))
        .expect("backup contains at least one sst");
    fs::remove_file(bak.join(&orphan)).unwrap();

    // Restore kopiert, was vorhanden ist; die wiederhergestellte DB muss aber
    // unbrauchbar sein (kein halb gültiger Restore): open() scheitert bereits
    // (validate_open_state erkennt die fehlende Segment-Datei).
    Database::restore(&bak, &dest).unwrap();
    let open_res = EntityStore::open(&dest);
    assert!(
        matches!(open_res, Err(DbError::Corrupt(_))),
        "open() einer unvollständigen DB muss scheitern (kein halb gültiger Restore)"
    );
}

// 9. Inkompatible VERSION im Backup -> UnsupportedFormatVersion.
#[test]
fn restore_incompatible_version_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let dest = tmp.path().join("dest");

    make_data_db(&src);
    backup(&src, &bak);

    // VERSION im Backup auf eine unvereinbare Version zwingen.
    fs::write(bak.join("VERSION"), "V 2\n").unwrap();

    let res = Database::restore(&bak, &dest);
    assert!(
        matches!(res, Err(DbError::UnsupportedFormatVersion { .. })),
        "erwartet UnsupportedFormatVersion, got {res:?}"
    );
}

// 10. Restore -> neue DB -> Write/Flush/Compact voll funktionsfähig.
#[test]
fn restore_then_mutate_full_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let dest = tmp.path().join("dest");

    make_data_db(&src);
    let before = collect(&src);

    backup(&src, &bak);
    Database::restore(&bak, &dest).unwrap();

    // Neue DB öffnen, weiteren Write + Flush + Compact.
    let mut store = EntityStore::open(&dest).unwrap();
    let mut coll = store.collection("users").unwrap();
    let mut e = Entity::new();
    e.insert("name", Value::String("newborn".into()));
    e.insert("age", Value::Int(99));
    coll.put("newborn", &e).unwrap();
    drop(coll);
    store.flush().unwrap();
    store.close().unwrap();

    {
        let mut db = Database::open(&dest).unwrap();
        db.compact_full().unwrap();
        db.close().unwrap();
    }

    // Wiedereröffnen: alle Originaldaten + neuer Eintrag vorhanden.
    let mut store = EntityStore::open(&dest).unwrap();
    let after = store.scan_collection("users").unwrap();
    store.close().unwrap();

    let mut merged: BTreeMap<String, Entity> = before;
    merged.insert("newborn".to_string(), e);
    let after_map: BTreeMap<String, Entity> = after.into_iter().collect();
    assert_eq!(merged, after_map);
}

// CLI-Roundtrip via `lsm-admin backup` / `restore`.
#[test]
fn cli_backup_restore_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let dest = tmp.path().join("dest");

    make_data_db(&src);
    let before = collect(&src);

    let s1 = std::process::Command::new(bin())
        .arg("backup")
        .arg("--dir")
        .arg(&src)
        .arg(&bak)
        .status()
        .expect("run backup");
    assert!(s1.success());

    let s2 = std::process::Command::new(bin())
        .arg("restore")
        .arg(&bak)
        .arg(&dest)
        .status()
        .expect("run restore");
    assert!(s2.success());

    assert_eq!(before, collect(&dest));
}

// CLI: Restore in bestehende Ziel-DB -> Exit 1 (kein stilles Überschreiben).
#[test]
fn cli_restore_existing_target_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let bak = tmp.path().join("bak");
    let existing = tmp.path().join("existing");

    make_data_db(&src);
    let s1 = std::process::Command::new(bin())
        .arg("backup")
        .arg("--dir")
        .arg(&src)
        .arg(&bak)
        .status()
        .expect("run backup");
    assert!(s1.success());
    make_data_db(&existing);

    let s2 = std::process::Command::new(bin())
        .arg("restore")
        .arg(&bak)
        .arg(&existing)
        .status()
        .expect("run restore");
    assert_eq!(s2.code(), Some(1));
}
