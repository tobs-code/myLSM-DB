//! Minimaler Einstieg in `my-lsm-db` (High-Level-API `EntityStore`).
//!
//! Zeigt: öffnen → schreiben/lesen → abfragen → sichern/wiederherstellen.
//! Starten mit `cargo run --example intro`.

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::query::{Aggregate, eq};
use tempfile::tempdir;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let db = dir.path().join("db");
    let backup = dir.path().join("backup");
    let restored = dir.path().join("restored");

    // --- open → put / get ---
    let mut store = EntityStore::open(&db)?;
    {
        let mut tasks = store.collection("tasks")?;
        for i in 0..3 {
            let mut e = Entity::new();
            e.insert("title", Value::String(format!("Task {i}")));
            e.insert("status", Value::String("open".into()));
            e.insert("priority", Value::Int(i as i64));
            tasks.put(&format!("k{i}"), &e)?;
        }
    }

    let one = store.collection("tasks")?.get("k0")?;
    println!(
        "k0.title = {:?}",
        one.and_then(|e| e.field("title").cloned())
    );

    // --- query (filter + aggregate) ---
    let mut b = store.query("tasks")?;
    b = b.filter(eq("status", Value::String("open".into())));
    b = b.aggregate(Aggregate::Count);
    println!("open tasks = {:?}", store.execute_aggregate(b)?);

    // --- backup ---
    store.backup(&backup)?;
    store.close()?;

    // --- restore → reopen → query ---
    EntityStore::restore(&backup, &restored)?;
    let mut r = EntityStore::open(&restored)?;
    let mut b = r.query("tasks")?;
    b = b.aggregate(Aggregate::Count);
    println!("restored open tasks = {:?}", r.execute_aggregate(b)?);
    r.close()?;

    println!("OK: intro example completed");
    Ok(())
}
