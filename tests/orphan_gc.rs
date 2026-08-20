//! Regressionstests fuer `db.gc()` (Orphan-GC, E.10 / §30.10).
//!
//! Keine automatische Bereinigung beim Open; `gc()` ist ein expliziter,
//! exklusiver Maintenance-Schritt. Alle Tests nutzen das oeffentliche API.

use std::path::Path;
use tempfile::TempDir;

use my_lsm_db::{Database, Options};

/// Oeffnet eine DB mit kleinen Schwellen, damit Compaction leicht ausloest.
fn open(dir: &Path) -> Database {
    Database::open_with(
        dir,
        Options {
            memtable_limit: 64 * 1024,
            l0_compact_threshold: 2,
            ..Options::default()
        },
    )
    .expect("open")
}

/// Baut eine DB mit mindestens einem referenzierten Segment auf.
fn build(dir: &Path) -> Database {
    let mut db = open(dir);
    for i in 0u64..10 {
        db.put(format!("k-{:04}", i).as_bytes(), format!("v-{}", i).as_bytes())
            .expect("put");
    }
    db.flush().expect("flush");
    for i in 10u64..20 {
        db.put(format!("k-{:04}", i).as_bytes(), format!("v-{}", i).as_bytes())
            .expect("put");
    }
    db.flush().expect("flush"); // L0=2 -> compact -> >=1 Segment
    assert!(!db.segments().is_empty(), "es muss mindestens ein Segment geben");
    db
}

/// Pfad einer `.sst` mit der gegebenen `file_id` (Schema `{:06}.sst`).
fn sst_path(dir: &Path, id: u64) -> std::path::PathBuf {
    dir.join(format!("{:06}.sst", id))
}

/// Alle Schluessel/Werte aus `build` unversehrt pruefen.
fn assert_all_intact(db: &mut Database) {
    for i in 0u64..20 {
        let key = format!("k-{:04}", i);
        let expected = format!("v-{}", i).into_bytes();
        assert_eq!(db.get(key.as_bytes()).expect("get"), Some(expected), "key {key}");
    }
}

/// 1. Orphan VOR Manifest-Commit: unreferenzierte SST wird entfernt,
///    Daten bleiben vollstaendig.
#[test]
fn gc_removes_unreferenced_orphan() {
    let dir: TempDir = tempfile::tempdir().unwrap();
    let mut db = build(dir.path());

    let orphan_id = 990_001u64;
    assert!(
        !db.segments().iter().any(|s| s.file_id == orphan_id),
        "Test-Id darf nicht referenziert sein"
    );
    std::fs::write(sst_path(dir.path(), orphan_id), b"orphan-garbage").unwrap();
    assert!(sst_path(dir.path(), orphan_id).exists(), "Orphan vor gc da");

    let removed = db.gc().expect("gc");
    assert_eq!(removed, 1, "genau ein Orphan geloescht");
    assert!(!sst_path(dir.path(), orphan_id).exists(), "Orphan nach gc weg");
    assert_all_intact(&mut db);
}

/// 2. Orphan NACH Manifest-Commit: nur nicht mehr referenzierte SSTs
///    verschwinden, referenzierte bleiben, Daten/Manifest korrekt.
#[test]
fn gc_only_removes_unreferenced_after_commit() {
    let dir: TempDir = tempfile::tempdir().unwrap();
    let mut db = build(dir.path());

    let referenced: Vec<u64> = db.segments().iter().map(|s| s.file_id).collect();
    let orphans = [990_001u64, 990_002, 990_003];
    for id in orphans.iter() {
        std::fs::write(sst_path(dir.path(), *id), b"x").unwrap();
    }

    let removed = db.gc().expect("gc");
    assert_eq!(removed, orphans.len(), "alle Orphans geloescht");

    for id in referenced.iter() {
        assert!(sst_path(dir.path(), *id).exists(), "referenziert {id} ueberlebt");
    }
    assert_eq!(db.segments().len(), referenced.len(), "Manifest unveraendert");
    assert_all_intact(&mut db);
}

/// 3. Referenzierte SST darf durch GC niemals geloescht werden
///    (auch bei kaputtem Inhalt).
#[test]
fn gc_never_deletes_referenced() {
    let dir: TempDir = tempfile::tempdir().unwrap();
    let mut db = build(dir.path());

    let ref_id = db.segments()[0].file_id;
    assert!(sst_path(dir.path(), ref_id).exists(), "referenziert vor gc da");
    // Inhalt absichtlich corrupt schreiben: gc darf referenzierte trotzdem
    // nicht anfassen.
    std::fs::write(sst_path(dir.path(), ref_id), b"corrupt").unwrap();

    let removed = db.gc().expect("gc");
    assert_eq!(removed, 0, "keine unreferenzierten Orphans");
    assert!(
        sst_path(dir.path(), ref_id).exists(),
        "referenziert muss gc ueberleben"
    );
}

/// 4. Corrupt/missing referenzierte SST: GC behandelt sie NICHT als Garbage.
#[test]
fn gc_does_not_treat_missing_referenced_as_garbage() {
    let dir: TempDir = tempfile::tempdir().unwrap();
    let mut db = build(dir.path());

    let ref_id = db.segments()[0].file_id;
    // Datei loeschen (fehlt, aber im Manifest referenziert).
    std::fs::remove_file(sst_path(dir.path(), ref_id)).unwrap();

    let removed = db.gc().expect("gc");
    assert_eq!(removed, 0, "nichts zu loeschen");
    // Manifest-Referenz bleibt erhalten (gc aendert das Manifest nie).
    assert!(
        db.segments().iter().any(|s| s.file_id == ref_id),
        "Manifest-Referenz unveraendert"
    );
}

/// 5. `.manifest.tmp` wird entfernt.
#[test]
fn gc_removes_stray_manifest_tmp() {
    let dir: TempDir = tempfile::tempdir().unwrap();
    let mut db = build(dir.path());

    let tmp = dir.path().join("MANIFEST.manifest.tmp");
    std::fs::write(&tmp, b"partial").unwrap();
    assert!(tmp.exists(), "tmp vor gc da");

    let removed = db.gc().expect("gc");
    assert!(!tmp.exists(), ".manifest.tmp nach gc weg");
    // DB weiterhin nutzbar.
    assert_all_intact(&mut db);
    assert!(removed == 0, "tmp nicht als .sst gezaehlt");
}

/// 6. Fremde `.sst` (nicht 6-stellige Namenskonvention) werden uebersprungen.
#[test]
fn gc_skips_foreign_nonconforming_sst() {
    let dir: TempDir = tempfile::tempdir().unwrap();
    let mut db = build(dir.path());

    let foreign = dir.path().join("foreign.sst");
    std::fs::write(&foreign, b"i am foreign").unwrap();
    let foreign2 = dir.path().join("123.sst"); // 3 Ziffern, nicht unser Schema
    std::fs::write(&foreign2, b"also foreign").unwrap();

    let removed = db.gc().expect("gc");
    assert_eq!(removed, 0, "keine 6-stelligen Orphans");
    assert!(foreign.exists(), "foreign.sst ueberlebt");
    assert!(foreign2.exists(), "123.sst (nicht 6-stellig) ueberlebt");
}
