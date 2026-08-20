//! B.2 value_cache Wirtschaftlichkeitsmessung (feature `bench-diag`).
//!
//! Vergleich: identischer Update-Stream, einmal ueber den Warm-Pfad
//! (`CollectionHandle::put` -> `put_entity`, nutzt `value_cache`) und einmal
//! ueber die Cold-Kontrollgruppe (`Transaction::update`+`commit`, uebergibt
//! immer `None` -> nie Cache-Konsultation). Beide auf demselben Store-Typ.
//!
//! Kein Produktionscode, kein Commit. Misst Delta = Cold - Warm pro Put.

use std::path::PathBuf;
use std::time::Instant;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::Options;

fn tmp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lsm-vc-{}-{}.cur", tag, std::process::id()));
    p
}

fn mk_entity(fields: usize, seed: u64) -> Entity {
    let mut e = Entity::new();
    for i in 0..fields {
        let v = if i % 3 == 0 {
            Value::Int((seed.wrapping_add(i as u64) % 1_000_000) as i64)
        } else {
            Value::String(format!("val-{}-{:08}", i, seed.wrapping_add(i as u64)))
        };
        e.insert(format!("f{}", i), v);
    }
    e
}

fn timed<F: FnOnce() -> R, R>(f: F) -> (R, u128) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_micros())
}

/// Warm-Pfad: non-tx put_entity (nutzt value_cache, falls bench-diag aktiv).
fn run_warm(dir: &PathBuf, n: usize, idx_fields: usize, entity_fields: usize) {
    let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
    let mut col = store.collection("users").unwrap();
    for i in 0..idx_fields {
        col.create_index(&format!("f{}", i)).unwrap();
    }
    let (_, us) = timed(|| {
        for i in 0..n {
            let e = mk_entity(entity_fields, i as u64);
            col.put(&format!("e{:08}", i % 100), &e).unwrap();
        }
    });
    let per = us as f64 / n as f64;
    println!(
        "[WARM] idx={:>2} fields={:>2} n={} : {:>9.1} us/op",
        idx_fields, entity_fields, n, per
    );
}

/// Cold-Kontrollgruppe: Transaction update+commit (immer Cold-Scan).
fn run_cold(dir: &PathBuf, n: usize, idx_fields: usize, entity_fields: usize) {
    let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
    let mut col = store.collection("users").unwrap();
    for i in 0..idx_fields {
        col.create_index(&format!("f{}", i)).unwrap();
    }
    let (_, us) = timed(|| {
        for i in 0..n {
            let e = mk_entity(entity_fields, i as u64);
            let mut tx = store.transaction().unwrap();
            tx.update("users", &format!("e{:08}", i % 100), &e).unwrap();
            tx.commit().unwrap();
        }
    });
    let per = us as f64 / n as f64;
    println!(
        "[COLD] idx={:>2} fields={:>2} n={} : {:>9.1} us/op",
        idx_fields, entity_fields, n, per
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5_000);

    println!("=== B.2 value_cache Wirtschaftlichkeit (n={}) ===", n);
    println!("WARM = non-tx put (value_cache aktiv); COLD = Tx update (Cold-Scan)");
    println!();

    for idx in [0usize, 1, 2, 4, 8] {
        for fields in [4usize, 16, 64] {
            let wd = tmp_dir("w");
            let cd = tmp_dir("c");
            run_warm(&wd, n, idx, fields);
            run_cold(&cd, n, idx, fields);
            let _ = std::fs::remove_dir_all(&wd);
            let _ = std::fs::remove_dir_all(&cd);
        }
    }

    println!();
    println!("[B.2] Diagnose: Delta = COLD - WARM; Anteil = Delta/COLD");
    println!("      (nur aussagekraeftig bei idx>0; nicht-indexierte Felder");
    println!("       profitieren nicht vom value_cache)");
}
