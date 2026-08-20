//! v1.0 CLI-Regression: inspect / stats / compact / gc.
//!
//! Nutzt die von Cargo bereitgestellte Binär-Umgebungsvariable, sodass der
//! `lsm-admin`-Build direkt ausgeführt wird.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::Options;

/// Pfad zum gebauten `lsm-admin`-Binary. Cargo setzt
/// `CARGO_BIN_EXE_LSM_ADMIN` nur unzuverlässig; wir leiten ihn aus der
/// Position der Test-Exe ab (`target/<profile>/deps/<test>` -> `<profile>`).
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
fn make_db(dir: &Path) {
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

fn collect(dir: &Path) -> BTreeMap<String, Entity> {
    let mut store = EntityStore::open(dir).unwrap();
    store.scan_collection("users").unwrap().into_iter().collect()
}

/// (Name, Größe) aller Dateien im Verzeichnis, sortiert — zum
/// Mutations-Nachweis für rein lesende Befehle.
fn file_state(dir: &Path) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = Vec::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        if let Ok(meta) = e.metadata() {
            if meta.is_file() {
                v.push((e.file_name().to_string_lossy().into_owned(), meta.len()));
            }
        }
    }
    v.sort();
    v
}

#[test]
fn cli_inspect_multi_level() {
    let dir = tempfile::tempdir().unwrap();
    make_db(dir.path());

    let out = Command::new(bin())
        .args(["inspect", "--dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(s.contains("format-version: 1"), "got:\n{s}");
    assert!(s.contains("next-table-id:"), "got:\n{s}");
    // Mehrere Levels/Tabellen: sowohl L0 als auch L1 müssen erscheinen.
    assert!(s.contains("[L0]"), "L0 erwartet:\n{s}");
    assert!(s.contains("[L1]"), "L1 erwartet:\n{s}");
    // Record-Counts pro Tabelle vorhanden.
    assert!(s.contains("records="), "records erwartet:\n{s}");
}

#[test]
fn cli_stats_counts() {
    let dir = tempfile::tempdir().unwrap();
    make_db(dir.path());

    let out = Command::new(bin())
        .args(["stats", "--dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(s.contains("db-size:"), "got:\n{s}");
    assert!(s.contains("sstable-count:"), "got:\n{s}");
    assert!(s.contains("L0:"), "got:\n{s}");
    assert!(s.contains("L1:"), "got:\n{s}");
    assert!(s.contains("wal-size:"), "got:\n{s}");
    assert!(s.contains("options:"), "got:\n{s}");
}

#[test]
fn cli_compact_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    make_db(dir.path());

    let before = collect(dir.path());
    assert_eq!(before.len(), 6, "6 Entities erwartet");

    let out = Command::new(bin())
        .args(["compact", "--dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("compacted:"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let after = collect(dir.path());
    // Inhalt muss identisch sein (kein Datenverlust durch Compaction).
    assert_eq!(after.len(), before.len());
    assert_eq!(after, before);
}

#[test]
fn cli_gc_removes_orphans() {
    let dir = tempfile::tempdir().unwrap();
    make_db(dir.path());

    // Absichtlich einen nicht referenzierten (Orphan-)SSTable erzeugen.
    fs::write(dir.path().join("999999.sst"), b"not a real sstable").unwrap();
    assert!(dir.path().join("999999.sst").exists());

    let out = Command::new(bin())
        .args(["gc", "--dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("removed 1"), "got: {s}");
    assert!(!dir.path().join("999999.sst").exists(), "orphan should be gone");

    // Referenzierte Daten bleiben intakt.
    let entities = collect(dir.path());
    assert_eq!(entities.len(), 6);
}

#[test]
fn cli_inspect_does_not_mutate() {
    let dir = tempfile::tempdir().unwrap();
    make_db(dir.path());
    let before = file_state(dir.path());

    let out = Command::new(bin())
        .args(["inspect", "--dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());

    let after = file_state(dir.path());
    assert_eq!(after, before, "inspect darf keine Dateien verändern");
}

#[test]
fn cli_error_incompatible_version() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("VERSION"), "V 2\n").unwrap();

    let out = Command::new(bin())
        .args(["inspect", "--dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success(), "incompatible version must fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("unsupported format version"),
        "got stderr: {err}"
    );
}

#[test]
fn cli_error_path_is_file() {
    let dir = tempfile::tempdir().unwrap();
    let as_file = dir.path().join("not-a-dir");
    fs::write(&as_file, b"x").unwrap();

    let out = Command::new(bin())
        .args(["inspect", "--dir", as_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success(), "opening a file as dir must fail");
    assert!(!out.stderr.is_empty());
}
