pub mod codec;
pub mod compaction;
pub mod entity;
pub mod error;
pub mod index;
pub mod iterator;
pub mod keycodec;
pub mod manifest;
pub mod memtable;
pub mod ordering;
pub mod query;
pub mod schema;
pub mod sstable;
pub mod wal;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use error::Result;
use iterator::{MergeIter, ScanIter, ScanSource, memtable_source, merge_vecs};
use manifest::Manifest;
use memtable::{Entry, MemTable};
use wal::Wal;

/// Standard-Größe der MemTable, bei der geflusht wird.
pub const DEFAULT_MEMTABLE_LIMIT: usize = 4 * 1024 * 1024;
/// Ab wie vielen Tabellen in Level 0 kompaktiert wird.
pub const DEFAULT_L0_COMPACT_THRESHOLD: usize = 4;

/// Konfiguration der Datenbank.
#[derive(Clone)]
pub struct Options {
    pub memtable_limit: usize,
    pub l0_compact_threshold: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            memtable_limit: DEFAULT_MEMTABLE_LIMIT,
            l0_compact_threshold: DEFAULT_L0_COMPACT_THRESHOLD,
        }
    }
}

/// Eine LSM-Engine: `put`/`get`/`delete`/`scan` über WAL + MemTable + SSTables.
pub struct Database {
    dir: PathBuf,
    wal_path: PathBuf,
    manifest_path: PathBuf,
    wal: Wal,
    memtable: MemTable,
    manifest: Manifest,
    opts: Options,
    closed: bool,
    /// Geöffnete SSTable-Reader, nach Tabellen-ID. Wird bei Flush/Compaction invalidiert.
    table_cache: HashMap<u64, sstable::TableReader>,
    /// Nächste frei zu vergebende Transaktions-ID (nie wiederverwendet).
    next_tx_id: u64,
}

impl Database {
    /// Öffnet (oder erstellt) eine Datenbank in `dir`. Spielt beim Start den
    /// WAL neu und rekonstruiert den SSTable-Bestand aus dem Manifest.
    pub fn open(dir: impl AsRef<Path>) -> Result<Database> {
        Self::open_with(dir, Options::default())
    }

    pub fn open_with(dir: impl AsRef<Path>, opts: Options) -> Result<Database> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let wal_path = dir.join("wal.log");
        let manifest_path = dir.join("MANIFEST");

        let manifest = Manifest::load(&manifest_path)?;
        // Rekonstruiere eine frische Manifest-Struktur bis zur maximalen Levelhöhe.
        let max_level = manifest.levels.len();
        let mut manifest = manifest;
        while manifest.levels.len() < max_level.max(1) {
            manifest.levels.push(Vec::new());
        }

        let wal = Wal::create(&wal_path)?;
        let mut db = Database {
            dir,
            wal_path,
            manifest_path,
            wal,
            memtable: MemTable::new(),
            manifest,
            opts,
            closed: false,
            table_cache: HashMap::new(),
            next_tx_id: 0,
        };

        // Recovery: WAL in die MemTable einspielen. Die höchste gesehene
        // Transaktions-ID (auch uncommitteter) übernehmen, damit nach einem
        // Crash keine TX-ID in demselben WAL wiederverwendet wird.
        let replay = wal::replay(&db.wal_path)?;
        for rec in replay.records {
            db.memtable.put(rec.key, rec.value);
        }
        db.next_tx_id = replay.max_tx_id + 1;

        Ok(db)
    }

    /// Löscht einen Schlüssel (Tombstone).
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.put_internal(key, None)
    }

    /// Setzt einen Wert. Append-only in WAL + MemTable, Flush bei Größenlimit.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.put_internal(key, Some(value))
    }

    fn put_internal(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        self.wal.append(key, value)?;
        // Beim Löschen über MemTable als Tombstone speichern; der WAL-Eintrag
        // (value=None) wird beim Recovery identisch rekonstruiert.
        self.memtable.put(key.to_vec(), value.map(|v| v.to_vec()));

        if self.memtable.size_bytes() >= self.opts.memtable_limit {
            self.flush()?;
        }
        Ok(())
    }

    /// Vergibt eine monotone, niemals wiederverwendete Transaktions-ID.
    pub(crate) fn alloc_tx_id(&mut self) -> u64 {
        let id = self.next_tx_id;
        self.next_tx_id += 1;
        id
    }

    /// Schreibt eine `Begin`-Markierung für `tx` in den WAL-Puffer.
    pub(crate) fn wal_begin(&mut self, tx: u64) -> Result<()> {
        self.wal.append_begin(tx)
    }

    /// Schreibt eine `TxPut`-Mutation für `tx` in den WAL-Puffer.
    pub(crate) fn wal_tx_put(&mut self, tx: u64, key: &[u8], value: &[u8]) -> Result<()> {
        self.wal.append_tx_put(tx, key, value)
    }

    /// Schreibt eine `TxDelete`-Mutation für `tx` in den WAL-Puffer.
    pub(crate) fn wal_tx_delete(&mut self, tx: u64, key: &[u8]) -> Result<()> {
        self.wal.append_tx_delete(tx, key)
    }

    /// Schreibt eine `Commit`-Markierung für `tx` in den WAL-Puffer.
    pub(crate) fn wal_commit(&mut self, tx: u64) -> Result<()> {
        self.wal.append_commit(tx)
    }

    /// Leert den WAL-Puffer und macht ihn dauerhaft (fsync) — der Commit-Point.
    pub(crate) fn wal_sync(&mut self) -> Result<()> {
        self.wal.sync()
    }

    /// Wendet eine committete Mutation NUR auf die MemTable an (kein WAL-Write).
    /// Infallibel: Ein Committeter `Commit`-Record ist bereits durable, das
    /// MemTable-Apply danach darf nicht mehr fehlschlagen.
    pub(crate) fn mem_put(&mut self, key: &[u8], value: &[u8]) {
        self.memtable.put(key.to_vec(), Some(value.to_vec()));
    }

    /// Wendet eine committete Löschung NUR auf die MemTable an. Infallibel.
    pub(crate) fn mem_delete(&mut self, key: &[u8]) {
        self.memtable.put(key.to_vec(), None);
    }

    /// Best-Effort-Flush, falls die MemTable das Limit überschritten hat.
    /// Nach einem Commit ist die Durability über den WAL gesichert, ein
    /// Flush-Fehler hier ist also nicht kritisch.
    pub(crate) fn flush_if_over_limit(&mut self) -> Result<()> {
        if self.memtable.size_bytes() >= self.opts.memtable_limit {
            self.flush()?;
        }
        Ok(())
    }
    /// Holt einen Wert. `None` = nicht vorhanden oder gelöscht.
    ///
    /// Gezielter Punkt-Lookup statt voller Snapshot: prüft die MemTable (neueste
    /// Quelle), dann die Level von neu (0) nach alt. Innerhalb eines Levels wird
    /// die neueste Tabelle zuerst geprüft. Der erste Treffer gewinnt. Dadurch ist
    /// `get` O(Anzahl Tabellen) statt O(gesamte Datenmenge).
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        // MemTable ist die neueste Quelle → autoritativ.
        if let Some(entry) = self.memtable.get(key) {
            return Ok(entry.clone());
        }
        for level in 0..self.manifest.levels.len() {
            let Some(ids) = self.manifest.levels.get(level) else {
                continue;
            };
            // Innerhalb des Levels neueste Tabelle zuerst.
            for id in ids.iter().rev() {
                let hit = if let Some(reader) = self.table_cache.get_mut(id) {
                    reader.lookup(key)?
                } else {
                    // Nicht gecacht → öffnen und IMMER cachen (auch ein negativer
                    // Lookup profitiert vom geparsten Index und geöffneten File).
                    let path = self.table_path(*id);
                    let mut reader = sstable::TableReader::open(&path)?;
                    let result = reader.lookup(key)?;
                    self.table_cache.insert(*id, reader);
                    result
                };
                // lookup unterscheidet "vorhanden (auch Tombstone)" von "fehlt".
                if let Some(entry) = hit {
                    return Ok(entry);
                }
            }
        }
        Ok(None)
    }

    /// Bereichs-Scan `[start, end)`. Lazy — materialisiert **nicht** die
    /// Datenmenge, sondern liefert einen Stream über MemTable-Kopie + gecachten
    /// SSTable-Cursorn.
    pub fn scan_stream<'a>(
        &'a mut self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<ScanIter<'a>> {
        let mut sources: Vec<Box<dyn ScanSource>> = Vec::new();
        // MemTable ist die neueste Quelle (Index 0). Kopie ist beschränkt
        // (≤ memtable_limit) und über den exklusiven Borrow konsistent.
        sources.push(Box::new(memtable_source(&self.memtable, start, end)));
        for level in &self.manifest.levels {
            for id in level.iter().rev() {
                // Gecachte Reader wiederverwenden (Index/Bloom/Handle teilen),
                // statt pro Scan jede Datei neu zu öffnen — sonst O(Tabellen) pro
                // Scan und quadratisch über viele Puts.
                if !self.table_cache.contains_key(id) {
                    self.table_cache
                        .insert(*id, sstable::TableReader::open(&self.table_path(*id))?);
                }
                let reader = self.table_cache.get(id).expect("cached").fork()?;
                sources.push(Box::new(sstable::TableIter::from_reader(
                    reader, start, end,
                )?));
            }
        }
        let merge = MergeIter::new(sources);
        Ok(ScanIter::new(self, merge))
    }

    /// Bereichs-Scan `[start, end)`. Liefert sortierte `(key, Option<value>)`.
    /// Komfort-Wrapper: sammelt [`scan_stream`](Self::scan_stream) ein.
    pub fn scan(
        &mut self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
        self.scan_stream(start, end)?
            .collect::<std::result::Result<Vec<_>, _>>()
    }

    /// Erzwingt das Flushen der aktuellen MemTable als SSTable (Level 0).
    pub fn flush(&mut self) -> Result<()> {
        if self.memtable.is_empty() {
            return Ok(());
        }
        let records: Vec<(Vec<u8>, Entry)> = self
            .memtable
            .iter()
            .map(|(k, v)| (k.to_vec(), v.clone()))
            .collect();

        let id = self.manifest.next_table_id;
        self.manifest.next_table_id += 1;
        let table_path = self.table_path(id);
        compaction::build_table_from_sorted(&table_path, &records)?;

        while self.manifest.levels.len() < 1 {
            self.manifest.levels.push(Vec::new());
        }
        self.manifest.levels[0].push(id);
        self.manifest.save(&self.manifest_path)?;
        self.table_cache.clear(); // Struktur hat sich geändert

        // WAL leeren: alle Daten sind jetzt persistiert.
        wal::clear(&self.wal_path)?;
        self.memtable = MemTable::new();

        // Optional kompaktieren.
        if self.manifest.levels[0].len() >= self.opts.l0_compact_threshold {
            self.compact_level(0)?;
        }
        Ok(())
    }

    /// Mergt Level `level` in Level `level + 1` zu einer neuen SSTable.
    fn compact_level(&mut self, level: usize) -> Result<()> {
        let level_records = self.merge_level(level);
        let next_records = self.merge_level(level + 1);

        let merged = merge_vecs(vec![level_records, next_records])?;

        // Neuen Schlüssel vergeben.
        let new_id = self.manifest.next_table_id;
        self.manifest.next_table_id += 1;
        let new_path = self.table_path(new_id);
        // build_table_from_sorted führt bereits ein fsync der neuen SSTable aus.
        compaction::build_table_from_sorted(&new_path, &merged)?;

        // Alte Tabellen aus dem Manifest entfernen + neue aufnehmen.
        let removed: Vec<u64> = self
            .manifest
            .levels
            .get(level)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .chain(
                self.manifest
                    .levels
                    .get(level + 1)
                    .cloned()
                    .unwrap_or_default(),
            )
            .collect();
        if self.manifest.levels.len() <= level + 1 {
            self.manifest.levels.push(Vec::new());
        }
        self.manifest.levels[level] = Vec::new();
        self.manifest.levels[level + 1] = vec![new_id];
        // Manifest-COMMIT (fsync + atomarer rename) MUSS vor dem Löschen der
        // alten SSTables passieren. Sonst verweist das Manifest nach einem Crash
        // auf bereits gelöschte Dateien. Verbleibende alte Dateien sind nach einem
        // Crash einfach Orphaned Garbage, das später aufgeräumt werden kann.
        self.manifest.save(&self.manifest_path)?;
        self.table_cache.clear(); // alte gelöscht, neue Tabelle entstanden

        // Erst NACH dem Commit die alten SSTable-Dateien von der Platte entfernen.
        for id in &removed {
            let _ = std::fs::remove_file(self.table_path(*id));
        }
        Ok(())
    }

    /// Lädt alle Tabellen eines Levels und mergt sie (neuere = höherer Index gewinnt).
    fn merge_level(&mut self, level: usize) -> Vec<(Vec<u8>, Entry)> {
        let Some(ids) = self.manifest.levels.get(level) else {
            return Vec::new();
        };
        let mut sources = Vec::new();
        // Höherer Index = neuer → zuerst, damit er im Merge gewinnt.
        for id in ids.iter().rev() {
            let path = self.table_path(*id);
            if let Ok(mut reader) = sstable::TableReader::open(&path) {
                if let Ok(records) = reader.iter() {
                    sources.push(records);
                }
            }
        }
        merge_vecs(sources).unwrap_or_default()
    }

    fn table_path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{:06}.sst", id))
    }

    /// Anzahl der aktuell bekannten SSTables (für Tests/Inspektion).
    pub fn table_count(&self) -> usize {
        self.manifest.all_ids().len()
    }

    pub fn level_count(&self) -> usize {
        self.manifest.levels.len()
    }

    pub fn level_tables(&self, level: usize) -> usize {
        self.manifest.levels.get(level).map_or(0, Vec::len)
    }

    /// Sauber schließen: MemTable flushen, WAL + Manifest dauerhaft machen.
    ///
    /// Das ist der primäre Durability-Mechanismus für einen sauberen Shutdown.
    /// Rufe `close()` bewusst auf, statt dich auf `drop(db)` zu verlassen.
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.flush()?;
        self.wal.sync()?;
        self.manifest.save(&self.manifest_path)?;
        self.closed = true;
        Ok(())
    }
}

/// Best-Effort-Fallback: Flusht beim Verwerfen, wenn `close()` nicht explizit
/// gerufen wurde. Fehler werden ignoriert — `close()` ist die zuverlässige API.
impl Drop for Database {
    fn drop(&mut self) {
        if !self.closed {
            // Best-Effort: nicht als Durability-Garantie betrachten.
            let _ = self.flush();
            let _ = self.wal.sync();
        }
    }
}

/// Abstraktion über eine KV-Sicht für Reads + Writes. Einerseits die direkte
/// (committete) Engine (`DirectMutator`), andererseits eine Transaktions-Sicht
/// mit Pending-Overlay (`TxMutator`). So teilen sich nicht-transaktionale und
/// transaktionale Pfade dieselbe Entity-/Index-Logik.
pub trait Mutator {
    /// Punkt-Lookup. `None` = nicht vorhanden oder gelöscht.
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    /// Bereichs-Scan `[start, end)`, sortiert, **lazy** (streamt die Quellen,
    /// materialisiert nicht die Datenmenge).
    fn scan<'s>(&'s mut self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<ScanStream<'s>>;
    /// Schreibt (bzw. überschreibt) einen Key.
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()>;
    /// Löscht einen Key (Tombstone).
    fn delete(&mut self, key: &[u8]) -> Result<()>;
}

/// Sortierter, lazy Scan-Stream über eine Mutator-Sicht.
pub type ScanStream<'s> = Box<dyn Iterator<Item = Result<(Vec<u8>, Option<Vec<u8>>)>> + 's>;

/// Mutator direkt auf der committeten Engine (nicht-transaktionaler Pfad).
pub struct DirectMutator<'a> {
    pub db: &'a mut Database,
}

impl<'a> Mutator for DirectMutator<'a> {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.db.get(key)
    }
    fn scan<'s>(&'s mut self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<ScanStream<'s>> {
        Ok(Box::new(self.db.scan_stream(start, end)?))
    }
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.db.put(key, value)
    }
    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.db.delete(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Eager-Referenz: newest-wins-Merge über MemTable + alle Tabellen
    /// (gleiche Quellen-Reihenfolge wie `scan_stream`).
    fn model_scan(
        db: &Database,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let in_range = |k: &[u8]| start.is_none_or(|s| k >= s) && end.is_none_or(|e| k < e);
        let mut order: Vec<Vec<(Vec<u8>, Option<Vec<u8>>)>> = Vec::new();
        order.push(
            db.memtable
                .iter()
                .map(|(k, v)| (k.to_vec(), v.clone()))
                .collect(),
        );
        for level in &db.manifest.levels {
            for id in level.iter().rev() {
                if let Ok(mut r) = sstable::TableReader::open(&db.table_path(*id)) {
                    if let Ok(recs) = r.iter() {
                        order.push(recs);
                    }
                }
            }
        }
        // order[0] ist die neueste Quelle → erster Insert pro Key gewinnt.
        let mut best: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        for src in order {
            for (k, v) in src {
                if in_range(&k) {
                    best.entry(k).or_insert(v);
                }
            }
        }
        best.into_iter().collect()
    }

    fn setup(dir: &Path) -> Database {
        let mut db = Database::open(dir).unwrap();
        db.put(b"a", b"new").unwrap();
        db.put(b"b", b"tomb-mem").unwrap();
        db.put(b"c", b"1").unwrap();
        db.flush().unwrap(); // Tabelle 1
        db.delete(b"b").unwrap(); // Tombstone in MemTable schattiert altes b
        db.put(b"d", b"2").unwrap();
        db.put(b"z", b"znew").unwrap();
        db.flush().unwrap(); // Tabelle 2
        db.put(b"a", b"newer").unwrap();
        db.delete(b"c").unwrap();
        db.put(b"e", b"5").unwrap();
        db.put(b"\x00", b"low").unwrap();
        db
    }

    #[test]
    fn lazy_equals_eager() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let ranges: Vec<(Option<&[u8]>, Option<&[u8]>)> = vec![
            (None, None),
            (Some(b"b"), None),
            (None, Some(b"d")),
            (Some(b"a"), Some(b"z")),
            (Some(b"cc"), Some(b"dd")), // mitten im Block / leere Mitte
            (Some(b"\xff"), None),      // größer als alle vorhandenen
            (Some(b""), Some(b"\x00")), // end == start → leer
        ];
        for (s, e) in ranges {
            let expected = model_scan(&db, s, e);
            let actual = db.scan(s, e).unwrap();
            assert_eq!(actual, expected, "range {s:?}..{e:?}");
        }
    }

    #[test]
    fn scan_reuses_cached_table_readers() {
        // Regression: scan_stream muss gecachte Reader wiederverwenden statt
        // pro Scan jede Datei neu zu öffnen. Nach dem ersten Scan ist der Cache
        // befüllt; weitere Scans dürfen die Cache-Größe nicht mehr wachsen lassen.
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path()); // erzeugt 2 SSTables via flush
        assert!(db.table_cache.is_empty(), "cache startet leer");
        let expected: Vec<(Vec<u8>, Option<Vec<u8>>)> = db.scan(None, None).unwrap();
        let n = db.table_cache.len();
        assert!(
            n > 0 && n == db.table_count(),
            "cache befüllt mit allen Tabellen ({n} vs {})",
            db.table_count()
        );
        for _ in 0..3 {
            let again = db.scan(None, None).unwrap();
            assert_eq!(again, expected, "Ergebnis stabil über Scans");
            assert_eq!(db.table_cache.len(), n, "keine neuen Reader nach 1. Scan");
        }
    }

    #[test]
    fn lazy_streams_consistently_under_borrow() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let mut stream = db.scan_stream(None, None).unwrap();
        let first = stream.next().unwrap().unwrap();
        // Der exklusive Borrow ist aktiv: weitere &mut-Zugriffe sind unmöglich.
        assert_eq!(first.0, b"\x00".to_vec());
        let rest: std::result::Result<Vec<_>, _> = stream.collect();
        assert!(!rest.unwrap().is_empty());
    }
}
