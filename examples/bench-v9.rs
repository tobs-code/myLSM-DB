//! v0.9 Profiling-Sprint (nur Messung, KEIN Storage-Touch, KEIN Commit).
//!
//! Reines Sampling ueber die oeffentliche EntityStore/Datenbank-API.
//! Keine Counter im Storage-Code. Jeder Block wird isoliert getaktet und als
//! Anteil an der Gesamtarbeit ausgewiesen, damit ein dominanter Hotspot
//! (>= ~20-30%) von mehreren vergleichbaren Bloecken unterscheidbar ist.
//!
//! Aufruf: cargo run --release --example bench-v9 -- <n> [mix-write-frac]
//!
//! Ebene 1: Warm-Write  (put Gesamt + Teilaufteilung)
//! Ebene 2: Read         (get, full_scan, kv_range, index_eq, index_range)
//! Ebene 3: Workload-Mix (write-dominant / read-dominant / mixed / local vs global)

use std::path::PathBuf;
use std::time::Instant;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::index::FindOp;
use my_lsm_db::Database;
use my_lsm_db::Options;

fn tmp_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lsm-v9-{}.cur", std::process::id()));
    p
}

fn make_entity(fields: usize, seed: u64) -> Entity {
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

/// Ebene 1: Warm-Write.
/// Aufteilung ueber Phasen (jede Phase = frischer Store, damit die Klasse
/// isoliert traegt):
///  - encode: Entity-Konstruktion + Feld-Inserts
///  - put: Gesamt inkl. WAL + MemTable + Index-Schreiben
///  - flush: MemTable -> SST (1x am Ende erzwungen)
fn profile_warm_write(dir: &PathBuf, n: usize) {
    println!("\n=== Ebene 1: Warm-Write (n={}) ===", n);

    // (a) reines Encoding
    let enc_us: u128 = {
        let mut total = 0u128;
        for i in 0..n {
            let (_, us) = timed(|| make_entity(8, i as u64));
            total += us;
        }
        total
    };
    // (b) put Gesamt inkl. WAL + MemTable + Index
    let put_total_us = {
        let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
        let mut col = store.collection("users").unwrap();
        col.create_index("f0").unwrap();
        let mut total = 0u128;
        for i in 0..n {
            let e = make_entity(8, i as u64);
            let (_, us) = timed(|| col.put(&format!("e{:08}", i), &e).unwrap());
            total += us;
        }
        total
    };
    // (c) flush isoliert
    let flush_us = {
        let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
        let mut col = store.collection("users").unwrap();
        for i in 0..n {
            let e = make_entity(8, i as u64);
            col.put(&format!("e{:08}", i), &e).unwrap();
        }
        let (_, us) = timed(|| store.flush().unwrap());
        us
    };

    let per = put_total_us as f64 / n as f64;
    println!(
        "  put gesamt        : {:>10} us | {:>8.1} us/op",
        put_total_us, per
    );
    println!(
        "  davon encode      : {:>10} us | {:>8.1} us/op ({:4.1}%)",
        enc_us,
        enc_us as f64 / n as f64,
        pct(enc_us, put_total_us)
    );
    println!(
        "  flush (1x)        : {:>10} us | {:>8.1} us/op (auf Gesamt {:4.1}%)",
        flush_us,
        flush_us as f64 / n as f64,
        pct(flush_us, put_total_us)
    );
    println!(
        "  => Rest (WAL+MemT+Index): {:>10} us | {:>8.1} us/op ({:4.1}%)",
        put_total_us - enc_us,
        (put_total_us - enc_us) as f64 / n as f64,
        pct(put_total_us - enc_us, put_total_us)
    );
}

/// Ebene 2: Read.
/// DB mit n Entities + indiziertem Feld aufbauen, dann Bloecke isoliert takten.
fn profile_read(dir: &PathBuf, n: usize) {
    println!("\n=== Ebene 2: Read ===");
    {
        let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
        let mut col = store.collection("users").unwrap();
        col.create_index("f0").unwrap();
        for i in 0..n {
            let e = make_entity(8, i as u64);
            col.put(&format!("e{:08}", i), &e).unwrap();
        }
        store.flush().unwrap();
    }

    // Scope A: Entity-Ebene (col leiht store)
    let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
    let mut col = store.collection("users").unwrap();

    let (_, get_us) = timed(|| {
        for i in 0..n {
            let _ = col.get(&format!("e{:08}", i)).unwrap();
        }
    });
    // (b) full_scan (ganze Collection ueber EntityStore)
    let full_us = {
        let mut s = EntityStore::open_with(dir, Options::default()).unwrap();
        let (_, us) = timed(|| {
            let _ = s.scan_collection("users").unwrap();
        });
        us
    };
    let (_, eq_us) = timed(|| {
        for i in 0..1000 {
            let _ = col
                .find("f0", FindOp::Eq(Value::Int((i % 1000) as i64)))
                .unwrap();
        }
    });
    let (_, rng_us) = timed(|| {
        for i in 0..100 {
            let lo = (i * (n / 100)) as i64;
            let hi = ((i + 1) * (n / 100)) as i64;
            let _ = col
                .find("f0", FindOp::Between(Value::Int(lo), Value::Int(hi)))
                .unwrap();
        }
    });
    drop(col);

    // Scope B: roher KV-Range ueber Database (echte Bereichs-Suche)
    let mut db = Database::open_with(dir, Options::default()).unwrap();
    let (_, kv_us) = timed(|| {
        for i in 0..100 {
            let start = format!("e{:08}", i * (n / 100));
            let end = format!("e{:08}", (i + 1) * (n / 100));
            let mut s = db
                .scan_stream(Some(start.as_bytes()), Some(end.as_bytes()))
                .unwrap();
            while s.next().is_some() {}
        }
    });
    drop(db);
    drop(store);

    let blocks: &[(&str, u128)] = &[
        ("get", get_us),
        ("full_scan", full_us),
        ("index_eq", eq_us),
        ("index_range", rng_us),
        ("kv_range", kv_us),
    ];
    let sum: u128 = blocks.iter().map(|(_, u)| u).sum();
    println!("  Blockaufteilung (us):");
    for (name, us) in blocks {
        println!(
            "    {:<12} {:>10} us | {:>6.2}%",
            name,
            us,
            if sum > 0 { *us as f64 / sum as f64 * 100.0 } else { 0.0 }
        );
    }
    println!("  Read-Gesamt (Summe Blöcke): {} us", sum);
}

/// Ebene 3: Workload-Mix.
fn profile_mix(dir: &PathBuf, n: usize, w_frac: f64) {
    println!(
        "\n=== Ebene 3: Workload-Mix (n={}, write-frac={:.2}) ===",
        n, w_frac
    );
    let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
    let mut col = store.collection("users").unwrap();
    col.create_index("f0").unwrap();
    let w = (n as f64 * w_frac) as usize;
    let r = n - w;
    let (_, total_us) = timed(|| {
        let mut wi = 0usize;
        let mut ri = 0usize;
        for _ in 0..n {
            if wi < w && (ri >= r || (wi + ri) % 100 < (w_frac * 100.0) as usize) {
                let e = make_entity(8, wi as u64);
                col.put(&format!("e{:08}", wi % n), &e).unwrap();
                wi += 1;
            } else {
                let _ = col.get(&format!("e{:08}", ri % n)).unwrap();
                ri += 1;
            }
        }
    });
    println!(
        "  Mix gesamt: {:>10} us | {:>8.1} us/op | write-ops={} read-ops={}",
        total_us,
        total_us as f64 / n as f64,
        w,
        r
    );
}

/// Ebene 3b: local (heisse Range) vs global (gleichmaessig ueber Keyraum).
fn profile_local_global(dir: &PathBuf, n: usize) {
    println!("\n=== Ebene 3b: local vs global (Hot-Range) ===");
    {
        let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
        let mut col = store.collection("users").unwrap();
        for i in 0..n {
            let e = make_entity(8, i as u64);
            col.put(&format!("e{:08}", i), &e).unwrap();
        }
        store.flush().unwrap();
    }
    let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
    let mut col = store.collection("users").unwrap();
    let (_, local_us) = timed(|| {
        for _ in 0..10_000 {
            let _ = col.get(&format!("e{:08}", rand_idx(5, n) as u64)).unwrap();
        }
    });
    let (_, global_us) = timed(|| {
        for _ in 0..10_000 {
            let _ = col.get(&format!("e{:08}", rand_idx(100, n) as u64)).unwrap();
        }
    });
    println!(
        "  local  (Hot 5%): {:>10} us | {:.1} us/op",
        local_us,
        local_us as f64 / 10_000.0
    );
    println!(
        "  global (ganzer Key): {:>10} us | {:.1} us/op",
        global_us,
        global_us as f64 / 10_000.0
    );
}

fn pct(part: u128, whole: u128) -> f64 {
    if whole > 0 {
        part as f64 / whole as f64 * 100.0
    } else {
        0.0
    }
}

/// Deterministischer Pseudo-Index in [0, frac% von n).
fn rand_idx(frac: u32, n: usize) -> usize {
    use std::cell::Cell;
    thread_local! {
        static S: Cell<u64> = Cell::new(0x9E3779B97F4A7C15);
    }
    let v = S.with(|s| {
        let x = s.get();
        let nxt = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        s.set(nxt);
        nxt
    });
    ((v % (n as u64 * frac as u64 / 100)) as usize).min(n - 1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let w_frac: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.5);

    let dir = tmp_dir();
    let _ = std::fs::remove_dir_all(&dir);

    profile_warm_write(&dir, n);
    profile_read(&dir, n);
    profile_mix(&dir, n, w_frac);
    profile_mix(&dir, n, 0.9);
    profile_mix(&dir, n, 0.1);
    profile_local_global(&dir, n);

    let _ = std::fs::remove_dir_all(&dir);
    println!("\n[v0.9 profiling done — keine Produktionsänderung]");
}
