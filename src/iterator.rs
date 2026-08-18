use crate::memtable::Entry;

/// Eine eingelegte, sortierte Datenquelle (bereits vollständig geladen).
type Source = Vec<(Vec<u8>, Entry)>;

/// Merge über mehrere sortierte Quellen zu einem sortierten Stream.
///
/// Konvention: **Quelle mit kleinem Index ist NEUER** (fresher).
/// Quelle 0 = MemTable, dann Level 0, Level 1, ... Bei gleichen Keys gewinnt
/// die neuere (kleinere Index) Quelle. Alte Quellen werden nur übersprungen.
pub struct MergedIter {
    sources: Vec<Source>,
    cursors: Vec<usize>,
}

impl MergedIter {
    pub fn new(sources: Vec<Source>) -> MergedIter {
        let cursors = vec![0usize; sources.len()];
        MergedIter { sources, cursors }
    }
}

impl Iterator for MergedIter {
    type Item = (Vec<u8>, Entry);

    fn next(&mut self) -> Option<Self::Item> {
        // Kleinsten Key über alle Quellen finden.
        let mut best_key: Option<&[u8]> = None;
        for (si, src) in self.sources.iter().enumerate() {
            let cur = self.cursors[si];
            if cur < src.len() {
                let key = src[cur].0.as_slice();
                if best_key.map_or(true, |bk| key < bk) {
                    best_key = Some(key);
                }
            }
        }
        let best_key = best_key?;

        // Gewinner = neueste Quelle (kleinster Index) mit diesem Key.
        let mut winner: Option<(Vec<u8>, Entry)> = None;
        for (si, src) in self.sources.iter().enumerate() {
            let cur = self.cursors[si];
            if cur < src.len() && src[cur].0.as_slice() == best_key {
                if winner.is_none() {
                    winner = Some((src[cur].0.clone(), src[cur].1.clone()));
                }
                self.cursors[si] += 1; // diese Quelle immer weitersetzen
            }
        }
        winner
    }
}

/// Sammlung der aktuellen Lesequellen: MemTable + alle SSTable-IDs je Level.
#[derive(Debug, Clone)]
pub struct ReadSnapshot {
    /// MemTable-Inhalt (neueste Quelle).
    pub memtable: Vec<(Vec<u8>, Entry)>,
    /// Sortierte Listen: Level 0, Level 1, ...
    pub levels: Vec<Vec<(Vec<u8>, Entry)>>,
}

impl ReadSnapshot {
    pub fn empty() -> ReadSnapshot {
        ReadSnapshot {
            memtable: Vec::new(),
            levels: Vec::new(),
        }
    }

    /// Baut den MergedIter mit korrekter Priorität (MemTable → Level 0 → ...).
    pub fn merge(self) -> MergedIter {
        let mut sources = vec![self.memtable];
        sources.extend(self.levels);
        MergedIter::new(sources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedups_newest_wins() {
        // Quelle 0 (neu): k=b -> v=new
        // Quelle 1 (alt): k=a, k=b -> v=old
        let s0 = vec![(b"b".to_vec(), Some(b"new".to_vec()))];
        let s1 = vec![
            (b"a".to_vec(), Some(b"1".to_vec())),
            (b"b".to_vec(), Some(b"old".to_vec())),
        ];
        let mut it = MergedIter::new(vec![s0, s1]);
        let first = it.next().unwrap();
        assert_eq!(first.0, b"a".to_vec());
        let second = it.next().unwrap();
        assert_eq!(second.0, b"b".to_vec());
        assert_eq!(second.1, Some(b"new".to_vec()));
        assert!(it.next().is_none());
    }

    #[test]
    fn tombstone_shadows_old_value() {
        // Quelle 0 (neu): k=x gelöscht (None)
        // Quelle 1 (alt): k=x -> v=old
        let s0 = vec![(b"x".to_vec(), None)];
        let s1 = vec![(b"x".to_vec(), Some(b"old".to_vec()))];
        let mut it = MergedIter::new(vec![s0, s1]);
        let rec = it.next().unwrap();
        assert_eq!(rec.0, b"x".to_vec());
        assert_eq!(rec.1, None); // Tombstone gewinnt
        assert!(it.next().is_none());
    }
}