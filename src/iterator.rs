use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::error::Result;
use crate::memtable::Entry;

/// Eine (lazy) sortierte Datenquelle für den Merge-Iterator.
///
/// Implementierungen: [`crate::sstable::TableIter`] (über eine SSTable), und
/// [`VecSource`] (über einen bereits materialisierten, sortierten Vektor, z.B.
/// die — beschränkte — MemTable-Kopie oder Testdaten).
pub trait ScanSource {
    /// Aktuelles Element ohne Konsum. `Ok(None)` = Quelle erschöpft.
    fn peek(&mut self) -> Result<Option<(Vec<u8>, Entry)>>;
    /// Konsumiert das aktuelle Element.
    fn advance(&mut self) -> Result<()>;
}

/// Quellenart über einen bereits sortierten Vektor (Test, MemTable-Kopie).
pub struct VecSource {
    items: Vec<(Vec<u8>, Entry)>,
    pos: usize,
}

impl VecSource {
    pub fn new(items: Vec<(Vec<u8>, Entry)>) -> VecSource {
        VecSource { items, pos: 0 }
    }

    /// Materialisiert nur den Range `[start, end)` eines sortierten Vektors.
    pub fn subrange(
        items: Vec<(Vec<u8>, Entry)>,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> VecSource {
        let items = items
            .into_iter()
            .filter(|(k, _)| {
                start.is_none_or(|s| k.as_slice() >= s) && end.is_none_or(|e| k.as_slice() < e)
            })
            .collect();
        VecSource::new(items)
    }
}

impl ScanSource for VecSource {
    fn peek(&mut self) -> Result<Option<(Vec<u8>, Entry)>> {
        Ok(self.items.get(self.pos).cloned())
    }
    fn advance(&mut self) -> Result<()> {
        self.pos += 1;
        Ok(())
    }
}

/// Heap-Eintrag: der aktuelle Key einer Quelle plus deren Index.
///
/// `idx` kodiert die Aktualität: kleinerer Index = neuer (0 = MemTable, dann
/// Level 0, 1, …). Der Heap ist ein Max-Heap; wir definieren `cmp` so, dass der
/// kleinste Key (bei Gleichstand der kleinste `idx`) zuerst gepoppt wird.
#[derive(Eq, PartialEq)]
struct HeapEntry {
    key: Vec<u8>,
    idx: usize,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Größer = soll zuerst gepoppt werden = kleinerer Key, dann kleinerer idx.
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.idx.cmp(&self.idx))
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Lazy k-way Merge über mehrere sortierte Quellen.
///
/// - **Newest-source-wins:** Bei identischem Key gewinnt die Quelle mit dem
///   kleineren `idx` (höhere Aktualität); alle älteren Quellen werden für diesen
///   Key **vollständig übersprungen** (Duplicate-Key-Shading). Eine Tombstone
///   (`Entry = None`) ist dabei ein regulärer Wert und verhindert das
///   Durchscheinen älterer Werte.
/// - Speicher: O(Anzahl Quellen) — der Heap enthält nur Cursor, nie Records.
pub struct MergeIter {
    sources: Vec<Box<dyn ScanSource>>,
    heap: BinaryHeap<HeapEntry>,
    primed: bool,
}

impl MergeIter {
    pub fn new(sources: Vec<Box<dyn ScanSource>>) -> MergeIter {
        MergeIter {
            sources,
            heap: BinaryHeap::new(),
            primed: false,
        }
    }

    /// Legt den Heap mit dem aktuellen Key jeder Quelle an.
    fn prime(&mut self) -> Result<()> {
        for (idx, src) in self.sources.iter_mut().enumerate() {
            if let Some((k, _)) = src.peek()? {
                self.heap.push(HeapEntry { key: k, idx });
            }
        }
        Ok(())
    }

    /// Liefert das nächste (deduplizierte) Element.
    pub fn next(&mut self) -> Result<Option<(Vec<u8>, Entry)>> {
        if !self.primed {
            self.prime()?;
            self.primed = true;
        }
        let Some(top) = self.heap.pop() else {
            return Ok(None);
        };
        let key = top.key;
        let idx = top.idx;

        // Der neueste Cursor für `key` (per Heap-Tie-Break ist top.idx der kleinste).
        let item = self.sources[idx]
            .peek()?
            .expect("Heap-Eintrag entspricht einem peek");

        // Neueste Quelle für diesen Key vorrücken.
        self.sources[idx].advance()?;

        // Ältere Quellen mit demselben Key vollständig überspringen.
        while let Some(e) = self.heap.peek() {
            if e.key != key {
                break;
            }
            let e = self.heap.pop().unwrap();
            self.sources[e.idx].advance()?;
            if let Some((nk, _)) = self.sources[e.idx].peek()? {
                self.heap.push(HeapEntry {
                    key: nk,
                    idx: e.idx,
                });
            }
        }

        // Neueste Quelle wieder in den Heap.
        if let Some((nk, _)) = self.sources[idx].peek()? {
            self.heap.push(HeapEntry { key: nk, idx });
        }

        Ok(Some(item))
    }
}

/// Die (beschränkte) MemTable-Kopie, konvertiert in eine Lazy-Quelle.
pub fn memtable_source(
    memtable: &crate::memtable::MemTable,
    start: Option<&[u8]>,
    end: Option<&[u8]>,
) -> VecSource {
    let items: Vec<(Vec<u8>, Entry)> = memtable
        .iter()
        .map(|(k, v)| (k.to_vec(), v.clone()))
        .collect();
    VecSource::subrange(items, start, end)
}

/// Mergt mehrere bereits materialisierte, sortierte Vektoren (z.B. für die
/// Compaction). Nutzt denselben Merge-Mechanismus wie der Lazy-Pfad.
pub fn merge_vecs(sources: Vec<Vec<(Vec<u8>, Entry)>>) -> Result<Vec<(Vec<u8>, Entry)>> {
    let sources: Vec<Box<dyn ScanSource>> = sources
        .into_iter()
        .map(|items| Box::new(VecSource::new(items)) as Box<dyn ScanSource>)
        .collect();
    let mut merge = MergeIter::new(sources);
    let mut out = Vec::new();
    while let Some(x) = merge.next()? {
        out.push(x);
    }
    Ok(out)
}

/// Lazy-Scan über die Database.
///
/// Besitzt **exklusiv** `&mut Database` für seine gesamte Lebensdauer — dadurch
/// sind `put`/`delete`/`flush`/`compact` während der Iteration unmöglich, und
/// der beim Erzeugen festgelegte SSTable-Satz (plus MemTable-Kopie) bleibt
/// konsistent (Snapshot-Semantik durch Borrowing).
pub struct ScanIter<'a> {
    #[allow(dead_code)] // gehalten, um den exklusiven Borrow zu verlängern
    db: &'a mut crate::Database,
    merge: MergeIter,
}

impl<'a> ScanIter<'a> {
    pub fn new(db: &'a mut crate::Database, merge: MergeIter) -> ScanIter<'a> {
        ScanIter { db, merge }
    }
}

impl<'a> Iterator for ScanIter<'a> {
    type Item = Result<(Vec<u8>, Entry)>;
    fn next(&mut self) -> Option<Self::Item> {
        self.merge.next().transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(items: Vec<(&[u8], Entry)>) -> Box<dyn ScanSource> {
        let items = items.into_iter().map(|(k, v)| (k.to_vec(), v)).collect();
        Box::new(VecSource::new(items))
    }

    fn drain(mut m: MergeIter) -> Vec<(Vec<u8>, Entry)> {
        let mut out = Vec::new();
        while let Some(x) = m.next().unwrap() {
            out.push(x);
        }
        out
    }

    #[test]
    fn merges_sorted_dedup_newest_wins() {
        // Quelle 0 (neu): b=new
        // Quelle 1 (alt): a, b=old
        let s0 = src(vec![(b"b".as_slice(), Some(b"new".to_vec()))]);
        let s1 = src(vec![
            (b"a".as_slice(), Some(b"1".to_vec())),
            (b"b".as_slice(), Some(b"old".to_vec())),
        ]);
        let out = drain(MergeIter::new(vec![s0, s1]));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, b"a");
        assert_eq!(out[1].0, b"b");
        assert_eq!(out[1].1, Some(b"new".to_vec()));
    }

    #[test]
    fn tombstone_shadows_older_value() {
        // Quelle 0 (neu): x = Tombstone (None)
        // Quelle 1 (alt): x = old
        let s0 = src(vec![(b"x".as_slice(), None)]);
        let s1 = src(vec![(b"x".as_slice(), Some(b"old".to_vec()))]);
        let out = drain(MergeIter::new(vec![s0, s1]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, b"x");
        assert_eq!(out[0].1, None);
    }

    #[test]
    fn value_then_tombstone_then_older_value_sequence() {
        // Zwei Keys mit vollständigem Shadowing.
        // Quelle 0: a=new, b=Tombstone, d=T
        // Quelle 1: a=old, b=old, c=other, d=old
        let s0 = src(vec![
            (b"a".as_slice(), Some(b"new".to_vec())),
            (b"b".as_slice(), None),
            (b"d".as_slice(), None),
        ]);
        let s1 = src(vec![
            (b"a".as_slice(), Some(b"old".to_vec())),
            (b"b".as_slice(), Some(b"old".to_vec())),
            (b"c".as_slice(), Some(b"other".to_vec())),
            (b"d".as_slice(), Some(b"old".to_vec())),
        ]);
        let out = drain(MergeIter::new(vec![s0, s1]));
        let keys: Vec<Vec<u8>> = out.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(
            keys,
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
        assert_eq!(out[0].1, Some(b"new".to_vec()));
        assert_eq!(out[1].1, None); // Tombstone verhindert Durchscheinen
        assert_eq!(out[2].1, Some(b"other".to_vec()));
        assert_eq!(out[3].1, None);
    }

    #[test]
    fn key_in_multiple_sources_shadowed_once() {
        // gleicher Key in 4 Quellen; nur neueste (idx 0) zählt.
        let s0 = src(vec![(b"k".as_slice(), Some(b"v0".to_vec()))]);
        let s1 = src(vec![(b"k".as_slice(), Some(b"v1".to_vec()))]);
        let s2 = src(vec![(b"k".as_slice(), Some(b"v2".to_vec()))]);
        let s3 = src(vec![(b"k".as_slice(), Some(b"v3".to_vec()))]);
        let out = drain(MergeIter::new(vec![s0, s1, s2, s3]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, Some(b"v0".to_vec()));
    }

    #[test]
    fn empty_sources() {
        let out = drain(MergeIter::new(vec![src(vec![]), src(vec![])]));
        assert!(out.is_empty());
    }
}
