//! Regression fuer F.3/F.5: ein korrupter ID-Token in einer `L`-Zeile des
//! MANIFEST darf NICHT still entfernt werden (silent data loss), sondern muss
//! bereits beim Laden mit `Error::InvalidFormat` scheitern.

use std::path::Path;

use my_lsm_db::Database;
use my_lsm_db::error::Error;
use my_lsm_db::manifest::Manifest;

fn manifest_path(dir: &Path) -> std::path::PathBuf {
    dir.join("MANIFEST")
}

/// Gueltiges Manifest mit L0-Tabellen 1 und 2; ein ID-Token korrupt ("x5").
fn corrupt_manifest() -> String {
    "N 10\nL 0 1 x5\nS 1 3 61 6d 7\nS 1 5 6e 7a 9\n".to_string()
}

#[test]
fn corrupt_l_id_rejected_on_load() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(manifest_path(dir.path()), corrupt_manifest()).unwrap();
    let res = Manifest::load(&manifest_path(dir.path()));
    assert!(
        matches!(res, Err(Error::InvalidFormat(_))),
        "korrupte L-ID muss InvalidFormat liefern, got {:?}",
        res.err()
    );
}

#[test]
fn corrupt_l_id_rejected_on_open() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(manifest_path(dir.path()), corrupt_manifest()).unwrap();
    // open() muss lautstark scheitern. Da kein gueltiges Database-Handle
    // entsteht, ist `db.gc()` in diesem Zustand strukturell nicht aufrufbar
    // (kein stiller, permanenter Verlust der live-Tabelle durch GC moeglich).
    let res = Database::open(dir.path());
    assert!(
        matches!(res, Err(Error::InvalidFormat(_))),
        "open() bei korrupter L-ID muss InvalidFormat liefern, got {:?}",
        res.err()
    );
}
