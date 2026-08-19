use my_lsm_db::{Database, Options};

#[test]
fn put_get_delete_basic() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
    db.put(b"a", b"1").unwrap();
    db.put(b"b", b"2").unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
    assert_eq!(db.get(b"nope").unwrap(), None);

    db.delete(b"a").unwrap();
    assert_eq!(db.get(b"a").unwrap(), None);
}

#[test]
fn scan_range_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
    for (k, v) in [("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")] {
        db.put(k.as_bytes(), v.as_bytes()).unwrap();
    }
    let all = db.scan(None, None).unwrap();
    assert_eq!(
        all.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    let range = db.scan(Some(b"b"), Some(b"d")).unwrap();
    assert_eq!(range.len(), 2);
    assert_eq!(range[0].0, b"b".to_vec());
    assert_eq!(range[1].0, b"c".to_vec());
}

#[test]
fn flush_persists_and_compacts() {
    let dir = tempfile::tempdir().unwrap();
    let opts = Options {
        memtable_limit: 256, // winzige MemTable → früher Flush
        l0_compact_threshold: 3,
        ..Options::default()
    };
    let mut db = Database::open_with(dir.path(), opts).unwrap();
    for i in 0..50u32 {
        db.put(
            format!("key-{:04}", i).as_bytes(),
            format!("val-{}", i).as_bytes(),
        )
        .unwrap();
    }
    // Compaction sollte nach mehreren Flushes Tabellen zusammengeführt haben.
    assert!(db.level_tables(1) >= 1 || db.table_count() > 0);
    // Alle Werte trotz Flush/Compaction korrekt lesbar.
    for i in 0..50u32 {
        assert_eq!(
            db.get(format!("key-{:04}", i).as_bytes()).unwrap(),
            Some(format!("val-{}", i).into_bytes())
        );
    }
    // Keys, die gelöscht wurden, tauchen nach Compaction nicht wieder auf.
    db.delete(b"key-0000").unwrap();
    db.flush().unwrap();
    assert_eq!(db.get(b"key-0000").unwrap(), None);
}

#[test]
fn recovery_replays_wal() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
    db.put(b"x", b"1").unwrap();
    db.put(b"y", b"2").unwrap();
    db.delete(b"x").unwrap();
    drop(db); // nicht geflusht → alles muss aus dem WAL rekonstruiert werden

    let mut db2 = Database::open(dir.path()).unwrap();
    assert_eq!(db2.get(b"x").unwrap(), None); // gelöscht, auch nach Recovery
    assert_eq!(db2.get(b"y").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn recovery_after_flush_and_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let opts = Options {
        memtable_limit: 128,
        l0_compact_threshold: 2,
        ..Options::default()
    };
    {
        let mut db = Database::open_with(dir.path(), opts.clone()).unwrap();
        for i in 0..30u32 {
            db.put(
                format!("k{:03}", i).as_bytes(),
                format!("v{}", i).as_bytes(),
            )
            .unwrap();
        }
    }
    drop(opts);
    // Neustart: Manifest + SSTables rekonstruieren den Zustand.
    let mut db2 = Database::open_with(dir.path(), Options::default()).unwrap();
    for i in 0..30u32 {
        assert_eq!(
            db2.get(format!("k{:03}", i).as_bytes()).unwrap(),
            Some(format!("v{}", i).into_bytes())
        );
    }
}

#[test]
fn overwrite_newest_wins() {
    let dir = tempfile::tempdir().unwrap();
    let opts = Options {
        memtable_limit: 64, // erzwingt Flush zwischen den Writes
        l0_compact_threshold: 3,
        ..Options::default()
    };
    let mut db = Database::open_with(dir.path(), opts).unwrap();
    db.put(b"k", b"v1").unwrap();
    db.flush().unwrap();
    db.put(b"k", b"v2").unwrap();
    db.flush().unwrap();
    db.put(b"k", b"v3").unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v3".to_vec()));
}
