//! v0.9a Read-Path-Profiling (nur Messung, KEIN Storage-Touch, KEIN Commit).
//!
//! Zerlegt den erfolgreichen `get` ueber differentielle Messungen an der
//! oeffentlichen API. Keine internen Counter, kein Cache, kein neues Format.
//! Die internen Stufen (MemTable / SSTable-Auswahl / Key-Suche / I/O /
//! Decoding) werden ueber Verhaltens-Differenziale eingegrenzt:
//!
//!  - wiederholter selber Key (kleiner WS) vs. viele Keys (großer WS)
//!    => I/O-Sensitivitaet (Cache wirksam?).
//!  - Hit vs. Miss                                                        => Bloom-Relevanz.
//!  - 1 SSTable vs. viele SSTables (pro Put flush)                       => Key-Suche/Auswahl.
//!  - kleine vs. grosse Entity                                            => Decoding-Anteil.
//!  - MemTable-Hit (frisch, kein flush) vs. SSTable-Pfad (nach flush)     => MemTable-Stufe.
//!
//! Aufruf: cargo run --release --example bench-v9a -- <n>

use std::path::PathBuf;
use std::time::Instant;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::Options;

fn tmp_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lsm-v9a-{}.cur", std::process::id()));
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

/// Baut n Entities in einem Store. Wenn `flush_each` gesetzt, wird nach jedem
/// Put geflusht -> erzeugt n L0-SSTables (Key-Suche ueber viele Tabellen).
fn build(dir: &PathBuf, n: usize, fields: usize, flush_each: bool) {
    let mut store = EntityStore::open_with(dir, Options::default()).unwrap();
    for i in 0..n {
        {
            let mut col = store.collection("users").unwrap();
            let e = make_entity(fields, i as u64);
            col.put(&format!("e{:08}", i), &e).unwrap();
        } // col dropped -> store allein
        if flush_each {
            store.flush().unwrap();
        }
    }
    if !flush_each {
        store.flush().unwrap();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(20_000);

    let dir = tmp_dir();
    let _ = std::fs::remove_dir_all(&dir);

    println!("=== v0.9a Read-Path-Profiling (n={}) ===", n);

    // ---------------------------------------------------------------
    // Fall A: MemTable-Hit (frisch, KEIN flush)
    // ---------------------------------------------------------------
    {
        let mut store = EntityStore::open_with(&dir, Options::default()).unwrap();
        let mut col = store.collection("users").unwrap();
        for i in 0..n {
            let e = make_entity(8, i as u64);
            col.put(&format!("e{:08}", i), &e).unwrap();
        }
        // kein flush -> alle in MemTable
        let (_, us) = timed(|| {
            for i in 0..n {
                let _ = col.get(&format!("e{:08}", i)).unwrap();
            }
        });
        println!(
            "[A] MemTable-Pfad (kein flush): {:>10} us | {:>6.1} us/op",
            us,
            us as f64 / n as f64
        );
    }

    // ---------------------------------------------------------------
    // Fall B: SSTable-Pfad, 1 Tabelle, wiederholter Key (kleiner WS)
    // ---------------------------------------------------------------
    build(&dir, n, 8, false);
    {
        let mut store = EntityStore::open_with(&dir, Options::default()).unwrap();
        let mut col = store.collection("users").unwrap();
        let (_, us) = timed(|| {
            for _ in 0..n {
                // immer derselbe Key -> max. Locality/Cache
                let _ = col.get(&format!("e{:08}", 0)).unwrap();
            }
        });
        println!(
            "[B] SSTable, repeat-1-key (WS=1): {:>10} us | {:>6.1} us/op",
            us,
            us as f64 / n as f64
        );
    }

    // ---------------------------------------------------------------
    // Fall C: SSTable-Pfad, 1 Tabelle, viele verschiedene Keys (großer WS)
    // ---------------------------------------------------------------
    build(&dir, n, 8, false);
    {
        let mut store = EntityStore::open_with(&dir, Options::default()).unwrap();
        let mut col = store.collection("users").unwrap();
        let (_, us) = timed(|| {
            for i in 0..n {
                let _ = col.get(&format!("e{:08}", i)).unwrap();
            }
        });
        println!(
            "[C] SSTable, distinct-keys (WS=n): {:>10} us | {:>6.1} us/op",
            us,
            us as f64 / n as f64
        );
    }

    // Vergleich B vs C: wenn C >> B, dann I/O-sensitiv (kein wirksamer Cache).
    // Wenn B ~ C, dann ist die Kosten primär unabhaengig von Locality
    // (Decoding / gleichbleibende Suche).

    // ---------------------------------------------------------------
    // Fall D: viele SSTables (pro Put flush) -> Key-Suche ueber viele Tabellen
    // Auf n_tables=500 begrenzt, sonst O(n^2) bei der get-Suche.
    // ---------------------------------------------------------------
    {
        let n_tables = n.min(500);
        let mut store = EntityStore::open_with(&dir, Options::default()).unwrap();
        for i in 0..n_tables {
            {
                let mut col = store.collection("users").unwrap();
                let e = make_entity(8, i as u64);
                col.put(&format!("e{:08}", i), &e).unwrap();
            }
            store.flush().unwrap();
        }
        let mut col = store.collection("users").unwrap();
        let (_, us) = timed(|| {
            for i in 0..n_tables {
                let _ = col.get(&format!("e{:08}", i)).unwrap();
            }
        });
        println!(
            "[D] {} SSTables (pro-put flush): {:>10} us | {:>6.1} us/op",
            n_tables,
            us,
            us as f64 / n_tables as f64
        );
    }
    // Vergleich C vs D: Kostenanstieg proportional zu #Tabellen => Key-Suche/
    // SSTable-Auswahl dominiert.

    // ---------------------------------------------------------------
    // Fall E: Hit vs. Miss (Bloom-Relevanz)
    // ---------------------------------------------------------------
    build(&dir, n, 8, false);
    {
        let mut store = EntityStore::open_with(&dir, Options::default()).unwrap();
        let mut col = store.collection("users").unwrap();
        let (_, hit_us) = timed(|| {
            for i in 0..n {
                let _ = col.get(&format!("e{:08}", i)).unwrap();
            }
        });
        let (_, miss_us) = timed(|| {
            for i in 0..n {
                let _ = col.get(&format!("x{:08}", i)).unwrap(); // existiert nicht
            }
        });
        println!(
            "[E] Hit : {:>10} us | {:>6.1} us/op",
            hit_us,
            hit_us as f64 / n as f64
        );
        println!(
            "[E] Miss: {:>10} us | {:>6.1} us/op",
            miss_us,
            miss_us as f64 / n as f64
        );
    }
    // Wenn Miss >> Hit, dann lohnt Bloom-Filter (vermeidet teure Suche).

    // ---------------------------------------------------------------
    // Fall F: kleine vs. grosse Entity (Decoding-Anteil)
    // ---------------------------------------------------------------
    {
        let mut store = EntityStore::open_with(&dir, Options::default()).unwrap();
        {
            let mut col = store.collection("users").unwrap();
            // klein
            for i in 0..n {
                let e = make_entity(2, i as u64);
                col.put(&format!("e{:08}", i), &e).unwrap();
            }
        }
        store.flush().unwrap();
        let mut col = store.collection("users").unwrap();
        let (_, small_us) = timed(|| {
            for i in 0..n {
                let _ = col.get(&format!("e{:08}", i)).unwrap();
            }
        });
        // gross
        let mut store2 = EntityStore::open_with(&dir, Options::default()).unwrap();
        {
            let mut col2 = store2.collection("users").unwrap();
            for i in 0..n {
                let e = make_entity(64, i as u64);
                col2.put(&format!("e{:08}", i), &e).unwrap();
            }
        }
        store2.flush().unwrap();
        let mut col2 = store2.collection("users").unwrap();
        let (_, big_us) = timed(|| {
            for i in 0..n {
                let _ = col2.get(&format!("e{:08}", i)).unwrap();
            }
        });
        println!(
            "[F] small (2 f): {:>10} us | {:>6.1} us/op",
            small_us,
            small_us as f64 / n as f64
        );
        println!(
            "[F] big  (64 f): {:>10} us | {:>6.1} us/op",
            big_us,
            big_us as f64 / n as f64
        );
    }
    // Wenn big >> small, dann Decoding/Payload dominiert (Field Projection relevant).

    let _ = std::fs::remove_dir_all(&dir);
    println!("\n[v0.9a done — keine Produktionsaenderung]");
}
