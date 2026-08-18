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
        })
    }

    /// Fügt einen sortiert eingehenden Datensatz hinzu.
    /// `value = None` = Tombstone.
    pub fn add(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        let flags: u8 = if value.is_some() { 0 } else { 1 };
        let offset = self.buf.len() as u64;

        if self.keys_since_index == 0 {
            // Index-Eintrag am Anfang eines Blocks
            self.index.extend_from_slice(&(key.len() as u32).to_le_bytes());
            self.index.extend_from_slice(key);
            self.index.extend_from_slice(&offset.to_le_bytes());
        }
        self.keys_since_index += 1;
        if self.keys_since_index == self.spacing {
            self.keys_since_index = 0;
        }

        self.bloom.insert(key);

        self.buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(&(value.map_or(0, |v| v.len()) as u32).to_le_bytes());
        self.buf.push(flags);
        self.buf.extend_from_slice(key);
        if let Some(v) = value {
            self.buf.extend_from_slice(v);
        }
        self.num_records += 1;
        Ok(())
    }

    pub fn finish(self) -> Result<u64> {
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
        w.flush()?;
        Ok(self.num_records)
    }
}

/// Ein unveränderliches, sortiertes Segment — gelesen.
pub struct TableReader {
    file: BufReader<File>,
    num_records: u64,
    bloom: Option<BloomFilter>,
    /// Einmalig beim Öffnen geparster sparse Index (Key → Block-Offset).
    index_entries: Vec<IndexEntry>,
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

        Ok(TableReader {
            file,
            index_entries: parse_index(&index),
            num_records,
            bloom,
        })
    }

    pub fn num_records(&self) -> u64 {
        self.num_records
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
        if let Some(bloom) = &self.bloom {
            if !bloom.maybe_contains(key) {
                return Ok(None);
            }
        }
        let start = self.block_offset_for(&self.index_entries, key);
        self.file.seek(SeekFrom::Start(start))?;
        loop {
            match self.read_record()? {
                None => return Ok(None),
                Some((k, v)) => {
                    match k.as_slice().cmp(key) {
                        std::cmp::Ordering::Equal => return Ok(Some(v)),
                        std::cmp::Ordering::Greater => return Ok(None),
                        std::cmp::Ordering::Less => continue,
                    }
                }
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
        let mut out = Vec::with_capacity(self.num_records as usize);
        for _ in 0..self.num_records {
            if let Some(rec) = self.read_record()? {
                out.push(rec);
            } else {
                break;
            }
        }
        Ok(out)
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
    fn bloom_basics() {
        let mut bf = BloomFilter::new(64, 2);
        bf.insert(b"hello");
        assert!(bf.maybe_contains(b"hello"));
        // "world" ist nie eingefügt → praktisch sicher false (bei 64 bit könnte false positive vorkommen)
    }
}