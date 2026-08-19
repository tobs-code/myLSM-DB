//! Regression- und Crash-Tests für die v0.7-C (1d)-Compaction.
//!
//! Kern-Designentscheidungen, die hier bewiesen werden:
//! - LWW über mehrere SSTables, unabhängig von der Compaction.
//! - Partial-Update: Compaction ist key-blind, jedes Feld behält seinen eigenen
//!   neuesten Wert über die Compaction hinweg.
//! - Tombstone-Sicherheit: Ein Tombstone darf bei einer *partiellen* Compaction
//!   (Flatten) nicht verschwinden, solange ältere Daten außerhalb des Merge-Sets
//!   existieren können. Erst beim Full-Merge (Konsolidierung) darf er entfallen —
//!   und dann nur, wenn keine ältere Version mehr existiert.
//! - Scan/get vor/nach Compaction identisch; Tombstones erscheinen in Scans nicht.
//! - Gebundene Lookup-Tiefe: `table_count()` bleibt ≤ `l0_compact_threshold + 1`.
//! - Crash-Recovery: Kein Manifest-Verweis auf fehlende Dateien (COMMIT vor Löschung).

use std::collections::BTreeMap;
use std::path::Path;

use my_lsm_db::{Database, Options};

/// Kleine MemTable + niedriger Schwellwert → viele Flushes + Compactions,
/// damit die Regression deterministisch und schnell auslöst.
fn db(dir: &Path) -> Database {
    Database::open_with(
        dir,
        Options {
            memtable_limit: 1024 * 1024,
            l0_compact_threshold: 2,
            ..Options::default()
        },
    )
    .unwrap()
}

/// Eager-Orakel (BTreeMap, newest-wins, ohne Tombstones) über die Keys.
fn model(puts: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    puts.iter()
        .map(|(k, v)| (k.clone(), Some(v.clone())))
        .collect()
}

#[test]
fn lww_newest_wins_across_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    for i in 0..10 {
        db.put(b"k", format!("v{i}").as_bytes()).unwrap();
        db.flush().unwrap(); // jede 2. flush löst compact aus
    }
    assert_eq!(db.get(b"k").unwrap(), Some(b"v9".to_vec()));
    // Invariante: gebundene Lookup-Tiefe (L0 ≤ threshold + L1 ≤ 1).
    assert!(db.table_count() <= 3, "tables: {}", db.table_count());
}

#[test]
fn partial_update_preserved_over_compaction() {
    // Eine Entität = Basis-Key + mehrere Feld-Keys. Compaction ist key-blind,
    // also muss jedes Feld seinen eigenen neuesten Wert über Flush/Compact behalten.
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    db.put(b"E:1", b"1").unwrap();
    db.put(b"E:1.name", b"alice").unwrap();
    db.put(b"E:1.age", b"30").unwrap();
    db.flush().unwrap();

    // Nur "age" ändern + "city" hinzufügen, Rest unangetastet.
    db.put(b"E:1.age", b"31").unwrap();
    db.put(b"E:1.city", b"berlin").unwrap();
    db.flush().unwrap(); // compact

    assert_eq!(db.get(b"E:1").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"E:1.name").unwrap(), Some(b"alice".to_vec()));
    assert_eq!(db.get(b"E:1.age").unwrap(), Some(b"31".to_vec()));
    assert_eq!(db.get(b"E:1.city").unwrap(), Some(b"berlin".to_vec()));

    // Noch eine Runde über weitere Compactions.
    db.put(b"E:1.name", b"bob").unwrap();
    db.flush().unwrap();
    db.put(b"E:1.age", b"32").unwrap();
    db.flush().unwrap(); // compact
    assert_eq!(db.get(b"E:1.name").unwrap(), Some(b"bob".to_vec()));
    assert_eq!(db.get(b"E:1.age").unwrap(), Some(b"32".to_vec()));
    assert_eq!(db.get(b"E:1.city").unwrap(), Some(b"berlin".to_vec()));
}

#[test]
fn delete_survives_compaction_while_older_may_exist() {
    // Kritischer Tombstone-Test (aus dem Design):
    //   old table:  entity.field = 42
    //   new table:  entity.field = DELETE
    //   partial compaction → get(entity.field) == None
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    db.put(b"E:1.score", b"42").unwrap();
    db.flush().unwrap(); // old: score = 42
    db.delete(b"E:1.score").unwrap();
    db.flush().unwrap(); // new: Tombstone; compact (flatten ODER konsolidieren)

    // Egal welcher Pfad: gelöscht bleibt gelöscht.
    assert_eq!(db.get(b"E:1.score").unwrap(), None);

    // Weitere Flushes/Compactions: bleibt gelöscht.
    db.delete(b"E:1.score").unwrap();
    db.flush().unwrap();
    db.delete(b"E:1.score").unwrap();
    db.flush().unwrap();
    assert_eq!(db.get(b"E:1.score").unwrap(), None);
}

#[test]
fn delete_then_reinsert_after_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    db.put(b"k", b"v1").unwrap();
    db.flush().unwrap();
    db.delete(b"k").unwrap();
    db.flush().unwrap(); // compact
    assert_eq!(db.get(b"k").unwrap(), None);

    db.put(b"k", b"v2").unwrap();
    db.flush().unwrap();
    db.flush().unwrap(); // compact
    assert_eq!(db.get(b"k").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn index_key_lifecycle_over_compaction() {
    // Index-Keys sind gewöhnliche KV-Keys → Compaction ist key-blind, aber wir
    // verifizieren den vollen Lebenszyklus (add → change → remove) über Compactions.
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    db.put(b"I:email:bob@x", b"E:5").unwrap();
    db.flush().unwrap();
    db.put(b"I:email:bob@x", b"E:7").unwrap(); // Re-Index (Mapping wechselt)
    db.flush().unwrap(); // compact
    assert_eq!(db.get(b"I:email:bob@x").unwrap(), Some(b"E:7".to_vec()));

    db.delete(b"I:email:bob@x").unwrap();
    db.flush().unwrap();
    db.flush().unwrap(); // compact
    assert_eq!(db.get(b"I:email:bob@x").unwrap(), None);
    // Prefix-Scan über den Indexbereich zeigt die Löschung nicht mehr.
    let range = db.scan(Some(b"I:email:"), None).unwrap();
    assert_eq!(range, Vec::<(Vec<u8>, Option<Vec<u8>>)>::new());

    // Wieder hinzufügen und per Range finden.
    db.put(b"I:email:c@x", b"E:9").unwrap();
    db.flush().unwrap();
    db.flush().unwrap(); // compact
    let range = db.scan(Some(b"I:email:"), None).unwrap();
    assert_eq!(range.len(), 1);
    assert_eq!(range[0].0, b"I:email:c@x".to_vec());
    assert_eq!(range[0].1, Some(b"E:9".to_vec()));
}

#[test]
fn scan_matches_model_after_compactions() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let mut puts: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut seq = 0u32;
    for round in 0..6 {
        for i in 0..3 {
            let k = format!("key{}", (i + round) % 4).into_bytes();
            let v = format!("v{}", seq).into_bytes();
            db.put(&k, &v).unwrap();
            puts.insert(k, v);
            seq += 1;
            if round % 2 == 0 {
                db.flush().unwrap();
            }
        }
        db.flush().unwrap();
    }
    // Einen Key löschen → darf in Scan (und Modell) nicht mehr auftauchen.
    db.delete(b"key1").unwrap();
    puts.remove(&b"key1".to_vec());
    db.flush().unwrap();
    db.flush().unwrap(); // compact

    let actual = db.scan(None, None).unwrap();
    assert_eq!(actual, model(&puts));
}

#[test]
fn scan_and_get_identical_before_after_reopen_compaction() {
    // Nach sauberem Close + Reopen (frische Compaction-Logik, keine MemTable)
    // müssen Scan/get exakt identisch zum geschlossenen Zustand sein.
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let mut puts: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for i in 0..20u32 {
        let k = format!("key-{:03}", i % 7).into_bytes();
        let v = format!("val-{}", i).into_bytes();
        db.put(&k, &v).unwrap();
        puts.insert(k.clone(), v.clone());
        if i % 3 == 0 {
            db.flush().unwrap();
        }
    }
    let before = db.scan(None, None).unwrap();
    assert_eq!(before, model(&puts));
    db.close().unwrap();

    let mut reopened = Database::open(dir.path()).unwrap();
    let after = reopened.scan(None, None).unwrap();
    assert_eq!(after, before);
    for (k, _) in &puts {
        assert_eq!(reopened.get(k).unwrap(), Some(puts[k].clone()));
    }
}

#[test]
fn table_count_stays_bounded_after_many_flushes() {
    // Gebundene Lookup-Tiefe ist die Architektur-Invariante: die Tabellenanzahl
    // darf nie über threshold + 1 wachsen, unabhängig von der Datenmenge.
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let bound = 3; // threshold(2) + 1
    for i in 0..200u32 {
        db.put(
            format!("key-{:04}", i % 50).as_bytes(),
            format!("val-{}", i).as_bytes(),
        )
        .unwrap();
        if i % 7 == 0 {
            db.flush().unwrap();
        }
    }
    db.flush().unwrap();
    assert!(
        db.table_count() <= bound,
        "tables {} > bound {bound}",
        db.table_count()
    );
}

#[test]
fn no_manifest_refs_to_missing_files_after_compaction() {
    // Nach einem sauberen Ablauf darf jede Manifest-Referenz auf eine Datei zeigen.
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    for i in 0..40u32 {
        db.put(
            format!("k{:03}", i % 10).as_bytes(),
            format!("v{}", i).as_bytes(),
        )
        .unwrap();
        if i % 5 == 0 {
            db.flush().unwrap();
        }
    }
    db.flush().unwrap();
    db.close().unwrap();

    let reopened = Database::open(dir.path()).unwrap();
    for id in reopened.table_ids() {
        let path = dir.path().join(format!("{:06}.sst", id));
        assert!(path.exists(), "manifest verweist auf fehlende Datei {path:?}");
    }
}
