//! bench-baseline — feature-freier Basis-Benchmark für den Produktionspfad.
//!
//! Vier getrennte Blöcke: Put, Flush, Compaction, Read. Keine Counter im
//! Produktionscode, keine Optimierung, keine Feature. Misst nur End-to-End-Zeit
//! + strukturelle Größen (Segmente/Tables) über die öffentliche API.
//!
//! Build: cargo run --release --example bench-baseline -- <n>
//!   n: Anzahl Writes pro Stufe (default 10000)

use std::path::PathBuf;
use std::time::Instant;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::{Database, Options};

fn tmp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lsm-base-{}-{}.cur", tag, std::process::id()));
    p
}

fn timed<F: FnOnce() -> R, R>(f: F) -> (R, u128) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_micros())
}

/// Misst Put isoliert (eine große MemTable, flush nur am Ende).
fn bench_put(n: usize) {
    let dir = tmp_dir("put");
    let mut db = Database::open_with(&dir, Options::default()).unwrap();
    // Warmup
    for i in 0..(n / 10) {
        db.put(format!("k{:08}", i).as_bytes(), b"v").unwrap();
    }
    db.flush().unwrap();
    let ((), us) = timed(|| {
        for i in 0..n {
            db.put(format!("p{:08}", i).as_bytes(), b"value").unwrap();
        }
    });
    let per = us as f64 / n as f64;
    println!(
        "[PUT]      n={:<6} : {:>9.1} us/op   ({} writes)",
        n, per, n
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Misst Flush separat: K Writes, dann expliziter Flush, wiederholt.
fn bench_flush(n: usize) {
    let dir = tmp_dir("flush");
    let mut db = Database::open_with(&dir, Options::default()).unwrap();
    let k = (n / 5).max(100);
    let rounds = 5;
    let mut flush_us = Vec::new();
    let mut seg_after = 0;
    let mut compacted = false;
    for _ in 0..rounds {
        // K writes (MemTable full → flush intern), dann expliziter flush
        for i in 0..k {
            db.put(format!("f{:08}", i).as_bytes(), b"v").unwrap();
        }
        let ((), us) = timed(|| db.flush().unwrap());
        flush_us.push(us);
        seg_after = db.segment_count();
        if db.table_count() <= 3 {
            compacted = true;
        }
    }
    flush_us.sort_unstable();
    let med = flush_us[flush_us.len() / 2];
    println!(
        "[FLUSH]    k={:<6} rounds={} : median {:>9.1} us/flush  segments={} compaction={}",
        k, rounds, med as f64, seg_after, if compacted { "yes" } else { "no" }
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Misst Compaction ISOLIERT über den impliziten Pfad (keine private API):
/// Phase A baut einen definierten L1-Zustand mit ~target_segs Segmenten auf
/// (nicht gemessen). Phase B schreibt einen Überschreib-Batch über den GESAMTEN
/// Key-Range und misst genau den einen `flush()`, der die Overlap-Compaction
/// über alle Segmente auslöst. Flush-Basiskosten sind klein (wenige Records)
/// und bekannt; der Rest ist die Compaction. Vor/nach Segmentanzahl auslesen.
fn bench_compaction_isolated() {
    // Kleine Segmente + niedriger Threshold, damit wenige Writes echte
    // Multi-Segment-Strukturen erzeugen (Default 30k Records pro Segment würde
    // alles in ein einziges Segment legen — das Setup-Fehlschluss-Problem).
    let mut opts = Options::default();
    opts.l0_compact_threshold = 2;
    opts.segment_max_records = 8;
    for target_segs in [5usize, 10, 25, 50] {
        let dir = tmp_dir(&format!("compiso{}", target_segs));
        let mut db = Database::open_with(&dir, opts.clone()).unwrap();

        // Phase A — Setup, nicht gemessen: pro Paar aus 2 Flushes entstehen
        // 2 disjunkte Segmente (je 8 Records, kein Overlap untereinander).
        let recs_per_seg = opts.segment_max_records as u32; // 8
        let pairs = ((target_segs + 1) / 2) as u32; // ceil, damit odd targets ≥ erreichen
        for pair in 0..pairs {
            let base = pair * 2 * recs_per_seg;
            for i in 0..recs_per_seg {
                db.put(format!("k{:08}", base + i).as_bytes(), b"v").unwrap();
            }
            db.flush().unwrap(); // L0=[T1], noch kein Compact (1 < threshold)
            for i in 0..recs_per_seg {
                db.put(format!("k{:08}", base + recs_per_seg + i).as_bytes(), b"v").unwrap();
            }
            db.flush().unwrap(); // L0=[T1,T2] → Compact: 2 neue Segmente
        }
        let segs_after_setup = db.segment_count();
        assert_eq!(segs_after_setup, (2 * pairs) as usize, "Setup-Segmentzahl");

        // Phase B-Setup, nicht gemessen: Überschreib-Batch, dessen Key-Span den
        // GESAMTEN L1-Range abdeckt → der nächste Compact mergt alle Segmente.
        let total_keys = pairs * 2 * recs_per_seg;
        let spread: Vec<String> = (0..total_keys)
            .step_by(recs_per_seg as usize)
            .map(|i| format!("k{:08}", i))
            .collect();
        for k in &spread {
            db.put(k.as_bytes(), b"new").unwrap();
        }
        db.flush().unwrap(); // L0=[T1], noch kein Compact (1 < threshold)
        let segs_before = db.segment_count();
        for k in &spread {
            db.put(k.as_bytes(), b"new").unwrap();
        }

        // Phase B — nur der eine flush(), der den Compact auslöst, wird gemessen.
        let ((), us) = timed(|| db.flush().unwrap());
        let segs_after = db.segment_count();
        let per_seg_ms = if segs_before > 0 {
            (us as f64 / 1000.0) / segs_before as f64
        } else {
            0.0
        };
        println!(
            "[COMPACT-ISO] segs_before={:<3} : {:>9.1} us  ({} ms/seg)  segs_after={}",
            segs_before, us as f64, per_seg_ms, segs_after
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Forensik eines Overlap-Compacts in Phasen, OHNE Produktionscode zu
/// instrumentieren. Für einen definierten Zustand (segs Segmente à
/// recs_per_seg Records) wird gemessen:
///
///   base_flush      — eine SSTable schreiben + Manifest fsync (Referenz, frische DB)
///   merge_read      — voller `scan()` = Merge/LWW/Read über alle Quellen (Proxy für
///                     merge_ids: liest dieselben Quellen, nur über öffentliche API)
///   compact_total   — der eine `flush()`, der die Overlap-Compaction auslöst
///   output_manifest_cleanup — compact_total − merge_read − base_flush (Rest)
///
/// Interpretation: merge_read ≈ Segment-Read + In-Memory-Merge; der Rest ≈
/// Schreiben neuer Segmente + Manifest/sync + Cleanup.
fn bench_compaction_forensic(segs: usize, recs_per_seg: u32) {
    let mut opts = Options::default();
    opts.l0_compact_threshold = 2;
    opts.segment_max_records = recs_per_seg as usize;
    let pairs = ((segs + 1) / 2) as u32;
    let recs = recs_per_seg;

    // Base-Flush: eine Table schreiben + Manifest auf einer frischen DB.
    let base_dir = tmp_dir(&format!("base-{}-{}", segs, recs));
    let mut base_db = Database::open_with(&base_dir, opts.clone()).unwrap();
    for i in 0..recs {
        base_db.put(format!("b{:08}", i).as_bytes(), b"v").unwrap();
    }
    let (_, base_us) = timed(|| base_db.flush().unwrap());
    drop(base_db);
    let _ = std::fs::remove_dir_all(&base_dir);

    // Zustand aufbauen: disjunkte Segmente über Paare aus 2 Flushes.
    let dir = tmp_dir(&format!("f-{}-{}", segs, recs));
    let mut db = Database::open_with(&dir, opts.clone()).unwrap();
    for pair in 0..pairs {
        let base = pair * 2 * recs;
        for i in 0..recs {
            db.put(format!("k{:08}", base + i).as_bytes(), b"v").unwrap();
        }
        db.flush().unwrap();
        for i in 0..recs {
            db.put(format!("k{:08}", base + recs + i).as_bytes(), b"v").unwrap();
        }
        db.flush().unwrap(); // L0=[T1,T2] → Compact: 2 neue Segmente
    }

    // Phase B: Spread-Batch über den GESAMTEN Key-Range → alle Segmente overlap.
    let total_keys = pairs * 2 * recs;
    let spread: Vec<String> = (0..total_keys)
        .step_by(recs as usize)
        .map(|i| format!("k{:08}", i))
        .collect();
    for k in &spread {
        db.put(k.as_bytes(), b"new").unwrap();
    }
    db.flush().unwrap(); // L0=[T1], noch kein Compact
    for k in &spread {
        db.put(k.as_bytes(), b"new").unwrap();
    }

    // Merge/Read-Proxy: voller scan über alle Quellen (MemTable + L0 + L1).
    let (rows, merge_us) = timed(|| db.scan(None, None).unwrap());
    let rows_in = rows.len();
    let segs_before = db.segment_count();

    // compact_total: der eine flush, der den Compact auslöst.
    let ((), comp_us) = timed(|| db.flush().unwrap());
    let segs_after = db.segment_count();
    let rest_us = (comp_us as i64 - merge_us as i64 - base_us as i64).max(0) as u128;

    println!(
        "[FORENSIC] segs={:<3} rec/seg={:<5} rows={:<7} : total {:>9.1} us | merge/read {:>9.1} us | output+man+cleanup {:>9.1} us (base_flush {:>6.1} us) | segs {}→{}",
        segs,
        recs,
        rows_in,
        comp_us as f64,
        merge_us as f64,
        rest_us as f64,
        base_us as f64,
        segs_before,
        segs_after
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Unabhängige Kontrollmessung zur Compaction-Forensik: Wie viel kostet
/// `sync_all()` pro Datei auf DIESEM Filesystem, getrennt von der Compaction?
///
/// Für N gleich große Dateien (Größe wie eine kleine SSTable, gleicher
/// Temp-Dir wie die DBs des Benchmarks): jeweils `write+flush` (ohne fsync)
/// vs `write+flush+sync_all` (mit fsync). Mehrere Wiederholungen → Median,
/// weil fsync-Latenzen auf Windows schwanken.
fn bench_fsync() {
    const ROUNDS: usize = 7;
    let size = 8 * 1024; // ~8 KB pro Datei, Größenordnung einer kleinen SSTable
    let data = vec![0xABu8; size];
    println!(
        "[FSYNC]     {} Rounds, {} KB/Datei, gleiches Filesystem (Temp-Dir) wie SSTables",
        ROUNDS,
        size / 1024
    );
    for n in [1usize, 10, 25, 50] {
        let dir = tmp_dir(&format!("fsync{}", n));
        std::fs::create_dir_all(&dir).unwrap();

        // write + flush (ohne sync_all): Werte sammeln
        let mut no_sync = Vec::new();
        for _ in 0..ROUNDS {
            let ((), us) = timed(|| {
                for i in 0..n {
                    let mut f = std::fs::File::create(dir.join(format!("{:04}.bin", i))).unwrap();
                    std::io::Write::write_all(&mut f, &data).unwrap();
                    std::io::Write::flush(&mut f).unwrap();
                }
            });
            no_sync.push(us);
        }

        // write + flush + sync_all
        let mut with_sync = Vec::new();
        for _ in 0..ROUNDS {
            let ((), us) = timed(|| {
                for i in 0..n {
                    let mut f = std::fs::File::create(dir.join(format!("{:04}.bin", i))).unwrap();
                    std::io::Write::write_all(&mut f, &data).unwrap();
                    std::io::Write::flush(&mut f).unwrap();
                    f.sync_all().unwrap();
                }
            });
            with_sync.push(us);
        }

        no_sync.sort_unstable();
        with_sync.sort_unstable();
        let med_no = no_sync[no_sync.len() / 2];
        let med_yes = with_sync[with_sync.len() / 2];
        let per_file = (med_yes - med_no) as f64 / n as f64;
        println!(
            "[FSYNC]     n={:<3} : write+flush {:>8.1} us | write+flush+sync_all {:>9.1} us  ({} us/file sync-Delta)",
            n,
            med_no as f64,
            med_yes as f64,
            per_file
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Realistisches Entity (4 Felder, wie v0.10-Befund: 3-4 Felder/Entity).
fn make_entity(seed: u64) -> Entity {
    let mut e = Entity::new();
    e.insert("name", Value::String(format!("user-{:08}", seed)));
    e.insert("age", Value::Int((seed % 90 + 10) as i64));
    e.insert("country", Value::String(format!("DE-{}", seed % 4)));
    e.insert("active", Value::Bool(seed % 2 == 0));
    e
}

/// Real-Workload-Gate (§30.6): Erzeugt der tatsächliche Workload genügend
/// kleine Segmente, dass die fsync-Kosten (§30.3-30.5) wirtschaftlich relevant
/// werden?
///
/// Szenario: 10k/50k/100k Entities (4 Felder + Index), Production-Default-
/// Optionen (memtable_limit 4 MB, segment_max_records 30k, threshold 4), dann
/// warme Updates (Hot-Set re-put) + Close. Erfasst die tatsächlich entstehende
/// Segment-/Table-Struktur. Kein Produktionscode, keine Optimierung.
fn bench_entity_gate() {
    println!("[GATE]      Entity-Workload mit Production-Defaults (4 Felder + Index auf age)");
    for size in [10_000usize, 50_000, 100_000] {
        let dir = tmp_dir(&format!("gate{}", size));
        let mut store = EntityStore::open_with(&dir, Options::default()).unwrap();
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();

        // Kalter Aufbau (Setup, nicht das Messziel hier).
        let (_, build_us) = timed(|| {
            for i in 0..size {
                let e = make_entity(i as u64);
                col.put(&format!("e{:08}", i), &e).unwrap();
            }
        });

        // Warme Updates: Hot-Set (10%) mehrfach re-put (reale Produktionslast).
        let hot_n = size / 10;
        let (_, warm_us) = timed(|| {
            for _ in 0..5 {
                for i in 0..hot_n {
                    let e = make_entity(i as u64);
                    col.put(&format!("e{:08}", i), &e).unwrap();
                }
            }
        });

        drop(col);
        store.flush().unwrap();
        let (tables_before_close, segs_before_close, l0) = {
            store.flush().unwrap();
            store.close().unwrap();
            // Nach Close: Struktur über eine frische Database-View erheben.
let db = Database::open_with(&dir, Options::default()).unwrap();
            let t = db.table_count();
            let s = db.segment_count();
            let l0 = db.level_tables(0);
            drop(db);
            (t, s, l0)
        };
        println!(
            "[GATE]      n={:<6} : build {:>8.1} s | warm {:>7.1} s | tables={} segments={} L0={}",
            size,
            build_us as f64 / 1e6,
            warm_us as f64 / 1e6,
            tables_before_close,
            segs_before_close,
            l0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Misst Read: get (existing/missing) + scan bei kleinem und großem Bestand.
fn bench_read(n: usize) {
    for scale in [1000usize, n] {
        let dir = tmp_dir(&format!("read{}", scale));
        let mut db = Database::open_with(&dir, Options::default()).unwrap();
        for i in 0..scale {
            db.put(format!("r{:08}", i).as_bytes(), b"value").unwrap();
        }
        db.flush().unwrap();

        let (_, g1) = timed(|| db.get(b"r00000100").unwrap());
        let (_, g2) = timed(|| db.get(b"missing-key").unwrap());
        let (_, s1) = timed(|| {
            let _ = db.scan(None, None).unwrap();
        });
        println!(
            "[READ]     scale={:<6} : get_exist {:>7.1} us | get_miss {:>7.1} us | fullscan {:>9.1} us",
            scale,
            g1 as f64,
            g2 as f64,
            s1 as f64 / scale as f64
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let only: Option<&str> = args.get(2).map(|s| s.as_str());

    if only == Some("gate") {
        println!("=== bench-baseline (feature-frei) — nur Real-Workload-Gate ===");
        bench_entity_gate();
        println!("\n=== Ende. Kein Produktionscode geändert. ===");
        return;
    }

    println!("=== bench-baseline (feature-frei) n={} ===", n);
    println!("(alle Blöcke separat, keine vermischten Kosten)");
    println!();

    for size in [1_000usize, 10_000, 50_000] {
        let m = if size > n { n } else { size };
        bench_put(m);
        bench_flush(m);
        bench_read(m);
        println!();
    }

    println!("--- isolated compaction ---");
    bench_compaction_isolated();
    println!();

    println!("--- compaction forensic: segments x records/segment ---");
    println!("(base_flush = 1 Table schreiben+Manifest; merge/read = voller scan; Rest = Output+Manifest+Cleanup)");
    for segs in [5usize, 10, 25, 50] {
        for recs in [8u32, 80, 800] {
            bench_compaction_forensic(segs, recs);
        }
        println!();
    }
    println!();

    println!("--- fsync control measurement ---");
    bench_fsync();
    println!();

    println!("--- real workload gate (entities x warm updates) ---");
    bench_entity_gate();
    println!();

    println!("=== Ende. Kein Produktionscode geändert. ===");
}
