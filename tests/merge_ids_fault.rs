//! E.8: Regression für den in E.7 reproduzierten `merge_ids`-Datenverlust-Bug.
//!
//! Vor dem Fix übersprang `merge_ids` (`src/lib.rs`) eine unlesbare manifestierte
//! SSTable still (`if let Ok`) und committete ein neues Manifest ohne die
//! Daten → stiller Datenverlust (key-A verschwand spurlos).
//!
//! Nach dem Fix muss `compact()` beim Lesefehler mit `Err` abbrechen, das
//! Manifest darf NICHT aktualisiert werden, und ein erneutes Öffnen muss die
//! Korruption sauber als `Error::Corrupt` melden — statt key-A still als
//! nicht vorhanden zu behandeln.
//!
//! Kein Produktionscode wird hier verändert; dies ist der permanente
//! Regressionstest für den Fix.

use std::path::Path;

use my_lsm_db::error::Error;
use my_lsm_db::{Database, Options};

fn db(dir: &Path) -> Database {
    Database::open_with(
        dir,
        Options {
            memtable_limit: 64 * 1024,
            l0_compact_threshold: 2,
            ..Options::default()
        },
    )
    .unwrap()
}

/// Baut deterministisch zwei disjunkte L1-Segmente auf und gibt die warme DB
/// plus die ID des ersten (S1 = {key-A, key-B}) zurück.
fn build(dir: &Path) -> (Database, u64) {
    let mut db = db(dir);

    db.put(b"key-A", b"v-round1").unwrap();
    db.put(b"key-B", b"v-round1").unwrap();
    db.flush().unwrap();
    db.put(b"key-A", b"v-round1").unwrap();
    db.put(b"key-B", b"v-round1").unwrap();
    db.flush().unwrap(); // L0={1,2} → compact → Segment S1 {key-A,key-B}

    db.put(b"key-C", b"v-round1").unwrap();
    db.put(b"key-D", b"v-round1").unwrap();
    db.flush().unwrap();
    db.put(b"key-C", b"v-round1").unwrap();
    db.put(b"key-D", b"v-round1").unwrap();
    db.flush().unwrap(); // Segment S2 {key-C,key-D}, S1 unangetastet

    let segs = db.segments().to_vec();
    assert_eq!(segs.len(), 2, "erwarte 2 L1-Segmente, habe {}", segs.len());
    assert_eq!(segs[0].min_key, b"key-A", "S1 min");
    assert_eq!(segs[0].max_key, b"key-B", "S1 max");

    // Prä-Check: key-A ist da.
    assert_eq!(
        db.get(b"key-A").unwrap(),
        Some(b"v-round1".to_vec()),
        "Prä-Check: key-A fehlt"
    );
    (db, segs[0].file_id)
}

/// Löst eine Compaction aus, deren Batch-Span S1 (key-A..key-B) überlappt, ohne
/// key-A neu zu schreiben. `key-B` wird überschrieben, um den Merge zu
/// erzwingen; key-A existiert NUR im (bald gelöschten) S1.
/// Gibt das Ergebnis des flush zurück, das den Compact auslöst.
fn trigger_compaction(db: &mut Database) -> my_lsm_db::error::Result<()> {
    db.put(b"key-B", b"overwrite").unwrap();
    db.flush().unwrap(); // L0 = {x}
    db.put(b"key-B", b"overwrite2").unwrap();
    db.flush() // L0 = {x,y} → threshold → compact liest S1
}

#[test]
fn missing_input_sstable_fails_compaction_instead_of_silent_loss() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, victim_id) = build(dir.path());

    // Opfer-Segment S1-Datei gezielt löschen (während des Betriebs).
    let victim_path = dir.path().join(format!("{:06}.sst", victim_id));
    assert!(victim_path.exists(), "Victim-Datei fehlt: {victim_path:?}");
    std::fs::remove_file(&victim_path).unwrap();

    // Compaction darf NICHT still erfolgreich sein (kein Datenverlust-Commit).
    let res = trigger_compaction(&mut db);
    assert!(
        res.is_err(),
        "compact() bei gelöschter manifestierter SSTable {victim_id} \
         muss mit Err abbrechen, nicht still committen"
    );

    // Manifest unverändert (alte SSTables intakt): Ein erneutes Öffnen muss die
    // Korruption sauber als `Error::Corrupt` melden, statt key-A still als
    // nicht-vorhanden zu behandeln.
    drop(db);
    let reopen = Database::open(dir.path());
    assert!(
        matches!(reopen, Err(Error::Corrupt(_))),
        "Reopen nach Löschen von Segment {victim_id} muss Corrupt melden, \
         nicht key-A still als nicht-vorhanden behandeln"
    );
}

#[test]
fn missing_input_sstable_compaction_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, victim_id) = build(dir.path());

    let victim_path = dir.path().join(format!("{:06}.sst", victim_id));
    std::fs::remove_file(&victim_path).unwrap();

    // Erzwinge compact direkt über put+flush+put+flush und fange den Fehler.
    db.put(b"key-B", b"overwrite").unwrap();
    db.flush().unwrap();
    db.put(b"key-B", b"overwrite2").unwrap();
    let res = db.flush(); // löst compact aus → muss Err sein
    assert!(
        res.is_err(),
        "compact() bei gelöschter manifestierter SSTable {victim_id} \
         muss mit Err abbrechen, nicht still committen"
    );
}
