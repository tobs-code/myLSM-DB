use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{Error, Result};
use crate::memtable::Entry;

const MAGIC: u32 = 0x5353_5442; // "SSTB"
const DEFAULT_SPACING: usize = 16;

/// Ein unveränderliches, sortiertes Segment auf der Platte.
/// Layout: [records][sparse index][bloom][footer]
pub struct TableBuilder {
    path: std::path::PathBuf,
    buf: Vec<u8>,
    index: Vec<u8>,
    spacing: usize,
    keys_since_index: usize,
    num_records: u64,
    bloom: BloomFilter,
    last_key: Option<Vec<u8>>,
    last_offset: u64,
    /// Hat der zuletzt hinzugefügte Record einen Index-Eintrag bekommen
    /// (nur Block-Start-Records tun das)? Steuert den finalen Index-Eintrag
    /// in `finish()`, damit `key_bounds().last` exakt ist.
    last_record_indexed: bool,
}

impl TableBuilder {
    pub fn new(path: &Path) -> Result<TableBuilder> {
        Ok(TableBuilder {
            path: path.to_path_buf(),
            buf: Vec::new(),
            index: Vec::new(),
            spacing: DEFAULT_SPACING,
            keys_since_index: 0,
            num_records: 0,
            bloom: BloomFilter::new(1024, 4),
            last_key: None,
            last_offset: 0,
            last_record_indexed: false,
        })
    }

    /// Fügt einen sortiert eingehenden Datensatz hinzu.
    /// `value = None` = Tombstone.
    pub fn add(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        let flags: u8 = if value.is_some() { 0 } else { 1 };
        let offset = self.buf.len() as u64;

        if self.keys_since_index == 0 {
            // Index-Eintrag am Anfang eines Blocks
            self.index
                .extend_from_slice(&(key.len() as u32).to_le_bytes());
            self.index.extend_from_slice(key);
            self.index.extend_from_slice(&offset.to_le_bytes());
        }
        self.last_record_indexed = self.keys_since_index == 0;
        self.keys_since_index += 1;
        if self.keys_since_index == self.spacing {
            self.keys_since_index = 0;
        }

        self.bloom.insert(key);

        self.last_key = Some(key.to_vec());
        self.last_offset = offset;

        self.buf
            .extend_from_slice(&(key.len() as u32).to_le_bytes());
        self.buf
            .extend_from_slice(&(value.map_or(0, |v| v.len()) as u32).to_le_bytes());
        self.buf.push(flags);
        self.buf.extend_from_slice(key);
        if let Some(v) = value {
            self.buf.extend_from_slice(v);
        }
        self.num_records += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Result<u64> {
        // Exakte Obergrenze für `key_bounds()`: Der sparse Index enthält nur
        // Einträge an Block-Grenzen. Ist der letzte Record keine Block-Grenze,
        // wird er als zusätzlicher finaler Index-Eintrag angehängt, damit die
        // Segment-Range (max_key) und der Index übereinstimmen (§9.2).
        if self.num_records > 0 && !self.last_record_indexed {
            if let Some(lk) = &self.last_key {
                self.index
                    .extend_from_slice(&(lk.len() as u32).to_le_bytes());
                self.index.extend_from_slice(lk);
                self.index
                    .extend_from_slice(&self.last_offset.to_le_bytes());
            }
        }
        let bloom_bytes = self.bloom.to_bytes();
        let index_offset = self.buf.len() as u64;
        let index_len = self.index.len() as u32;
        let bloom_offset = index_offset + index_len as u64;
        let bloom_len = bloom_bytes.len() as u32;

        let file = File::create(&self.path)?;
        let mut w = BufWriter::new(file);
        w.write_all(&self.buf)?;
        w.write_all(&self.index)?;
        w.write_all(&bloom_bytes)?;

        w.write_all(&index_offset.to_le_bytes())?;
        w.write_all(&index_len.to_le_bytes())?;
        w.write_all(&bloom_offset.to_le_bytes())?;
        w.write_all(&bloom_len.to_le_bytes())?;
        w.write_all(&self.spacing.to_le_bytes())?;
        w.write_all(&self.num_records.to_le_bytes())?;
        w.write_all(&MAGIC.to_le_bytes())?;
        // Dauerhaft aufs Medium schreiben, BEVOR ein Manifest-Commit die Datei
        // referenzieren kann. Nur flush() reicht nicht (Userspace-Puffer).
        w.flush()?;
        w.get_ref().sync_all()?;
        Ok(self.num_records)
    }
}

/// Ein unveränderliches, sortiertes Segment — gelesen.
pub struct TableReader {
    /// Gemeinsam geteilter, einmalig geöffneter/geparster Zustand. Mehrere
    /// `TableReader`-Klone teilen sich File-Handle, sparse Index und Bloom,
    /// sodass ein `fork()` (für Scans) keinen erneuten Datei-/Index-Read kostet.
    shared: std::sync::Arc<SharedTable>,
    /// Eigenes Lesepuffer-Objekt über einen duplizierten Handle. Position ist
    /// pro `TableReader` unabhängig (wir seeken vor jedem Lesevorgang).
    file: BufReader<File>,
}

/// Der teilsbare, unveränderliche Zustand einer geöffneten SSTable.
struct SharedTable {
    /// Geteiltes OS-File-Handle. `fork` dupliziert es via `try_clone`.
    file: File,
    num_records: u64,
    bloom: Option<BloomFilter>,
    /// Einmalig beim Öffnen geparster sparse Index (Key → Block-Offset).
    index_entries: Vec<IndexEntry>,
    /// Physische Grenze der Record-Daten (= Index-Offset). Kein Record darf
    /// jemals über diese Grenze hinaus gelesen werden (schützt vor dem
    /// Hineinlesen in Index/Bloom/Footer).
    data_end: u64,
}

struct IndexEntry {
    key: Vec<u8>,
    offset: u64,
}

/// Parst den rohen Index-Block in sortierte Einträge.
fn parse_index(raw: &[u8]) -> Vec<IndexEntry> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < raw.len() {
        let key_len = u32::from_le_bytes(raw[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        let key = raw[i..i + key_len].to_vec();
        i += key_len;
        let offset = u64::from_le_bytes(raw[i..i + 8].try_into().unwrap());
        i += 8;
        out.push(IndexEntry { key, offset });
    }
    out
}

impl TableReader {
    pub fn open(path: &Path) -> Result<TableReader> {
        let mut file = BufReader::new(File::open(path)?);
        let file_len = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        const FOOTER_LEN: u64 = 8 + 4 + 8 + 4 + 8 + 8 + 4; // 44
        if file_len < FOOTER_LEN {
            return Err(Error::InvalidFormat("sstable too small".into()));
        }

        let mut footer = [0u8; FOOTER_LEN as usize];
        file.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
        file.read_exact(&mut footer)?;

        let magic = u32::from_le_bytes(footer[40..44].try_into().unwrap());
        if magic != MAGIC {
            return Err(Error::InvalidFormat("bad sstable magic".into()));
        }
        let index_offset = u64::from_le_bytes(footer[0..8].try_into().unwrap());
        let index_len = u32::from_le_bytes(footer[8..12].try_into().unwrap()) as usize;
        let bloom_offset = u64::from_le_bytes(footer[12..20].try_into().unwrap());
        let bloom_len = u32::from_le_bytes(footer[20..24].try_into().unwrap()) as usize;
        let spacing = usize::from_le_bytes(footer[24..32].try_into().unwrap());
        let num_records = u64::from_le_bytes(footer[32..40].try_into().unwrap());
        let _ = spacing;

        let mut index = vec![0u8; index_len];
        file.seek(SeekFrom::Start(index_offset))?;
        file.read_exact(&mut index)?;

        let bloom = if bloom_len > 0 {
            let mut b = vec![0u8; bloom_len];
            file.seek(SeekFrom::Start(bloom_offset))?;
            file.read_exact(&mut b)?;
            Some(BloomFilter::from_bytes(&b))
        } else {
            None
        };

        let shared_file = file.get_ref().try_clone()?;
        Ok(TableReader {
            shared: std::sync::Arc::new(SharedTable {
                file: shared_file,
                num_records,
                bloom,
                index_entries: parse_index(&index),
                data_end: index_offset,
            }),
            file,
        })
    }

    /// Billiger Klon für Scans: teilt Index/Bloom/Handle, aber mit eigenem,
    /// unabhängig positionierbarem Lese-Puffer. Kein erneuter Datei-/Index-Read.
    pub fn fork(&self) -> Result<TableReader> {
        let r = Ok(TableReader {
            shared: std::sync::Arc::clone(&self.shared),
            file: BufReader::new(self.shared.file.try_clone()?),
        });
        r
    }

    pub fn num_records(&self) -> u64 {
        self.shared.num_records
    }

    /// Erster und letzter Key der Tabelle (aus dem sparse Index) — für die
    /// Segment-Range-Bestimmung und die Manifest-Validierung (§9.2/§12.3).
    pub fn key_bounds(&self) -> Option<(&[u8], &[u8])> {
        let first = self.shared.index_entries.first()?;
        let last = self.shared.index_entries.last()?;
        Some((&first.key, &last.key))
    }

    /// Sucht den Block-Start-Offset für `key` über binäre Suche im Index.
    fn block_offset_for(&self, entries: &[IndexEntry], key: &[u8]) -> u64 {
        let mut lo = 0usize;
        let mut hi = entries.len(); // exclusive
        while lo < hi {
            let mid = (lo + hi) / 2;
            if entries[mid].key.as_slice() <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            // key liegt vor dem ersten Indexeintrag → Scan von Anfang
            entries.first().map_or(0, |e| e.offset)
        } else {
            entries[lo - 1].offset
        }
    }

    fn read_record(&mut self) -> Result<Option<(Vec<u8>, Entry)>> {
        // Nie über die physische Record-Grenze hinaus lesen (Index/Bloom/Footer
        // sind keine Records). Schützt lookup() vor Fehlinterpretation.
        if self.file.stream_position()? >= self.shared.data_end {
            return Ok(None);
        }
        let mut len_buf = [0u8; 8];
        match self.file.read_exact(&mut len_buf) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(Error::Io(e)),
        }
        let key_len = u32::from_le_bytes(len_buf[0..4].try_into().unwrap()) as usize;
        let val_len = u32::from_le_bytes(len_buf[4..8].try_into().unwrap()) as usize;
        let mut flags = [0u8; 1];
        self.file.read_exact(&mut flags)?;
        let deleted = flags[0] == 1;

        let mut key = vec![0u8; key_len];
        self.file.read_exact(&mut key)?;
        let value = if deleted {
            None
        } else {
            let mut v = vec![0u8; val_len];
            self.file.read_exact(&mut v)?;
            Some(v)
        };
        Ok(Some((key, value)))
    }

    /// Punkt-Zugriff mit Präsenz-Unterscheidung.
    ///
    /// Rückgabe: `Ok(Some(entry))` = Key vorhanden (entry ist `Some(value)`
    /// oder `None` für Tombstone). `Ok(None)` = Key definitiv nicht vorhanden.
    /// Nutzt Bloom (falls vorhanden) + sparse Index + linearen Scan.
    pub fn lookup(&mut self, key: &[u8]) -> Result<Option<Entry>> {
        if let Some(bloom) = &self.shared.bloom {
            if !bloom.maybe_contains(key) {
                return Ok(None);
            }
        }
        let start = self.block_offset_for(&self.shared.index_entries, key);
        self.file.seek(SeekFrom::Start(start))?;
        loop {
            match self.read_record()? {
                None => return Ok(None),
                Some((k, v)) => match k.as_slice().cmp(key) {
                    std::cmp::Ordering::Equal => return Ok(Some(v)),
                    std::cmp::Ordering::Greater => return Ok(None),
                    std::cmp::Ordering::Less => continue,
                },
            }
        }
    }

    /// Komfort-Wrapper: liefert den Wert, wobei Tombstone und "nicht vorhanden"
    /// beide `None` sind (keine Präsenz-Unterscheidung).
    pub fn get(&mut self, key: &[u8]) -> Result<Entry> {
        Ok(self.lookup(key)?.unwrap_or(None))
    }

    /// Liefert alle Datensätze sortiert (genau `num_records` Stück).
    pub fn iter(&mut self) -> Result<Vec<(Vec<u8>, Entry)>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut out = Vec::with_capacity(self.shared.num_records as usize);
        for _ in 0..self.shared.num_records {
            if let Some(rec) = self.read_record()? {
                out.push(rec);
            } else {
                break;
            }
        }
        Ok(out)
    }
}

/// Lazy Iterator über eine SSTable im Range `[start, end)`.
///
/// `start` inklusiv, `end` exklusiv. Der Iterator positioniert sich per
/// sparse Index auf den Block, in dem `start` liegen würde, und liest dann nur
/// die Records des Ranges — **keine** vollständige Materialisierung.
pub struct TableIter {
    reader: TableReader,
    start: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
    /// `None` = noch nicht geladen; `Some(None)` = erschöpft; `Some(Some(..))` = gepuffert.
    cur: Option<Option<(Vec<u8>, Entry)>>,
}

impl TableIter {
    pub fn open(path: &Path, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<TableIter> {
        Self::from_reader(TableReader::open(path)?, start, end)
    }

    /// Baut aus einem (ggf. geforkten) Reader einen Iterator — ohne erneutes
    /// Öffnen der Datei. Positioniert auf den Block von `start`.
    pub fn from_reader(
        mut reader: TableReader,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<TableIter> {
        let offset = match start {
            Some(s) => reader.block_offset_for(&reader.shared.index_entries, s),
            None => reader.shared.index_entries.first().map_or(0, |e| e.offset),
        };
        reader.file.seek(SeekFrom::Start(offset))?;
        Ok(TableIter {
            reader,
            start: start.map(Into::into),
            end: end.map(Into::into),
            cur: None,
        })
    }

    /// Lädt den nächsten Record in `cur`, respektiert `[start, end)`.
    fn load(&mut self) -> Result<()> {
        loop {
            match self.reader.read_record()? {
                None => {
                    self.cur = Some(None);
                    return Ok(());
                }
                Some((k, v)) => {
                    if let Some(e) = &self.end {
                        if k.as_slice() >= e.as_slice() {
                            self.cur = Some(None);
                            return Ok(());
                        }
                    }
                    if let Some(s) = &self.start {
                        if k.as_slice() < s.as_slice() {
                            continue; // unterhalb des Ranges überspringen
                        }
                    }
                    self.cur = Some(Some((k, v)));
                    return Ok(());
                }
            }
        }
    }
}

impl crate::iterator::ScanSource for TableIter {
    fn peek(&mut self) -> Result<Option<(Vec<u8>, Entry)>> {
        if self.cur.is_none() {
            self.load()?;
        }
        Ok(self.cur.clone().unwrap())
    }
    fn advance(&mut self) -> Result<()> {
        self.cur = None;
        Ok(())
    }
}

/// Einfacher Bloom-Filter mit doppeltem Hashing (FNV-1a).
pub struct BloomFilter {
    bits: Vec<u8>,
    nbits: u32,
    k: u32,
}

impl BloomFilter {
    pub fn new(nbits: u32, k: u32) -> BloomFilter {
        let nbytes = (nbits.div_ceil(8)) as usize;
        BloomFilter {
            bits: vec![0u8; nbytes],
            nbits,
            k,
        }
    }

    fn hashes(&self, key: &[u8]) -> (u64, u64) {
        let mut h1 = 0xcbf2_9ce4_8422_2325u64;
        let mut h2 = 0xbf58_476d_1ce4_e5b9u64;
        for &b in key {
            let b = b as u64;
            h1 = h1.rotate_left(5) ^ b;
            h1 = h1.wrapping_mul(0x1000_0000_01b3);
            h2 = h2.rotate_left(5) ^ b;
            h2 = h2.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
        (h1, h2)
    }

    pub fn insert(&mut self, key: &[u8]) {
        let (a, b) = self.hashes(key);
        for i in 0..self.k {
            let h = a.wrapping_add(b.wrapping_mul(i as u64));
            let bit = (h % self.nbits as u64) as usize;
            self.bits[bit / 8] |= 1 << (bit % 8);
        }
    }

    pub fn maybe_contains(&self, key: &[u8]) -> bool {
        let (a, b) = self.hashes(key);
        for i in 0..self.k {
            let h = a.wrapping_add(b.wrapping_mul(i as u64));
            let bit = (h % self.nbits as u64) as usize;
            if self.bits[bit / 8] & (1 << (bit % 8)) == 0 {
                return false;
            }
        }
        true
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.bits.len());
        out.extend_from_slice(&self.nbits.to_le_bytes());
        out.extend_from_slice(&self.k.to_le_bytes());
        out.extend_from_slice(&self.bits);
        out
    }

    pub fn from_bytes(data: &[u8]) -> BloomFilter {
        let nbits = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let k = u32::from_le_bytes(data[4..8].try_into().unwrap());
        BloomFilter {
            bits: data[8..].to_vec(),
            nbits,
            k,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterator::ScanSource;

    #[test]
    fn build_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sst");
        let mut b = TableBuilder::new(&path).unwrap();
        b.add(b"a".as_slice(), Some(b"1".as_slice())).unwrap();
        b.add(b"b".as_slice(), None).unwrap();
        b.add(b"c".as_slice(), Some(b"3".as_slice())).unwrap();
        b.finish().unwrap();

        let mut r = TableReader::open(&path).unwrap();
        assert_eq!(r.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(r.get(b"b").unwrap(), None);
        assert_eq!(r.get(b"c").unwrap(), Some(b"3".to_vec()));
        assert_eq!(r.get(b"zz").unwrap(), None);

        let all = r.iter().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, b"a".to_vec());
    }

    #[test]
    fn lookup_distinguishes_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sst");
        let mut b = TableBuilder::new(&path).unwrap();
        b.add(b"a".as_slice(), Some(b"1".as_slice())).unwrap();
        b.add(b"b".as_slice(), None).unwrap(); // Tombstone
        b.finish().unwrap();

        let mut r = TableReader::open(&path).unwrap();
        // vorhandener Wert
        assert_eq!(r.lookup(b"a").unwrap(), Some(Some(b"1".to_vec())));
        // vorhandener Tombstone → Some(None)
        assert_eq!(r.lookup(b"b").unwrap(), Some(None));
        // nicht vorhanden → None
        assert_eq!(r.lookup(b"zz").unwrap(), None);
    }

    #[test]
    fn key_bounds_exact_at_block_alignment() {
        // Regression: `key_bounds().last` muss auch dann exakt der letzte
        // Record sein, wenn die Record-Zahl ein Vielfaches von `spacing` ist
        // (der letzte Record ist dann KEINE Block-Grenze und bekommt über den
        // Block-Start-Eintrag keinen Index-Platz). Sonst stimmt die Segment-
        // Range nicht mit dem Index überein → `validate_open_state` meldet
        // fälschlich `Corrupt`.
        let dir = tempfile::tempdir().unwrap();
        for n in [16usize, 30_000] {
            let path = dir.path().join(format!("t{n}.sst"));
            let mut b = TableBuilder::new(&path).unwrap();
            for i in 0..n {
                let k = format!("k{:08}", i);
                b.add(k.as_bytes(), Some(b"v".as_slice())).unwrap();
            }
            b.finish().unwrap();
            let r = TableReader::open(&path).unwrap();
            let (first, last) = r.key_bounds().unwrap();
            assert_eq!(first, b"k00000000".as_slice());
            assert_eq!(last, format!("k{:08}", n - 1).as_bytes());
        }
    }

    #[test]
    fn lookup_stays_within_record_region() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sst");
        let mut b = TableBuilder::new(&path).unwrap();
        for i in 0..100u32 {
            let k = format!("key-{:03}", i);
            b.add(k.as_bytes(), Some(b"v".as_slice())).unwrap();
        }
        b.finish().unwrap();

        let mut r = TableReader::open(&path).unwrap();
        // Key jenseits des Maximalwerts: darf NICHT in Index/Bloom/Footer lesen,
        // sondern muss sauber Ok(None) liefern (kein UnexpectedEof/InvalidFormat).
        assert_eq!(r.lookup(b"zzz").unwrap(), None);
        assert_eq!(r.lookup(b"key-099").unwrap(), Some(Some(b"v".to_vec())));
        // Bloß zur Sicherheit auch die Grenze im direkten Record-Bereich prüfen.
        assert_eq!(r.iter().unwrap().len(), 100);
    }

    #[test]
    fn table_iter_seeks_and_stops_at_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sst");
        let mut b = TableBuilder::new(&path).unwrap();
        for i in 0..100u32 {
            let k = format!("key-{:03}", i);
            b.add(k.as_bytes(), Some(b"v".as_slice())).unwrap();
        }
        b.finish().unwrap();

        // Vollständiger Scan: alle 100, sortiert.
        let mut it = TableIter::open(&path, None, None).unwrap();
        let all = drain_table(&mut it);
        assert_eq!(all.len(), 100);
        assert_eq!(all[0], b"key-000".to_vec());

        // Seek exakt am ersten >= start, mitten im 2. Block.
        let mut it = TableIter::open(&path, Some(b"key-017"), Some(b"key-050")).unwrap();
        let r = drain_table(&mut it);
        let expected: Vec<Vec<u8>> = (17..50)
            .map(|i| format!("key-{:03}", i).into_bytes())
            .collect();
        assert_eq!(r, expected);

        // start exakt an einer Blockgrenze (Record 16 beginnt Block 2).
        let mut it = TableIter::open(&path, Some(b"key-016"), Some(b"key-032")).unwrap();
        let r = drain_table(&mut it);
        let expected: Vec<Vec<u8>> = (16..32)
            .map(|i| format!("key-{:03}", i).into_bytes())
            .collect();
        assert_eq!(r, expected);

        // start jenseits des Maximalwerts bzw. end vor dem Minimalwert → leer.
        let mut it = TableIter::open(&path, Some(b"key-999"), None).unwrap();
        assert!(drain_table(&mut it).is_empty());
        let mut it = TableIter::open(&path, None, Some(b"key-000")).unwrap();
        assert!(drain_table(&mut it).is_empty());
    }

    fn drain_table(it: &mut TableIter) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            match it.peek().unwrap() {
                None => break,
                Some((k, _)) => {
                    out.push(k);
                    it.advance().unwrap();
                }
            }
        }
        out
    }

    #[test]
    fn bloom_basics() {
        let mut bf = BloomFilter::new(64, 2);
        bf.insert(b"hello");
        assert!(bf.maybe_contains(b"hello"));
        // "world" ist nie eingefügt → praktisch sicher false (bei 64 bit könnte false positive vorkommen)
    }
}
