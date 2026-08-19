//! v0.7 baseline benchmark harness.
//!
//! The harness is intentionally split into isolated workloads so setup time,
//! measurement time, and result sizes stay separable.
use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::index::FindOp;
use my_lsm_db::query::{SortDir, eq};
use my_lsm_db::{Database, Options};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    Get,
    Scan,
    Range,
    IndexEq,
    IndexRange,
    TopK,
    SetupDiag,
    Flush,
    All,
}

fn parse_args() -> (usize, Workload) {
    let mut size = 100_000usize;
    let mut workload = Workload::All;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workload" => {
                let Some(w) = args.next() else {
                    panic!("missing value for --workload");
                };
                workload = match w.as_str() {
                    "get" => Workload::Get,
                    "scan" => Workload::Scan,
                    "range" => Workload::Range,
                    "index-eq" => Workload::IndexEq,
                    "index-range" => Workload::IndexRange,
                    "top-k" => Workload::TopK,
                    "setup-diag" => Workload::SetupDiag,
                    "flush" => Workload::Flush,
                    "all" => Workload::All,
                    other => panic!("unknown workload: {other}"),
                };
            }
            "--size" => {
                let Some(v) = args.next() else {
                    panic!("missing value for --size");
                };
                size = v.parse().expect("invalid --size");
            }
            x if x.chars().all(|c| c.is_ascii_digit()) => {
                size = x.parse().expect("invalid size");
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    (size, workload)
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn dir_size(dir: &Path, filter: fn(&Path) -> bool) -> u64 {
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

fn shuffle_indices(n: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    let mut seed = 0x9E37_79B9u64;
    for i in (1..n).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed as usize) % (i + 1);
        idx.swap(i, j);
    }
    idx
}

fn bench_root(name: &str, n: usize) -> PathBuf {
    std::env::temp_dir().join(format!("lsm_v07_{name}_{}_{}", std::process::id(), n))
}

fn mk_db(dir: &Path) -> Database {
    let _ = fs::remove_dir_all(dir);
    Database::open_with(
        dir,
        Options {
            memtable_limit: 8 * 1024 * 1024,
            l0_compact_threshold: 4,
        },
    )
    .expect("open db")
}

fn make_entity(i: usize) -> Entity {
    let mut e = Entity::new();
    e.insert("age", Value::Int((i % 10_000) as i64));
    e.insert("score", Value::Int(((i as i64) * 7) % 1_000_000));
    e.insert("name", Value::String(format!("user-{i:08}")));
    e.insert("active", Value::Bool(i % 2 == 0));
    e
}

fn populate_collection(col: &mut my_lsm_db::entity::CollectionHandle<'_>, n: usize) -> f64 {
    let t = Instant::now();
    for i in 0..n {
        col.put(&format!("u{i:08}"), &make_entity(i)).expect("put");
    }
    t.elapsed().as_secs_f64()
}

fn setup_raw(dir: &Path, n: usize) -> (Database, Vec<Vec<u8>>, Vec<Vec<u8>>, f64) {
    let mut db = mk_db(dir);
    let keys: Vec<Vec<u8>> = (0..n).map(|i| format!("u{i:08}").into_bytes()).collect();
    let vals: Vec<Vec<u8>> = (0..n).map(|i| format!("value-{i}").into_bytes()).collect();
    let t = Instant::now();
    for i in 0..n {
        db.put(&keys[i], &vals[i]).expect("put");
    }
    let setup = t.elapsed().as_secs_f64();
    (db, keys, vals, setup)
}

fn setup_store(dir: &Path, n: usize) -> (EntityStore, f64) {
    let mut store = EntityStore::open(dir).expect("open store");
    {
        let mut col = store.collection("users").expect("collection");
        col.create_index("age").expect("index age");
        col.create_index("score").expect("index score");
    }
    let t = Instant::now();
    {
        let mut col = store.collection("users").expect("collection");
        let _ = populate_collection(&mut col, n);
    }
    (store, t.elapsed().as_secs_f64())
}

fn setup_diag(size: usize) {
    let base = bench_root("setupdiag", size);
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create dir");
    println!("== v0.7 setup diagnosis: {} entities ==", size);

    // 1) EntityStore ohne Index.
    let no_index = base.join("no_index");
    let mut store = EntityStore::open(&no_index).expect("open store");
    let setup = {
        let mut col = store.collection("users").expect("collection");
        populate_collection(&mut col, size)
    };
    println!("  no-index writes:       {:.3}s", setup);
    store.close().expect("close");
    let _ = fs::remove_dir_all(&no_index);

    // 2) Index vor den Writes.
    let index_before = base.join("index_before");
    let mut store = EntityStore::open(&index_before).expect("open store");
    let (index_create, write_time) = {
        let mut col = store.collection("users").expect("collection");
        let t = Instant::now();
        col.create_index("age").expect("index age");
        col.create_index("score").expect("index score");
        let index_create = t.elapsed().as_secs_f64();
        let write_time = populate_collection(&mut col, size);
        (index_create, write_time)
    };
    println!("  index-before create:   {:.3}s", index_create);
    println!("  index-before writes:   {:.3}s", write_time);
    store.close().expect("close");
    let _ = fs::remove_dir_all(&index_before);

    // 1b) Engine-Write-Vergleich: roher db.put, ohne Entity-Logik.
    let raw_dir = base.join("raw_put");
    let raw_setup = {
        let mut db = mk_db(&raw_dir);
        let t = Instant::now();
        for i in 0..size {
            db.put(
                format!("u{i:08}").as_bytes(),
                format!("value-{i}").as_bytes(),
            )
            .expect("put");
        }
        db.close().expect("close");
        t.elapsed().as_secs_f64()
    };
    println!(
        "  raw db.put writes:     {:.3}s  ({:.1} puts/s)",
        raw_setup,
        size as f64 / raw_setup
    );
    let _ = fs::remove_dir_all(&raw_dir);

    // 3) Index nach den Writes.
    let index_after = base.join("index_after");
    let mut store = EntityStore::open(&index_after).expect("open store");
    let write_time = {
        let mut col = store.collection("users").expect("collection");
        populate_collection(&mut col, size)
    };
    let create_time = {
        let mut col = store.collection("users").expect("collection");
        let t = Instant::now();
        col.create_index("age").expect("index age");
        col.create_index("score").expect("index score");
        t.elapsed().as_secs_f64()
    };
    println!("  index-after writes:    {:.3}s", write_time);
    println!("  index-after create:    {:.3}s", create_time);
    store.close().expect("close");
    let _ = fs::remove_dir_all(&index_after);

    let _ = fs::remove_dir_all(&base);
}

fn print_sizes(dir: &Path, db: &Database) {
    let wal = dir_size(dir, |p| p.extension().is_some_and(|e| e == "log"));
    let sst = dir_size(dir, |p| p.extension().is_some_and(|e| e == "sst"));
    println!("  db size:      {:.3} MB", mb(dir_size(dir, |_| true)));
    println!("  wal:          {:.3} MB", mb(wal));
    println!(
        "  sstables:     {:.3} MB  ({} tables)",
        mb(sst),
        db.table_count()
    );
}

fn bench_get(db: &mut Database, keys: &[Vec<u8>]) -> (f64, usize) {
    let idx = shuffle_indices(keys.len());
    let t = Instant::now();
    for &i in &idx {
        assert!(db.get(&keys[i]).expect("get").is_some());
    }
    let elapsed = t.elapsed().as_secs_f64();
    (elapsed, keys.len())
}

fn bench_scan(db: &mut Database) -> (f64, usize, u64) {
    let t = Instant::now();
    let scanned = db.scan(None, None).expect("scan");
    let elapsed = t.elapsed().as_secs_f64();
    let scanned_bytes: u64 = scanned
        .iter()
        .map(|(k, v)| (k.len() + v.as_ref().map_or(0, |v| v.len())) as u64)
        .sum();
    (elapsed, scanned.len(), scanned_bytes)
}

fn bench_range(db: &mut Database) -> (f64, usize, u64) {
    let t = Instant::now();
    let scanned = db
        .scan(Some(b"u00001000"), Some(b"u00002000"))
        .expect("range");
    let elapsed = t.elapsed().as_secs_f64();
    let scanned_bytes: u64 = scanned
        .iter()
        .map(|(k, v)| (k.len() + v.as_ref().map_or(0, |v| v.len())) as u64)
        .sum();
    (elapsed, scanned.len(), scanned_bytes)
}

fn bench_index_eq(store: &mut EntityStore) -> (f64, usize) {
    let t = Instant::now();
    let ids = {
        let mut col = store.collection("users").expect("collection");
        col.find("age", FindOp::Eq(Value::Int(42)))
            .expect("index eq")
    };
    (t.elapsed().as_secs_f64(), ids.len())
}

fn bench_index_range(store: &mut EntityStore) -> (f64, usize) {
    let t = Instant::now();
    let ids = {
        let mut col = store.collection("users").expect("collection");
        col.find("age", FindOp::Between(Value::Int(100), Value::Int(5_000)))
            .expect("index range")
    };
    (t.elapsed().as_secs_f64(), ids.len())
}

fn bench_top_k(store: &mut EntityStore) -> (f64, usize) {
    let t = Instant::now();
    let q = store
        .query("users")
        .expect("query")
        .filter(eq("age", Value::Int(42)))
        .sort("age", SortDir::Asc)
        .limit(50);
    let rows = store.execute_query(q).expect("execute");
    (t.elapsed().as_secs_f64(), rows.len())
}

fn bench_flush(db: &mut Database) -> f64 {
    let t = Instant::now();
    db.flush().expect("flush");
    t.elapsed().as_secs_f64()
}

fn run_single(size: usize, workload: Workload) {
    let raw_dir = bench_root("raw", size);
    let entity_dir = bench_root("entity", size);
    let _ = fs::remove_dir_all(&raw_dir);
    let _ = fs::remove_dir_all(&entity_dir);

    println!("== v0.7 baseline: {} entities, {:?} ==", size, workload);

    match workload {
        Workload::Get | Workload::Scan | Workload::Range | Workload::Flush => {
            let (mut db, keys, _, setup) = setup_raw(&raw_dir, size);
            println!("  setup:         {:.3}s", setup);
            match workload {
                Workload::Get => {
                    let (elapsed, count) = bench_get(&mut db, &keys);
                    println!(
                        "  get(random):   {:.0} reads/s  ({:.3}s, {} rows)",
                        count as f64 / elapsed,
                        elapsed,
                        count
                    );
                }
                Workload::Scan => {
                    let (elapsed, count, bytes) = bench_scan(&mut db);
                    println!(
                        "  scan(full):    {:.3} MB/s   ({count} rows, {} MB)",
                        bytes as f64 / elapsed / (1024.0 * 1024.0),
                        mb(bytes)
                    );
                }
                Workload::Range => {
                    let (elapsed, count, bytes) = bench_range(&mut db);
                    println!(
                        "  scan(range):   {:.3} MB/s   ({count} rows, {} MB)",
                        bytes as f64 / elapsed / (1024.0 * 1024.0),
                        mb(bytes)
                    );
                }
                Workload::Flush => {
                    let flush = bench_flush(&mut db);
                    println!("  flush:         {:.3}s", flush);
                }
                _ => unreachable!(),
            }
            println!("  reopen+recover: {:.3}s", {
                let t = Instant::now();
                db.close().expect("close");
                let _ = Database::open(&raw_dir).expect("reopen");
                t.elapsed().as_secs_f64()
            });
            print_sizes(&raw_dir, &db);
        }
        Workload::IndexEq | Workload::IndexRange | Workload::TopK => {
            let (mut store, setup) = setup_store(&entity_dir, size);
            println!("  setup:         {:.3}s", setup);
            match workload {
                Workload::IndexEq => {
                    let (elapsed, count) = bench_index_eq(&mut store);
                    println!(
                        "  index eq:      {:.0} lookups/s  ({:.3}s, {} hits)",
                        if elapsed > 0.0 {
                            count as f64 / elapsed
                        } else {
                            0.0
                        },
                        elapsed,
                        count
                    );
                }
                Workload::IndexRange => {
                    let (elapsed, count) = bench_index_range(&mut store);
                    println!(
                        "  index range:   {:.0} lookups/s  ({:.3}s, {} hits)",
                        if elapsed > 0.0 {
                            count as f64 / elapsed
                        } else {
                            0.0
                        },
                        elapsed,
                        count
                    );
                }
                Workload::TopK => {
                    let (elapsed, count) = bench_top_k(&mut store);
                    println!(
                        "  order+limit:   {:.0} rows/s  ({:.3}s, {} rows)",
                        if elapsed > 0.0 {
                            count as f64 / elapsed
                        } else {
                            0.0
                        },
                        elapsed,
                        count
                    );
                }
                _ => unreachable!(),
            }
            store.close().expect("close");
            let _ = fs::remove_dir_all(&entity_dir);
        }
        Workload::SetupDiag => {
            setup_diag(size);
        }
        Workload::All => {
            for w in [
                Workload::Get,
                Workload::Scan,
                Workload::Range,
                Workload::IndexEq,
                Workload::IndexRange,
                Workload::TopK,
                Workload::SetupDiag,
                Workload::Flush,
            ] {
                run_single(size, w);
            }
        }
    }
    let _ = fs::remove_dir_all(&raw_dir);
    let _ = fs::remove_dir_all(&entity_dir);
}

fn main() {
    let (size, workload) = parse_args();
    run_single(size, workload);
}
