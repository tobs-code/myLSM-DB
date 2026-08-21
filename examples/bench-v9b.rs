//! v0.9b Bytes/Decoding-Kausalitaet (nur Messung, KEIN Storage-Touch, KEIN Commit).
//!
//! Trennt, ob `get` mit den tatsaechlich gelesenen Bytes (Recordgroesse)
//! skaliert, oder ob Record-Struktur / Decoding / Allokation dominiert.
//!
//! Messungen (reine API-Differenziale, n Entities, get-Hit mit festem Key):
//!  - S1: ein großes String-Feld (1 Feld, ~N Bytes Payload)
//!  - S2: viele kleine Felder (N/40 Felder a ~40 Bytes) -> gleiche Payload
//!  - S3: festes 8-Feld-Schema, nur Wert-Bytes variieren (klein vs. groß)
//!  - S4: reine Feldanzahl-Variation (1/8/32/128 Felder, konstante Feldgroesse)
//! Jeder Fall: frischer Store + 1 Warmup-Runde, dann getaktet.
//!
//! Aufruf: cargo run --release --example bench-v9b -- <n> <payload_bytes>

use std::path::PathBuf;
use std::time::Instant;

use my_lsm_db::Options;
use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};

fn tmp_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lsm-v9b-{}.cur", std::process::id()));
    p
}

fn timed<F: FnOnce() -> R, R>(f: F) -> (R, u128) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_micros())
}

fn entity_one_big(payload: usize) -> Entity {
    let mut e = Entity::new();
    e.insert("f0", Value::String("x".repeat(payload)));
    e
}

fn entity_many_small(payload: usize) -> Entity {
    let per = 40usize;
    let n = (payload / per).max(1);
    let mut e = Entity::new();
    for i in 0..n {
        e.insert(format!("f{}", i), Value::String("x".repeat(per)));
    }
    e
}

fn entity_fixed_schema(payload: usize) -> Entity {
    let mut e = Entity::new();
    let per = (payload / 8).max(1);
    for i in 0..8 {
        e.insert(format!("f{}", i), Value::String("x".repeat(per)));
    }
    e
}

fn entity_n_fields(fields: usize, per_field: usize) -> Entity {
    let mut e = Entity::new();
    for i in 0..fields {
        e.insert(format!("f{}", i), Value::String("x".repeat(per_field)));
    }
    e
}

fn measure(dir: &PathBuf, n: usize, e: Entity) -> u128 {
    let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.put("e00000000", &e).unwrap();
    }
    store.flush().unwrap();
    let mut col = store.collection("users").unwrap();
    // Warmup (kein Timing)
    for _ in 0..100 {
        let _ = col.get("e00000000").unwrap();
    }
    let (_, us) = timed(|| {
        for _ in 0..n {
            let _ = col.get("e00000000").unwrap();
        }
    });
    us
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5_000);
    let payload: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4_000);

    println!(
        "=== v0.9b Bytes/Decoding-Kausalitaet (n={}, base-payload={}B) ===",
        n, payload
    );

    let dir = tmp_dir();
    let _ = std::fs::remove_dir_all(&dir);

    // S1: ein großes String-Feld
    {
        let us = measure(&dir, n, entity_one_big(payload));
        println!(
            "[S1] 1 Feld, {}B         : {:>10} us | {:>7.1} us/op",
            payload,
            us,
            us as f64 / n as f64
        );
    }
    // S2: viele kleine Felder, gleiche Payload
    {
        let fields = (payload / 40).max(1);
        let us = measure(&dir, n, entity_many_small(payload));
        println!(
            "[S2] {} Felder, {}B   : {:>10} us | {:>7.1} us/op",
            fields,
            payload,
            us,
            us as f64 / n as f64
        );
    }
    // S3a: festes 8-Feld-Schema, klein
    {
        let small = payload / 20;
        let us = measure(&dir, n, entity_fixed_schema(small));
        println!(
            "[S3a] 8 Felder, {}B      : {:>10} us | {:>7.1} us/op",
            small,
            us,
            us as f64 / n as f64
        );
    }
    // S3b: festes 8-Feld-Schema, groß
    {
        let us = measure(&dir, n, entity_fixed_schema(payload));
        println!(
            "[S3b] 8 Felder, {}B      : {:>10} us | {:>7.1} us/op",
            payload,
            us,
            us as f64 / n as f64
        );
    }
    // S4: reine Feldanzahl (konstante Feldgroesse 30B)
    for fields in [1usize, 8, 32, 128] {
        let us = measure(&dir, n, entity_n_fields(fields, 30));
        println!(
            "[S4] {} Felder a 30B     : {:>10} us | {:>7.1} us/op (gesamt {}B)",
            fields,
            us,
            us as f64 / n as f64,
            fields * 30
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    println!("\n[v0.9b done — keine Produktionsaenderung]");
}
