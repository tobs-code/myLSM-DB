//! Simpler Benchmark für die v0.1.1-Engine.
//!
//! Aufruf: `cargo run --example bench [n]`
//! misst put / random get / sequential get / scan und berichtet Metriken.
use my_lsm_db::{Database, Options};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn dir_size(dir: &PathBuf, filter: fn(&std::path::Path) -> bool) -> u64 {
    fs::read_dir(dir)
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| filter(&e.path()))
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);

    let dir = std::env::temp_dir().join(format!("lsm_bench_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);

    let opts = Options {
        memtable_limit: 8 * 1024 * 1024,
        l0_compact_threshold: 4,
    };
    let mut db = Database::open_with(&dir, opts).expect("open");

    let keys: Vec<Vec<u8>> = (0..n).map(|i| format!("key-{:08}", i).into_bytes()).collect();
    let vals: Vec<Vec<u8>> = (0..n).map(|i| format!("value-{}", i).into_bytes()).collect();

    println!("== myLSM-DB benchmark: {} keys ==", n);

    // --- put ---
    let t = Instant::now();
    for i in 0..n {
        db.put(&keys[i], &vals[i]).expect("put");
    }
    let put_elapsed = t.elapsed().as_secs_f64();
    println!("put:            {:.0} writes/s  ({:.3}s, {} MB total)",
        n as f64 / put_elapsed, put_elapsed, mb(dir_size(&dir, |_| true)));

    // --- flush erzwingen + Zeit ---
    let t = Instant::now();
    db.flush().expect("flush");
    println!("flush:          {:.3}s", t.elapsed().as_secs_f64());

    // --- random get ---
    let mut idx: Vec<usize> = (0..n).collect();
    let mut seed = 0x9E37_79B9u64;
    // einfacher xorshift zum Mischen
    for i in (1..n).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed as usize) % (i + 1);
        idx.swap(i, j);
    }
    let t = Instant::now();
    for &i in &idx {
        assert!(db.get(&keys[i]).expect("get").is_some());
    }
    let rg = t.elapsed().as_secs_f64();
    println!("random get:     {:.0} reads/s  ({:.3}s)", n as f64 / rg, rg);

    // --- sequential get ---
    let t = Instant::now();
    for i in 0..n {
        assert!(db.get(&keys[i]).expect("get").is_some());
    }
    let sg = t.elapsed().as_secs_f64();
    println!("sequential get: {:.0} reads/s  ({:.3}s)", n as f64 / sg, sg);

    // --- scan (alle Keys) ---
    let t = Instant::now();
    let scanned = db.scan(None, None).expect("scan");
    let scan_elapsed = t.elapsed().as_secs_f64();
    let scanned_bytes: u64 = scanned
        .iter()
        .map(|(k, v)| (k.len() + v.as_ref().map_or(0, |v| v.len())) as u64)
        .sum();
    println!("scan:           {:.3} MB/s   ({} rows, {} MB)",
        scanned_bytes as f64 / scan_elapsed / (1024.0 * 1024.0), scanned.len(), mb(scanned_bytes));

    // --- Datei-Metriken ---
    let wal = dir_size(&dir, |p| p.extension().map_or(false, |e| e == "log"));
    let sst = dir_size(&dir, |p| p.extension().map_or(false, |e| e == "sst"));
    println!("db size:        {:.3} MB", mb(dir_size(&dir, |_| true)));
    println!("  wal:          {:.3} MB", mb(wal));
    println!("  sstables:     {:.3} MB  ({} tables)", mb(sst), db.table_count());

    // --- restart + recovery ---
    let t = Instant::now();
    db.close().expect("close");
    let mut db2 = Database::open(&dir).expect("reopen");
    println!("reopen+recover: {:.3}s", t.elapsed().as_secs_f64());
    // Verifikation nach Reopen
    let ok = (0..n).all(|i| db2.get(&keys[i]).expect("get").is_some());
    println!("recovery check: {}", if ok { "OK" } else { "FAILED" });

    let _ = fs::remove_dir_all(&dir);
}