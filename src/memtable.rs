use std::collections::BTreeMap;
use std::ops::Bound;

/// Ein Eintrag in der MemTable. `None` = Tombstone (gelöscht).
pub type Entry = Option<Vec<u8>>;

/// In-Memory, byte-sortierte Struktur (BTreeMap). Alle Schreibzugriffe landen hier,
/// bis sie als SSTable geflusht werden.
#[derive(Default)]
pub struct MemTable {
    map: BTreeMap<Vec<u8>, Entry>,
    size_bytes: usize,
}

impl MemTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Setzt einen Wert oder löscht ihn (value = None → Tombstone).
    pub fn put(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) {
        let value_len = value.as_ref().map_or(0, Vec::len);
        if let Some(old) = self.map.insert(key.clone(), value) {
            self.size_bytes -= old.as_ref().map_or(0, Vec::len) + key.len();
        }
        self.size_bytes += value_len + key.len();
    }

    pub fn get(&self, key: &[u8]) -> Option<&Entry> {
        self.map.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Geschätzte Größe in Bytes (für Flush-Schwellenwert).
    pub fn size_bytes(&self) -> usize {
        self.size_bytes
    }

    /// Anzahl der Einträge (inkl. Tombstones).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Liefert einen Iterator über den Wertebereich [start, end).
    pub fn range(
        &self,
        start: Bound<Vec<u8>>,
        end: Bound<Vec<u8>>,
    ) -> impl Iterator<Item = (&[u8], &Entry)> {
        self.map.range((start, end)).map(|(k, v)| (k.as_slice(), v))
    }

    /// Liefert einen Iterator über ALLE Einträge.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &Entry)> {
        self.map.iter().map(|(k, v)| (k.as_slice(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_delete() {
        let mut m = MemTable::new();
        m.put(b"a".to_vec(), Some(b"1".to_vec()));
        m.put(b"b".to_vec(), Some(b"2".to_vec()));
        assert_eq!(m.get(b"a"), Some(&Some(b"1".to_vec())));
        assert_eq!(m.get(b"c"), None);
        m.put(b"a".to_vec(), None); // delete
        assert_eq!(m.get(b"a"), Some(&None));
    }

    #[test]
    fn range_sorted() {
        let mut m = MemTable::new();
        for (k, v) in [("b", "2"), ("a", "1"), ("c", "3")] {
            m.put(k.as_bytes().to_vec(), Some(v.as_bytes().to_vec()));
        }
        let keys: Vec<&[u8]> = m.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![&b"a"[..], &b"b"[..], &b"c"[..]]);
    }
}
