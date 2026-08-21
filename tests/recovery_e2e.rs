//! End-to-End Recovery-Test (Vertrag aus Phase H / v1.1):
//! create → write → backup → restore → reopen → query/write.
//!
//! Es wird KEINE künstliche Crash-Semantik eingeführt — ausschließlich der
//! bestehende Backup/Restore-Vertrag wird über die Public API ausgeübt.

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::query::{Aggregate, eq};
use tempfile::tempdir;

fn seed(store: &mut EntityStore) {
    let mut tasks = store.collection("tasks").unwrap();
    for i in 0..10 {
        let mut e = Entity::new();
        e.insert("title", Value::String(format!("t{i}")));
        e.insert("status", Value::String("open".into()));
        e.insert("priority", Value::Int(i as i64));
        tasks.put(&format!("k{i}"), &e).unwrap();
    }
    // `tasks`-Handle fällt am Scope-Ende ab und gibt die `&mut`-Borrow frei.
}

fn count_open(store: &mut EntityStore) -> i64 {
    let mut b = store.query("tasks").unwrap();
    b = b.filter(eq("status", Value::String("open".into())));
    b = b.aggregate(Aggregate::Count);
    match store.execute_aggregate(b).unwrap() {
        Some(Value::Int(n)) => n,
        other => panic!("unexpected count result: {other:?}"),
    }
}

#[test]
fn recovery_create_write_backup_restore_reopen() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("db");
    let backup = dir.path().join("backup");
    let restored = dir.path().join("restored");

    // create + write
    let mut store = EntityStore::open(&src).unwrap();
    seed(&mut store);
    assert_eq!(count_open(&mut store), 10);

    // backup, dann close (deferred-durable Direct-Writes werden damit persistenz)
    store.backup(&backup).unwrap();
    store.close().unwrap();

    // restore in ein frisches Verzeichnis
    EntityStore::restore(&backup, &restored).unwrap();

    // reopen der wiederhergestellten DB + Integritätsprüfung
    let mut r = EntityStore::open(&restored).unwrap();
    assert_eq!(count_open(&mut r), 10);

    // weiterarbeiten: zusätzliche Entity schreiben, erneut öffnen, Wachstum prüfen
    {
        let mut tasks = r.collection("tasks").unwrap();
        let mut e = Entity::new();
        e.insert("title", Value::String("extra".into()));
        e.insert("status", Value::String("open".into()));
        tasks.put("extra", &e).unwrap();
    }
    r.close().unwrap();

    let mut r2 = EntityStore::open(&restored).unwrap();
    assert_eq!(count_open(&mut r2), 11);
    r2.close().unwrap();

    // Quell-DB ist vom Restore unberührt
    let mut src_store = EntityStore::open(&src).unwrap();
    assert_eq!(count_open(&mut src_store), 10);
    src_store.close().unwrap();
}
