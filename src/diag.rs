//! Diagnose-Zähler für die Setup-Analyse — **nur** mit `--features bench-diag`.
//!
//! Im normalen Build (ohne das Feature) existiert dieses Modul samt aller
//! Aufrufstellen gar nicht; es gibt also keinerlei Laufzeit-Overhead oder
//! Counter im Release-Pfad.
//!
//! Die Zähler sind globale Atomics, die der Benchmark per `reset`/`snapshot`
//! auswertet. Bewusst keine Locks, keine Threads, nur monotone Akkumulation.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SCANS: AtomicU64 = AtomicU64::new(0);
static TABLES_PER_SCAN: AtomicU64 = AtomicU64::new(0);
static FORKS: AtomicU64 = AtomicU64::new(0);
static TRY_CLONES: AtomicU64 = AtomicU64::new(0);
static CLONE_US: AtomicU64 = AtomicU64::new(0);
static MEMRANGE_US: AtomicU64 = AtomicU64::new(0);
static SSTABLE_ITER_US: AtomicU64 = AtomicU64::new(0);
static FLUSHES: AtomicU64 = AtomicU64::new(0);
static SSTABLES_AFTER_FLUSH: AtomicU64 = AtomicU64::new(0);
static PUT_INTERNAL_US: AtomicU64 = AtomicU64::new(0);
static FLUSH_US: AtomicU64 = AtomicU64::new(0);
static ENTITY_PUT_US: AtomicU64 = AtomicU64::new(0);
static FIELD_ID_US: AtomicU64 = AtomicU64::new(0);
static IDX_ENC_US: AtomicU64 = AtomicU64::new(0);
static IDX_FIELDS_US: AtomicU64 = AtomicU64::new(0);
static SCAN_COLLECT_US: AtomicU64 = AtomicU64::new(0);
static TABLE_BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);
static COMPACT_INPUT_BYTES: AtomicU64 = AtomicU64::new(0);
static COMPACTIONS: AtomicU64 = AtomicU64::new(0);
static COMPACT_US: AtomicU64 = AtomicU64::new(0);
static LIVE_TABLE_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_TABLE_BYTES: AtomicU64 = AtomicU64::new(0);
static WAL_US: AtomicU64 = AtomicU64::new(0);
static MEMTABLE_US: AtomicU64 = AtomicU64::new(0);
static GET_US: AtomicU64 = AtomicU64::new(0);
static PUT_HINT_US: AtomicU64 = AtomicU64::new(0);
static PUT_FIELDENC_US: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

/// Aktiviert die Erfassung (einmalig, z. B. vom Benchmark beim Start).
pub fn enable() {
    ACTIVE.store(true, Ordering::Relaxed);
}

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

pub fn add_scan(tables: usize) {
    SCANS.fetch_add(1, Ordering::Relaxed);
    TABLES_PER_SCAN.fetch_add(tables as u64, Ordering::Relaxed);
}

pub fn add_fork(try_clones: u32, us: u64) {
    FORKS.fetch_add(1, Ordering::Relaxed);
    TRY_CLONES.fetch_add(try_clones as u64, Ordering::Relaxed);
    CLONE_US.fetch_add(us, Ordering::Relaxed);
}

pub fn add_memrange_us(us: u64) {
    MEMRANGE_US.fetch_add(us, Ordering::Relaxed);
}

pub fn add_sstable_iter_us(us: u64) {
    SSTABLE_ITER_US.fetch_add(us, Ordering::Relaxed);
}

pub fn add_flush(sstables_after: usize) {
    FLUSHES.fetch_add(1, Ordering::Relaxed);
    SSTABLES_AFTER_FLUSH.fetch_add(sstables_after as u64, Ordering::Relaxed);
}

pub fn add_put_internal_us(us: u64) {
    PUT_INTERNAL_US.fetch_add(us, Ordering::Relaxed);
}

pub fn add_flush_us(us: u64) {
    FLUSH_US.fetch_add(us, Ordering::Relaxed);
}

pub fn add_entity_put_us(us: u64) {
    ENTITY_PUT_US.fetch_add(us, Ordering::Relaxed);
}

pub fn add_field_id_us(us: u64) {
    FIELD_ID_US.fetch_add(us, Ordering::Relaxed);
}

pub fn add_idx_enc_us(us: u64) {
    IDX_ENC_US.fetch_add(us, Ordering::Relaxed);
}

pub fn add_idx_fields_us(us: u64) {
    IDX_FIELDS_US.fetch_add(us, Ordering::Relaxed);
}

pub fn add_scan_collect_us(us: u64) {
    SCAN_COLLECT_US.fetch_add(us, Ordering::Relaxed);
}

/// Eine neue SSTable wurde auf die Platte geschrieben (Flush ODER Compaction).
/// Zähler für die Write-Amplification-Analyse.
pub fn add_table_written(bytes: u64) {
    TABLE_BYTES_WRITTEN.fetch_add(bytes, Ordering::Relaxed);
    let live = LIVE_TABLE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    let mut peak = PEAK_TABLE_BYTES.load(Ordering::Relaxed);
    while live > peak {
        match PEAK_TABLE_BYTES.compare_exchange_weak(
            peak,
            live,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => peak = actual,
        }
    }
}

/// Eine SSTable wurde bei der Compaction gelöscht (nicht mehr live).
pub fn add_table_removed(bytes: u64) {
    LIVE_TABLE_BYTES.fetch_sub(bytes, Ordering::Relaxed);
}

/// Compaction hat `bytes` an Eingabedaten gelesen (Summe der gemergten Quellen).
pub fn add_compact_input(bytes: u64) {
    COMPACT_INPUT_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// WAL-`append` hat `us` gedauert (ein innerer Write).
pub fn add_wal_us(us: u64) {
    WAL_US.fetch_add(us, Ordering::Relaxed);
}

/// MemTable-Insert hat `us` gedauert (ein innerer Write).
pub fn add_memtable_us(us: u64) {
    MEMTABLE_US.fetch_add(us, Ordering::Relaxed);
}

/// Ein Point-Lookup (`Mutator::get`) hat `us` gedauert.
pub fn add_get_us(us: u64) {
    GET_US.fetch_add(us, Ordering::Relaxed);
}

/// Hint-Bookkeeping (map.get + Feld-Differenz) hat `us` gedauert.
pub fn add_put_hint_us(us: u64) {
    PUT_HINT_US.fetch_add(us, Ordering::Relaxed);
}

/// Feld-Encoding (schema.field_id + codec::encode) hat `us` gedauert.
pub fn add_put_fieldenc_us(us: u64) {
    PUT_FIELDENC_US.fetch_add(us, Ordering::Relaxed);
}

/// Old-Value-Cache-Treffer: Index-Diff konnte ohne Disk-Read gelöst werden.
pub fn add_cache_hit() {
    CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}

/// Old-Value-Cache-Fehlschlag: es wurde der normale Point-Lookup verwendet.
pub fn add_cache_miss() {
    CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
}

/// Eine Compaction abgeschlossen; `us` = verbrauchte Zeit.
pub fn add_compact(us: u64) {
    COMPACTIONS.fetch_add(1, Ordering::Relaxed);
    COMPACT_US.fetch_add(us, Ordering::Relaxed);
}

/// Setzt alle Zähler zurück (für den Anfang einer Messphase).
pub fn reset() {
    SCANS.store(0, Ordering::Relaxed);
    TABLES_PER_SCAN.store(0, Ordering::Relaxed);
    FORKS.store(0, Ordering::Relaxed);
    TRY_CLONES.store(0, Ordering::Relaxed);
    CLONE_US.store(0, Ordering::Relaxed);
    MEMRANGE_US.store(0, Ordering::Relaxed);
    SSTABLE_ITER_US.store(0, Ordering::Relaxed);
    FLUSHES.store(0, Ordering::Relaxed);
    SSTABLES_AFTER_FLUSH.store(0, Ordering::Relaxed);
    PUT_INTERNAL_US.store(0, Ordering::Relaxed);
    FLUSH_US.store(0, Ordering::Relaxed);
    ENTITY_PUT_US.store(0, Ordering::Relaxed);
    FIELD_ID_US.store(0, Ordering::Relaxed);
    IDX_ENC_US.store(0, Ordering::Relaxed);
    IDX_FIELDS_US.store(0, Ordering::Relaxed);
    SCAN_COLLECT_US.store(0, Ordering::Relaxed);
    TABLE_BYTES_WRITTEN.store(0, Ordering::Relaxed);
    COMPACT_INPUT_BYTES.store(0, Ordering::Relaxed);
    COMPACTIONS.store(0, Ordering::Relaxed);
    COMPACT_US.store(0, Ordering::Relaxed);
    LIVE_TABLE_BYTES.store(0, Ordering::Relaxed);
    PEAK_TABLE_BYTES.store(0, Ordering::Relaxed);
    WAL_US.store(0, Ordering::Relaxed);
    MEMTABLE_US.store(0, Ordering::Relaxed);
    GET_US.store(0, Ordering::Relaxed);
    PUT_HINT_US.store(0, Ordering::Relaxed);
    PUT_FIELDENC_US.store(0, Ordering::Relaxed);
    CACHE_HITS.store(0, Ordering::Relaxed);
    CACHE_MISSES.store(0, Ordering::Relaxed);
}

/// Snapshot aller Zähler als `(Name, Wert)`-Paare.
pub fn snapshot() -> Vec<(&'static str, u64)> {
    vec![
        ("scans", SCANS.load(Ordering::Relaxed)),
        ("tables_per_scan", TABLES_PER_SCAN.load(Ordering::Relaxed)),
        ("forks", FORKS.load(Ordering::Relaxed)),
        ("try_clones", TRY_CLONES.load(Ordering::Relaxed)),
        ("clone_us", CLONE_US.load(Ordering::Relaxed)),
        ("memrange_us", MEMRANGE_US.load(Ordering::Relaxed)),
        ("sstable_iter_us", SSTABLE_ITER_US.load(Ordering::Relaxed)),
        ("flushes", FLUSHES.load(Ordering::Relaxed)),
        (
            "sstables_after_flush",
            SSTABLES_AFTER_FLUSH.load(Ordering::Relaxed),
        ),
        ("put_internal_us", PUT_INTERNAL_US.load(Ordering::Relaxed)),
        ("flush_us", FLUSH_US.load(Ordering::Relaxed)),
        ("entity_put_us", ENTITY_PUT_US.load(Ordering::Relaxed)),
        ("field_id_us", FIELD_ID_US.load(Ordering::Relaxed)),
        ("idx_enc_us", IDX_ENC_US.load(Ordering::Relaxed)),
        ("idx_fields_us", IDX_FIELDS_US.load(Ordering::Relaxed)),
        ("scan_collect_us", SCAN_COLLECT_US.load(Ordering::Relaxed)),
        (
            "table_bytes_written",
            TABLE_BYTES_WRITTEN.load(Ordering::Relaxed),
        ),
        (
            "compact_input_bytes",
            COMPACT_INPUT_BYTES.load(Ordering::Relaxed),
        ),
        ("compactions", COMPACTIONS.load(Ordering::Relaxed)),
        ("compact_us", COMPACT_US.load(Ordering::Relaxed)),
        (
            "live_table_bytes",
            LIVE_TABLE_BYTES.load(Ordering::Relaxed),
        ),
        ("peak_table_bytes", PEAK_TABLE_BYTES.load(Ordering::Relaxed)),
        ("wal_us", WAL_US.load(Ordering::Relaxed)),
        ("memtable_us", MEMTABLE_US.load(Ordering::Relaxed)),
        ("get_us", GET_US.load(Ordering::Relaxed)),
        ("put_hint_us", PUT_HINT_US.load(Ordering::Relaxed)),
        ("put_fieldenc_us", PUT_FIELDENC_US.load(Ordering::Relaxed)),
        ("cache_hits", CACHE_HITS.load(Ordering::Relaxed)),
        ("cache_misses", CACHE_MISSES.load(Ordering::Relaxed)),
    ]
}
