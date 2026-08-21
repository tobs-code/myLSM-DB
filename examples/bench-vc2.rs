//! B.2a faire non-tx Kontrollgruppe (zwei Builds: mit/ohne bench-diag).
//!
//! Derselbe non-tx Put-Stream; einmal mit aktivem value_cache (Feature an),
//! einmal ohne (Feature aus -> Code wegkompiliert). Misst den *reinen*
//! Cache-Effekt, ohne Transaction-/Commit-Overhead.
//!
//! Kein Produktionscode, kein Commit.

use std::path::PathBuf;
use std::time::Instant;

use my_lsm_db::Options;
use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};

fn tmp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lsm-vc2-{}-{}.cur", tag, std::process::id()));
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

fn run(dir: &PathBuf, n: usize, idx_fields: usize, entity_fields: usize) {
    let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        for i in 0..idx_fields {
            col.create_index(&format!("f{}", i)).unwrap();
        }
    }
    // Warmup (nicht gemessen)
    for i in 0..(n / 10) {
        let e = mk_entity(entity_fields, i as u64);
        store
            .collection("users")
            .unwrap()
            .put(&format!("e{:08}", i % 100), &e)
            .unwrap();
    }
    let t = Instant::now();
    for i in 0..n {
        let e = mk_entity(entity_fields, (i + 100_000) as u64);
        store
            .collection("users")
            .unwrap()
            .put(&format!("e{:08}", i % 100), &e)
            .unwrap();
    }
    let us = t.elapsed().as_micros();
    let per = us as f64 / n as f64;
    let mode = if cfg!(feature = "bench-diag") {
        "CACHE-ON "
    } else {
        "CACHE-OFF"
    };
    println!(
        "[{}] idx={:>2} fields={:>2} n={} : {:>9.1} us/op",
        mode, idx_fields, entity_fields, n, per
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5_000);

    println!(
        "=== B.2a non-tx Kontrollgruppe (n={}, feature={}) ===",
        n,
        if cfg!(feature = "bench-diag") {
            "bench-diag"
        } else {
            "none"
        }
    );
    for idx in [0usize, 1, 2, 4, 8] {
        for fields in [4usize, 16, 64] {
            let d = tmp_dir("x");
            run(&d, n, idx, fields);
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}
