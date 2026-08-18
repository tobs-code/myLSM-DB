pub mod compaction;
pub mod error;
pub mod iterator;
pub mod manifest;
pub mod memtable;
pub mod sstable;
pub mod wal;

use std::path::{Path, PathBuf};
use std::collections::HashMap;

use error::Result;
use iterator::{MergedIter, ReadSnapshot};
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
        };

        // Recovery: WAL in die MemTable einspielen.
        let records = wal::replay(&db.wal_path)?;
        for rec in records {
            db.memtable.put(rec.key, rec.value);
        }

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
                    // Nicht gecacht → öffnen, lookuppen und (falls gefunden) cachen.
                    let path = self.table_path(*id);
                    let mut reader = sstable::TableReader::open(&path)?;
                    let result = reader.lookup(key)?;
                    if result.is_some() {
                        self.table_cache.insert(*id, reader);
                    }
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

    /// Bereichs-Scan [start, end). Liefert sortierte (key, Option<value>).
    pub fn scan(&mut self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
        let snapshot = self.read_snapshot()?;
        let merged: Vec<_> = snapshot.merge().collect();
        Ok(merged
            .into_iter()
            .filter(|(k, _)| {
                let in_start = start.map_or(true, |s| k.as_slice() >= s);
                let in_end = end.map_or(true, |e| k.as_slice() < e);
                in_start && in_end
            })
            .collect())
    }

    /// Erzwingt das Flushen der aktuellen MemTable als SSTable (Level 0).
    pub fn flush(&mut self) -> Result<()> {
        if self.memtable.is_empty() {
            return Ok(());
        }
        let records: Vec<(Vec<u8>, Entry)> = self.memtable.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect();

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

        let merged: Vec<(Vec<u8>, Entry)> =
            MergedIter::new(vec![level_records, next_records]).collect();

        // Neuen Schlüssel vergeben.
        let new_id = self.manifest.next_table_id;
        self.manifest.next_table_id += 1;
        let new_path = self.table_path(new_id);
        compaction::build_table_from_sorted(&new_path, &merged)?;

        // Alte Tabellen aus Manifest + Platte entfernen.
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
        for id in &removed {
            let _ = std::fs::remove_file(self.table_path(*id));
        }
        if self.manifest.levels.len() <= level + 1 {
            self.manifest.levels.push(Vec::new());
        }
        self.manifest.levels[level] = Vec::new();
        self.manifest.levels[level + 1] = vec![new_id];
        self.manifest.save(&self.manifest_path)?;
        self.table_cache.clear(); // alte gelöscht, neue Tabelle entstanden
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
        MergedIter::new(sources).collect()
    }

    fn read_snapshot(&mut self) -> Result<ReadSnapshot> {
        let mut snapshot = ReadSnapshot::empty();
        snapshot.memtable = self.memtable.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect();
        for level in 0..self.manifest.levels.len() {
            snapshot.levels.push(self.merge_level(level));
        }
        Ok(snapshot)
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