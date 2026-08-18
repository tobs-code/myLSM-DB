use std::path::Path;

use crate::error::Result;
use crate::memtable::Entry;
use crate::sstable::TableBuilder;

/// Schreibt einen bereits sortierten Datensatz-Stream als neue SSTable.
/// Wird von der Compaction und vom Flush genutzt.
pub fn build_table_from_sorted(path: &Path, records: &[(Vec<u8>, Entry)]) -> Result<u64> {
    let mut builder = TableBuilder::new(path)?;
    for (key, value) in records {
        builder.add(key, value.as_deref())?;
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_sorted_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sst");
        let records = vec![
            (b"a".to_vec(), Some(b"1".to_vec())),
            (b"b".to_vec(), None),
        ];
        let n = build_table_from_sorted(&path, &records).unwrap();
        assert_eq!(n, 2);
        let mut r = crate::sstable::TableReader::open(&path).unwrap();
        assert_eq!(r.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(r.get(b"b").unwrap(), None);
    }
}