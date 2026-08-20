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
use manifest::{Manifest, SegmentMeta};
use memtable::{Entry, MemTable};
use wal::Wal;

/// Standard-Größe der MemTable, bei der geflusht wird.
pub const DEFAULT_MEMTABLE_LIMIT: usize = 4 * 1024 * 1024;
/// Ab wie vielen Tabellen in Level 0 kompaktiert wird.
pub const DEFAULT_L0_COMPACT_THRESHOLD: usize = 4;
/// Standard-Record-Obergrenze pro L1-Segment (Split-Regel §11.2 der
/// Design-Spez: deterministisch, keine Größensteuerung). Größenordnung im
/// Bereich des MemTable-Limits.
pub const DEFAULT_SEGMENT_MAX_RECORDS: usize = 30_000;

/// Konfiguration der Datenbank.
#[derive(Clone)]
pub struct Options {
    pub memtable_limit: usize,
    pub l0_compact_threshold: usize,
    /// Deterministische Split-Regel für die L1-Segmente: ein Segment wird
    /// geschlossen, sobald es diese Record-Anzahl erreicht.
    pub segment_max_records: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            memtable_limit: DEFAULT_MEMTABLE_LIMIT,
            l0_compact_threshold: DEFAULT_L0_COMPACT_THRESHOLD,
            segment_max_records: DEFAULT_SEGMENT_MAX_RECORDS,
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

        // Manifest-Reconstruction (§12.3): Legacy-L1-Konvertierung + harte
        // Validierung von Datei-Existenz und Segment-Ranges.
        db.convert_legacy_l1()?;
        db.validate_open_state()?;

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

    /// Legacy-Konvertierung (§12.3): Alte `L 1`-Tabellen (v0.7-C/1d-Ära) ohne
    /// Segment-Metadaten werden in Segmente überführt, Range aus dem
    /// SSTable-Index abgeleitet. Danach gelten die Segment-Invarianten.
    fn convert_legacy_l1(&mut self) -> Result<()> {
        if !self.manifest.segments.is_empty() {
            return Ok(());
        }
        let Some(legacy) = self.manifest.levels.get(1).cloned() else {
            return Ok(());
        };
        if legacy.is_empty() {
            return Ok(());
        }
        for id in legacy {
            let path = self.table_path(id);
            let reader = sstable::TableReader::open(&path)?;
            let Some((first, last)) = reader.key_bounds() else {
                return Err(crate::error::Error::Corrupt(
                    "legacy L1 table without index keys",
                ));
            };
            self.manifest.segments.push(SegmentMeta {
                file_id: id,
                min_key: first.to_vec(),
                max_key: last.to_vec(),
                records: reader.num_records(),
            });
        }
        self.manifest
            .segments
            .sort_unstable_by(|a, b| a.min_key.cmp(&b.min_key));
        while self.manifest.levels.len() <= 1 {
            self.manifest.levels.push(Vec::new());
        }
        self.manifest.levels[1] = Vec::new();
        self.manifest.validate()
    }

    /// Harte Validierung beim Öffnen (§12.3): keine Manifest-Referenz darf auf
    /// eine fehlende Datei zeigen; die Segment-Range muss mit dem Index der
    /// Datei übereinstimmen. Verstöße sind `Corrupt`, kein stilles Droppen.
    fn validate_open_state(&self) -> Result<()> {
        for level in &self.manifest.levels {
            for id in level {
                if !self.table_path(*id).exists() {
                    return Err(crate::error::Error::Corrupt(
                        "manifest references missing L0 table",
                    ));
                }
            }
        }
        for seg in &self.manifest.segments {
            let path = self.table_path(seg.file_id);
            if !path.exists() {
                return Err(crate::error::Error::Corrupt(
                    "manifest references missing segment file",
                ));
            }
            let reader = sstable::TableReader::open(&path)?;
            let Some((first, last)) = reader.key_bounds() else {
                return Err(crate::error::Error::Corrupt(
                    "segment table without index keys",
                ));
            };
            if first != seg.min_key.as_slice() || last != seg.max_key.as_slice() {
                return Err(crate::error::Error::Corrupt(
                    "segment range mismatch with table index",
                ));
            }
        }
        Ok(())
    }

    /// Löscht einen Schlüssel (Tombstone).
    ///
    /// Nicht durable, solange die Operation returniert — der Write wird erst
    /// nach einem erfolgreichen `flush()`/`close()` dauerhaft (deferred
    /// durability). Transaktionen sind nach `commit()` durable.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.put_internal(key, None)
    }

    /// Setzt einen Wert. Append-only in WAL + MemTable, Flush bei Größenlimit.
    ///
    /// Nicht durable, solange die Operation returniert: der Write wird erst
    /// nach einem erfolgreichen `flush()`/`close()` dauerhaft (deferred
    /// durability). Für Durability `flush()`/`close()` nutzen; Transaktionen
    /// sind nach `commit()` durable.
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
        // L0: neueste Tabelle zuerst.
        if let Some(ids) = self.manifest.levels.first() {
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
        // L1: genau das eine Segment, dessen [min_key, max_key] den Key enthält.
        if let Some(i) = self.find_segment(key) {
            let id = self.manifest.segments[i].file_id;
            let hit = if let Some(reader) = self.table_cache.get_mut(&id) {
                reader.lookup(key)?
            } else {
                let path = self.table_path(id);
                let mut reader = sstable::TableReader::open(&path)?;
                let result = reader.lookup(key)?;
                self.table_cache.insert(id, reader);
                result
            };
            if let Some(entry) = hit {
                return Ok(entry);
            }
        }
        Ok(None)
    }

    /// Binary-Search: das eine L1-Segment, dessen `[min_key, max_key]` `key`
    /// enthält (Disjunktheits-Invariante garantiert max. ein Treffer).
    fn find_segment(&self, key: &[u8]) -> Option<usize> {
        let segs = &self.manifest.segments;
        if segs.is_empty() {
            return None;
        }
        // Letztes Segment mit min_key <= key.
        let mut lo = 0usize;
        let mut hi = segs.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if segs[mid].min_key.as_slice() <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return None;
        }
        let i = lo - 1;
        if key <= segs[i].max_key.as_slice() {
            Some(i)
        } else {
            None
        }
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
        // L0: neueste zuerst.
        if let Some(ids) = self.manifest.levels.first() {
            for id in ids.iter().rev() {
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
        // L1-Segmente (disjunkt, sortiert nach min_key; jede Reihenfolge als
        // Quelle ist korrekt, da sich die Ranges nicht überlappen).
        for seg in &self.manifest.segments {
            let id = seg.file_id;
            if !self.table_cache.contains_key(&id) {
                self.table_cache
                    .insert(id, sstable::TableReader::open(&self.table_path(id))?);
            }
            let reader = self.table_cache.get(&id).expect("cached").fork()?;
            sources.push(Box::new(sstable::TableIter::from_reader(
                reader, start, end,
            )?));
        }
        let merge = MergeIter::new(sources);
        Ok(ScanIter::new(self, merge))
    }

    /// Bereichs-Scan `[start, end)`. Liefert sortierte `(key, Option<value>)`.
    /// Komfort-Wrapper: sammelt [`scan_stream`](Self::scan_stream) ein.
    ///
    /// Tombstones (`None`) werden **nicht** ausgegeben: gelöschte Keys tauchen in
    /// Scans nicht auf, unabhängig davon, ob ihr Tombstone bereits bei einer
    /// Compaction physisch entfernt wurde. So ist ein Scan vor/nach Compaction
    /// identisch. (`scan_stream` ist die rohe, lazy Variante und liefert die
    /// Tombstones weiterhin als `None`.)
    pub fn scan(
        &mut self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
        Ok(self
            .scan_stream(start, end)?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(_, v)| v.is_some())
            .collect())
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

        // WAL leeren: alle Daten sind jetzt persistiert. MUSS über den Wal-Handle
        // laufen, damit der BufWriter-Puffer verworfen wird (sonst schreibt ein
        // späteres sync() die alten Records zurück in den geleerten Log).
        self.wal.truncate()?;
        self.memtable = MemTable::new();

        // Optional kompaktieren.
        if self.manifest.levels[0].len() >= self.opts.l0_compact_threshold {
            self.compact()?;
        }
        Ok(())
    }

    /// 3a-Compaction (Overlap-Merge): hält L0 klein und ordnet die L0-Tabellen
    /// in die L1-Segmente ein, ohne die komplette L1-Basis neu zu schreiben.
    ///
    /// Nur die L1-Segmente, deren Key-Range den L0-Batch-Span schneidet, werden
    /// gelesen und durch neue Segmente ersetzt; nicht-überlappende Segmente
    /// bleiben unangetastet (gleiche Datei, gleiche Range). Tombstones dürfen
    /// physisch entfallen: für jeden Key im Merge-Span liegt die komplette
    /// Historie im Merge-Set (Disjunktheits-Invariante + alle L0-Tabellen im
    /// Batch).
    fn compact(&mut self) -> Result<()> {
        let l0 = self.manifest.levels.get(0).cloned().unwrap_or_default();
        if l0.len() < self.opts.l0_compact_threshold {
            return Ok(());
        }
        let (batch_min, batch_max) = self
            .table_bounds(&l0)?
            .ok_or_else(|| crate::error::Error::Corrupt("empty L0 table in compact"))?;

        // Überlappende Segmente mergen, Rest behalten.
        let overlaps = |s: &SegmentMeta| {
            s.min_key.as_slice() <= batch_max.as_slice()
                && s.max_key.as_slice() >= batch_min.as_slice()
        };
        let overlap_ids: Vec<u64> = self
            .manifest
            .segments
            .iter()
            .filter(|s| overlaps(s))
            .map(|s| s.file_id)
            .collect();
        let retained: Vec<SegmentMeta> = self
            .manifest
            .segments
            .iter()
            .filter(|s| !overlaps(s))
            .cloned()
            .collect();

        // Merge: L0-Tabellen (neueste zuerst) + überlappende Segmente.
        let mut input_ids: Vec<u64> = l0.iter().copied().rev().collect();
        input_ids.extend(overlap_ids.iter().copied());
        let merged = self.merge_ids(&input_ids, true)?;

        // Deterministischer Split: Chunks von `segment_max_records` → Segmente.
        let mut new_segments: Vec<SegmentMeta> = Vec::new();
        for chunk in merged.chunks(self.opts.segment_max_records) {
            if chunk.is_empty() {
                continue;
            }
            let id = self.write_table(chunk)?;
            new_segments.push(SegmentMeta {
                file_id: id,
                min_key: chunk[0].0.clone(),
                max_key: chunk[chunk.len() - 1].0.clone(),
                records: chunk.len() as u64,
            });
        }

        // Neue L1 = beibehaltene + neue Segmente, sortiert nach min_key.
        let mut all = retained;
        all.extend(new_segments);
        all.sort_unstable_by(|a, b| a.min_key.cmp(&b.min_key));
        self.manifest.segments = all;

        // Manifest-COMMIT (fsync + atomarer rename) MUSS vor dem Löschen der
        // alten SSTables passieren (Crash-Fenster §13 der Design-Spez).
        while self.manifest.levels.len() < 1 {
            self.manifest.levels.push(Vec::new());
        }
        self.manifest.levels[0] = Vec::new();
        self.manifest.save(&self.manifest_path)?;
        self.table_cache.clear(); // alte gelöscht, neue Segmente entstanden
        for id in &l0 {
            let _ = std::fs::remove_file(self.table_path(*id));
        }
        for &id in &overlap_ids {
            let _ = std::fs::remove_file(self.table_path(id));
        }
        Ok(())
    }

    /// Bereinigt verwaiste (Orphan-)SSTables. **Expliziter Maintenance-Schritt**,
    /// keine automatische Bereinigung beim Oeffnen (siehe Design-Doc §30.10).
    ///
    /// Sicherheitsregeln:
    /// - Nur `.sst`-Dateien, deren `file_id` **nicht** im committed Manifest
    ///   (`self.manifest`, identisch mit `MANIFEST` auf Disk) steht, werden
    ///   geloescht. Referenzierte Dateien werden niemals angetastet.
    /// - `.manifest.tmp` (nie recovery-relevant) wird ebenfalls entfernt.
    /// - Keine Aenderung am Manifest, keine Compaction.
    ///
    /// Erfordert exklusiven Zugriff: der `&mut self`-Borrow stellt sicher,
    /// dass waehrend `gc()` kein gleichzeitiger Lese-/Schreibzugriff auf
    /// dieselbe DB-Instanz besteht (In-Process). Cross-Process-Locking ist
    /// bewusst nicht implementiert.
    ///
    /// Rueckgabe: Anzahl geloeschter `.sst`-Orphans.
    pub fn gc(&mut self) -> Result<usize> {
        use std::collections::HashSet;
        let referenced: HashSet<u64> = self.manifest.all_ids().into_iter().collect();

        let mut removed = 0usize;
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            // Nur echte `.sst`-Dateien betrachten.
            if path.extension().and_then(|e| e.to_str()) != Some("sst") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };
            // Unsere Dateien folgen exakt dem Schema `{:06}.sst` (6 Ziffern,
            // zero-padded). Fremde/anders benannte `.sst` werden uebersprungen
            // (Opt-in-Maintenance-Risiko).
            if stem.len() != 6 || !stem.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let id: u64 = match stem.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            if referenced.contains(&id) {
                continue; // referenziert -> niemals anfassen
            }
            std::fs::remove_file(&path)?;
            removed += 1;
        }

        // Stray `.manifest.tmp` ist nie recovery-relevant -> immer gefahrlos
        // entfernen (nicht in `removed` gezaehlt, da kein `.sst`).
        let tmp = self.manifest_path.with_extension("manifest.tmp");
        if tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
        }

        // Cache von entfernten (unreferenzierten) Eintraegen befreien.
        self.table_cache.retain(|id, _| referenced.contains(id));

        Ok(removed)
    }

    /// Mergt die gegebenen Tabellen-IDs in der uebergebenen Reihenfolge
    /// **neueste zuerst** (erste Quelle gewinnt bei Key-Kollision = LWW).
    /// Der Aufrufer garantiert die Ordnung: L0-Tabellen (desc nach ID) vor
    /// L1-Segmenten (disjunkt, Reihenfolge egal).
    /// `drop_tombstones` steuert, ob endgueltige Tombstones (resultierender Wert
    /// `None`) physisch entfernt werden - nur zulaessig, wenn die komplette
    /// Historie jedes betroffenen Keys im Merge-Set liegt (11.1 der Spez).
    fn merge_ids(
        &self,
        ids: &[u64],
        drop_tombstones: bool,
    ) -> Result<Vec<(Vec<u8>, Entry)>> {
        let mut sources = Vec::new();
        for id in ids {
            let path = self.table_path(*id);
            // Manifestierte SSTable muss lesbar sein: Ein Lesefehler (fehlende
            // oder korrupte Datei) bricht die Compaction ab, statt die Tabelle
            // still zu ueberspringen und Daten zu verlieren (E.8).
            let mut reader = sstable::TableReader::open(&path)?;
            let records = reader.iter()?;
            sources.push(records);
        }
        let mut merged = merge_vecs(sources)?;
        if drop_tombstones {
            merged.retain(|(_, v)| v.is_some());
        }
        Ok(merged)
    }

    /// `[min_key, max_key]`-Span ueber mehrere Tabellen (erste/letzte Index-Keys).
    fn table_bounds(&self, ids: &[u64]) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut min: Option<Vec<u8>> = None;
        let mut max: Option<Vec<u8>> = None;
        for id in ids {
            let path = self.table_path(*id);
            // Wie `merge_ids`: Eine unlesbare Tabelle darf die Compaction nicht
            // still fortschreiten lassen (E.8).
            let reader = sstable::TableReader::open(&path)?;
            let (first, last) = reader
                .key_bounds()
                .ok_or_else(|| crate::error::Error::Corrupt("empty L0 table referenced in compact"))?;
            let f = first.to_vec();
            let l = last.to_vec();
            if min.as_ref().map_or(true, |m| f < *m) {
                min = Some(f);
            }
            if max.as_ref().map_or(true, |m| l > *m) {
                max = Some(l);
            }
        }
        Ok(match (min, max) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        })
    }

    /// Schreibt eine neue SSTable mit frischer ID und gibt die ID zurück.
    /// `build_table_from_sorted` fsynct die neue Datei bereits.
    fn write_table(&mut self, records: &[(Vec<u8>, Entry)]) -> Result<u64> {
        let new_id = self.manifest.next_table_id;
        self.manifest.next_table_id += 1;
        let path = self.table_path(new_id);
        compaction::build_table_from_sorted(&path, records)?;
        Ok(new_id)
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
        if level == 1 {
            return self.manifest.segments.len();
        }
        self.manifest.levels.get(level).map_or(0, Vec::len)
    }

    /// Anzahl der L1-Segmente (für Tests/Inspektion).
    pub fn segment_count(&self) -> usize {
        self.manifest.segments.len()
    }

    /// Segment-Metadaten (für Tests/Inspektion der Disjunktheits-Invariante).
    pub fn segments(&self) -> &[SegmentMeta] {
        &self.manifest.segments
    }

    /// Alle Tabellen-IDs, die aktuell im Manifest referenziert werden (für
    /// Tests/Inspektion: prüft, dass keine Referenz auf fehlende Dateien besteht).
    pub fn table_ids(&self) -> Vec<u64> {
        self.manifest.all_ids()
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
        let r = self.db.get(key);
        r
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
        for seg in &db.manifest.segments {
            if let Ok(mut r) = sstable::TableReader::open(&db.table_path(seg.file_id)) {
                if let Ok(recs) = r.iter() {
                    order.push(recs);
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
        // `scan()` gibt keine Tombstones aus (gelöschte Keys erscheinen nicht).
        best.into_iter()
            .filter(|(_, v)| v.is_some())
            .collect()
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
