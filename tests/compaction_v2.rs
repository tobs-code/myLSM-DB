//! Regression-Tests für die v0.7.4 (3a)-Compaction (Overlap-Merge in
//! partitionierte L1-Segmente).
//!
//! Zusätzlich zu den generischen LWW/Tombstone-Fällen (siehe compaction.rs)
//! prüfen diese Tests die 3a-spezifischen Invarianten der Design-Spez (§9–§14):
//! - Segment-Disjunktheit: Segmente bleiben streng nach `min_key` sortiert und
//!   `seg[i].max_key < seg[i+1].min_key`, auch nach vielen Compactions.
//! - Lookup-Tiefe: `get()` berührt höchstens L0 + genau ein L1-Segment.
//! - Range-Scan über Segmentgrenzen liefert global sortierte, LWW-korrekte Daten.
//! - Tombstone-Sweep: endgültig gelöschte Keys verschwinden physisch.
//! - Reopen: Segment-Zustand übersteht sauberes close/open.
//! - Harte Manifest-Validierung: fehlende Segment-Datei oder falsche
//!   Segment-Range → `Error::Corrupt`.

use std::collections::BTreeMap;
use std::path::Path;

use my_lsm_db::error::Error;
use my_lsm_db::{Database, Options};

/// Kleine MemTable + Schwellwert 2 → Compaction nach jedem 2. Flush;
/// `segment_max_records` klein → deterministischer Segment-Split nach wenigen
/// Records (echte Segmentgrenzen, mehrere Segmente).
fn db(dir: &Path) -> Database {
    Database::open_with(
        dir,
        Options {
            memtable_limit: 64 * 1024,
            l0_compact_threshold: 2,
            segment_max_records: 8,
            ..Options::default()
        },
    )
    .unwrap()
}

fn model(puts: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    puts.iter()
        .map(|(k, v)| (k.clone(), Some(v.clone())))
        .collect()
}

fn assert_disjoint(db: &Database, dir: &Path) {
    let segs = db.segments();
    for w in segs.windows(2) {
        assert!(w[0].min_key < w[1].min_key, "segments not sorted: {w:?}");
        assert!(
            w[0].max_key < w[1].min_key,
            "segments overlap: {w:?}"
        );
    }
    for s in segs {
        assert!(s.min_key <= s.max_key, "segment range inverted: {s:?}");
        assert!(
            dir.join(format!("{:06}.sst", s.file_id)).exists(),
            "segment {} missing on disk",
            s.file_id
        );
    }
}

/// Schreibt `n` eindeutige Keys in Flush-Runden (löst Compactions aus).
fn seed_keys(db: &mut Database, puts: &mut BTreeMap<Vec<u8>, Vec<u8>>, n: u32) {
    for i in 0..n {
        let k = format!("key-{:03}", i).into_bytes();
        let v = format!("val-{}", i).into_bytes();
        db.put(&k, &v).unwrap();
        puts.insert(k, v);
        if i % 5 == 0 {
            db.flush().unwrap();
        }
    }
    db.flush().unwrap();
}

#[test]
fn lww_newest_wins_across_multiple_segments() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    // Schlüssel k über mehrere Compactions überschreiben → LWW.
    for i in 0..24u32 {
        db.put(b"k", format!("v{i}").as_bytes()).unwrap();
        if i % 3 == 0 {
            db.flush().unwrap();
        }
    }
    assert_eq!(db.get(b"k").unwrap(), Some(b"v23".to_vec()));
    assert_disjoint(&db, dir.path());
    assert!(db.table_count() <= 3, "tables: {}", db.table_count());
}

#[test]
fn partial_update_preserved_across_segments() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    for round in 0..3 {
        db.put(b"E:1", b"1").unwrap();
        db.put(b"E:1.name", format!("alice{round}").as_bytes()).unwrap();
        db.put(b"E:1.age", b"30").unwrap();
        db.put(b"E:1.city", b"berlin").unwrap();
        db.flush().unwrap();
        db.put(b"E:1.age", b"31").unwrap();
        db.flush().unwrap(); // compact
        assert_eq!(db.get(b"E:1.name").unwrap(), Some(format!("alice{round}").into_bytes()));
        assert_eq!(db.get(b"E:1.age").unwrap(), Some(b"31".to_vec()));
        assert_eq!(db.get(b"E:1.city").unwrap(), Some(b"berlin".to_vec()));
    }
    assert_eq!(db.get(b"E:1").unwrap(), Some(b"1".to_vec()));
    assert_disjoint(&db, dir.path());
}

#[test]
fn delete_then_reinsert_across_segments() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let mut puts = BTreeMap::new();
    seed_keys(&mut db, &mut puts, 30);
    db.delete(b"key-010").unwrap();
    puts.remove(&b"key-010".to_vec());
    db.flush().unwrap(); // compact → Tombstone wird im Merge gesweept
    assert_eq!(db.get(b"key-010").unwrap(), None);

    // Wieder einfügen → lebt erneut (LWW gegen die historischen Werte).
    db.put(b"key-010", b"v2").unwrap();
    puts.insert(b"key-010".to_vec(), b"v2".to_vec());
    db.flush().unwrap();
    db.flush().unwrap(); // compact
    assert_eq!(db.get(b"key-010").unwrap(), Some(b"v2".to_vec()));
    assert_eq!(db.scan(None, None).unwrap(), model(&puts));
    assert_disjoint(&db, dir.path());
}

#[test]
fn index_key_lifecycle_across_segments() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    db.put(b"I:email:bob@x", b"E:5").unwrap();
    db.flush().unwrap();
    db.put(b"I:email:bob@x", b"E:7").unwrap(); // Re-Index
    db.flush().unwrap(); // compact
    assert_eq!(db.get(b"I:email:bob@x").unwrap(), Some(b"E:7".to_vec()));

    db.delete(b"I:email:bob@x").unwrap();
    db.flush().unwrap();
    db.flush().unwrap(); // compact
    assert_eq!(db.get(b"I:email:bob@x").unwrap(), None);
    assert_eq!(
        db.scan(Some(b"I:email:"), None).unwrap(),
        Vec::<(Vec<u8>, Option<Vec<u8>>)>::new()
    );

    db.put(b"I:email:c@x", b"E:9").unwrap();
    db.flush().unwrap();
    db.flush().unwrap(); // compact
    let range = db.scan(Some(b"I:email:"), None).unwrap();
    assert_eq!(range.len(), 1);
    assert_eq!(range[0].0, b"I:email:c@x".to_vec());
    assert_eq!(range[0].1, Some(b"E:9".to_vec()));
    assert_disjoint(&db, dir.path());
}

#[test]
fn scan_globally_sorted_across_segment_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let mut puts = BTreeMap::new();
    seed_keys(&mut db, &mut puts, 40); // 40 Keys / 8 je Segment → mehrere Segmente
    assert!(db.segment_count() > 1, "expected multiple segments");
    assert_disjoint(&db, dir.path());

    // Voll-Scan == Modell (global sortiert, LWW).
    assert_eq!(db.scan(None, None).unwrap(), model(&puts));

    // Range, der eine Segmentgrenze kreuzt: key-007 (Segment A) .. key-008 (B).
    let cross = db.scan(Some(b"key-007"), Some(b"key-009")).unwrap();
    assert_eq!(
        cross,
        vec![
            (b"key-007".to_vec(), Some(b"val-7".to_vec())),
            (b"key-008".to_vec(), Some(b"val-8".to_vec())),
        ]
    );

    // Präfix über eine Segmentgrenze hinweg: key-000..key-009 (end exklusiv).
    let prefix = db.scan(Some(b"key-000"), Some(b"key-010")).unwrap();
    assert_eq!(prefix.len(), 10);
    assert_eq!(prefix[0].0, b"key-000".to_vec());
    assert_eq!(prefix[9].0, b"key-009".to_vec());
}

#[test]
fn get_matches_scan_per_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let mut puts = BTreeMap::new();
    seed_keys(&mut db, &mut puts, 40);
    assert!(db.segment_count() > 1);
    let scan = db.scan(None, None).unwrap();
    assert_eq!(scan, model(&puts));
    for (k, v) in &puts {
        assert_eq!(db.get(k).unwrap(), Some(v.clone()), "get({k:?})");
    }
}

#[test]
fn segment_invariants_hold_after_many_compactions() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let mut puts = BTreeMap::new();
    for round in 0..8 {
        for i in 0..12u32 {
            let k = format!("key-{:03}", (i + round) % 30).into_bytes();
            let v = format!("val-r{}-{}", round, i).into_bytes();
            db.put(&k, &v).unwrap();
            puts.insert(k, v);
            if i % 4 == 0 {
                db.flush().unwrap();
            }
        }
        db.flush().unwrap();
        assert_disjoint(&db, dir.path());
    }
    assert_eq!(db.scan(None, None).unwrap(), model(&puts));
    assert!(db.table_count() <= 3, "tables: {}", db.table_count());
    for (k, v) in &puts {
        assert_eq!(db.get(k).unwrap(), Some(v.clone()));
    }
}

#[test]
fn tombstone_sweep_removes_dead_keys_physically() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let mut puts = BTreeMap::new();
    seed_keys(&mut db, &mut puts, 30);

    // Alle Keys key-010..key-029 endgültig löschen (volle Historie im Merge-Set).
    for i in 10..30u32 {
        let k = format!("key-{:03}", i).into_bytes();
        db.delete(&k).unwrap();
        puts.remove(&k);
    }
    db.flush().unwrap(); // flush löst compact aus → Tombstones gesweept

    // Survivors sind key-000..key-009.
    assert_eq!(db.scan(None, None).unwrap(), model(&puts));
    for i in 10..30u32 {
        assert_eq!(db.get(format!("key-{:03}", i).as_bytes()).unwrap(), None);
    }
    // Physischer Sweep: kein Segment enthält gelöschte Keys mehr.
    let total_records: u64 = db.segments().iter().map(|s| s.records).sum();
    assert!(total_records <= 10, "sweep ineffektiv: {total_records} records");
    assert_disjoint(&db, dir.path());
}

#[test]
fn reopen_preserves_segments_and_data() {
    let dir = tempfile::tempdir().unwrap();
    let mut puts = BTreeMap::new();
    let segments_before;
    let scan_before;
    {
        let mut db = db(dir.path());
        seed_keys(&mut db, &mut puts, 40);
        assert!(db.segment_count() > 1);
        segments_before = db.segments().to_vec();
        scan_before = db.scan(None, None).unwrap();
        db.close().unwrap();
    }
    let mut reopened = Database::open(dir.path()).unwrap();
    assert_disjoint(&reopened, dir.path());
    assert_eq!(reopened.segments(), segments_before);
    assert_eq!(reopened.scan(None, None).unwrap(), scan_before);
    for (k, v) in &puts {
        assert_eq!(reopened.get(k).unwrap(), Some(v.clone()));
    }
}

#[test]
fn open_fails_if_segment_file_missing() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = db(dir.path());
        let mut puts = BTreeMap::new();
        seed_keys(&mut db, &mut puts, 40);
        db.close().unwrap();
    }
    // Eine Segment-Datei entfernen → Manifest-Referenz auf fehlende Datei.
    let missing = {
        let opened = db(dir.path());
        opened.segments()[0].file_id
    };
    let path = dir.path().join(format!("{missing:06}.sst"));
    std::fs::remove_file(&path).unwrap();
    let err = match Database::open(dir.path()) {
        Ok(_) => panic!("expected error, got Ok"),
        Err(e) => e,
    };
    assert!(
        matches!(err, Error::Corrupt(_)),
        "expected Corrupt, got {err:?}"
    );
}

#[test]
fn open_fails_if_segment_range_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = db(dir.path());
        let mut puts = BTreeMap::new();
        seed_keys(&mut db, &mut puts, 12); // 1 Segment (12 Keys ≤ 2×8)
        db.close().unwrap();
    }
    // Manifest manipulieren: min_hex der S-Zeile auf "00" setzen → Range
    // stimmt nicht mehr mit dem Index der Datei überein → Corrupt.
    let manifest = dir.path().join("MANIFEST");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    for line in &mut lines {
        if line.starts_with("S ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            *line = format!("S 1 {} 00 {} {}", parts[2], parts[4], parts[5]);
            break;
        }
    }
    std::fs::write(&manifest, lines.join("\n")).unwrap();
    let err = match Database::open(dir.path()) {
        Ok(_) => panic!("expected error, got Ok"),
        Err(e) => e,
    };
    assert!(
        matches!(err, Error::Corrupt(_)),
        "expected Corrupt, got {err:?}"
    );
}

#[test]
fn open_fails_on_overlapping_manifest_segments() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = db(dir.path());
        let mut puts = BTreeMap::new();
        seed_keys(&mut db, &mut puts, 40);
        db.close().unwrap();
    }
    // Die zweite S-Zeile bekommt die Range der ersten → Segmente überlappen →
    // `Manifest::validate()` lehnt hart ab (InvalidFormat).
    let manifest = dir.path().join("MANIFEST");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut s_indices: Vec<usize> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.starts_with("S ") {
            s_indices.push(i);
        }
    }
    assert!(s_indices.len() >= 2, "expected at least 2 segments");
    let first_parts: Vec<String> = lines[s_indices[0]].split_whitespace().map(String::from).collect();
    let second_parts: Vec<String> = lines[s_indices[1]].split_whitespace().map(String::from).collect();
    lines[s_indices[1]] = format!(
        "S 1 {} {} {} {}",
        second_parts[2], first_parts[3], first_parts[4], second_parts[5]
    );
    std::fs::write(&manifest, lines.join("\n")).unwrap();
    let err = match Database::open(dir.path()) {
        Ok(_) => panic!("expected error, got Ok"),
        Err(e) => e,
    };
    assert!(
        matches!(err, Error::InvalidFormat(_)),
        "expected InvalidFormat, got {err:?}"
    );
}