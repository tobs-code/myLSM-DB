use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};

/// Metadaten eines L1-Segments (eine SSTable mit disjunkter Key-Range).
///
/// `min_key`/`max_key` sind inklusiv. Die Segment-Liste im Manifest ist streng
/// nach `min_key` sortiert und disjunkt: `seg[i].max_key < seg[i+1].min_key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentMeta {
    pub file_id: u64,
    pub min_key: Vec<u8>,
    pub max_key: Vec<u8>,
    pub records: u64,
}

/// Verfolgt den persistenten Zustand der SSTable-Segmente: welche Tabellen in
/// welchem Level liegen (L0 + zukünftige Level als id-Listen), die L1-Segmente
/// mit Key-Ranges und welche Tabellen-ID als Nächstes vergeben wird.
/// Wird atomar ersetzt (Tmp-Datei + rename), damit ein Crash nie ein halbes
/// Manifest hinterlässt.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    pub levels: Vec<Vec<u64>>,
    /// L1-Segmente, sortiert nach `min_key`, disjunkt (§9.1 der Design-Spez).
    pub segments: Vec<SegmentMeta>,
    pub next_table_id: u64,
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(Error::InvalidFormat("segment hex range odd length".into()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for i in (0..bytes.len()).step_by(2) {
        let hi = (bytes[i] as char).to_digit(16).ok_or_else(|| {
            Error::InvalidFormat(format!("segment hex range bad char at {i}"))
        })?;
        let lo = (bytes[i + 1] as char).to_digit(16).ok_or_else(|| {
            Error::InvalidFormat(format!("segment hex range bad char at {}", i + 1))
        })?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

impl Manifest {
    pub fn new() -> Manifest {
        Manifest {
            levels: Vec::new(),
            segments: Vec::new(),
            next_table_id: 1,
        }
    }

    /// Validiert die Invarianten der Segment-Liste (sortiert + disjunkt) und
    /// die Eindeutigkeit der Datei-IDs. Hard errors, kein stilles Droppen.
    pub(crate) fn validate(&self) -> Result<()> {
        for w in self.segments.windows(2) {
            if w[0].min_key >= w[1].min_key {
                return Err(Error::InvalidFormat(
                    "manifest segments not strictly sorted by min_key".into(),
                ));
            }
            if w[0].max_key >= w[1].min_key {
                return Err(Error::InvalidFormat(
                    "manifest segments overlap (not disjoint)".into(),
                ));
            }
        }
        for s in &self.segments {
            if s.min_key > s.max_key {
                return Err(Error::InvalidFormat(
                    "manifest segment min_key > max_key".into(),
                ));
            }
        }
        let mut seen: Vec<u64> = self.all_ids();
        seen.sort_unstable();
        if seen.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::InvalidFormat(
                "manifest duplicate table id".into(),
            ));
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Manifest> {
        if !path.exists() {
            return Ok(Manifest::new());
        }
        let text = fs::read_to_string(path)?;
        let mut m = Manifest::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("N") => {
                    m.next_table_id = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("manifest N line".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("manifest id".into()))?;
                }
                Some("L") => {
                    let level: usize = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("manifest L level".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("manifest level".into()))?;
                    let ids: Vec<u64> = parts.filter_map(|s| s.parse().ok()).collect();
                    while m.levels.len() <= level {
                        m.levels.push(Vec::new());
                    }
                    m.levels[level] = ids;
                }
                Some("S") => {
                    // S 1 <file_id> <min_hex> <max_hex> <records>
                    let _level: usize = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("manifest S level".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("manifest S level".into()))?;
                    let file_id: u64 = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("manifest S id".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("manifest S id".into()))?;
                    let min_hex = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("manifest S min".into()))?;
                    let max_hex = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("manifest S max".into()))?;
                    let records: u64 = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("manifest S records".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("manifest S records".into()))?;
                    m.segments.push(SegmentMeta {
                        file_id,
                        min_key: hex_decode(min_hex)?,
                        max_key: hex_decode(max_hex)?,
                        records,
                    });
                }
                _ => {}
            }
        }
        m.validate()?;
        Ok(m)
    }

    /// Schreibt das Manifest atomar auf die Platte.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut buf = String::new();
        buf.push_str(&format!("N {}\n", self.next_table_id));
        for (level, ids) in self.levels.iter().enumerate() {
            if ids.is_empty() {
                continue;
            }
            let joined: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
            buf.push_str(&format!("L {} {}\n", level, joined.join(" ")));
        }
        for s in &self.segments {
            buf.push_str(&format!(
                "S 1 {} {} {} {}\n",
                s.file_id,
                hex_encode(&s.min_key),
                hex_encode(&s.max_key),
                s.records
            ));
        }
        let tmp = path.with_extension("manifest.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(buf.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Alle Tabellen-IDs, unabhängig von Level/Segment.
    pub fn all_ids(&self) -> Vec<u64> {
        let mut out: Vec<u64> = self.levels.iter().flatten().copied().collect();
        out.extend(self.segments.iter().map(|s| s.file_id));
        out
    }

    pub fn level_of(&self, id: u64) -> Option<usize> {
        self.levels.iter().position(|ids| ids.contains(&id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("MANIFEST");
        let mut m = Manifest::new();
        m.next_table_id = 42;
        m.levels = vec![vec![1, 2]];
        m.segments = vec![
            SegmentMeta {
                file_id: 3,
                min_key: b"a".to_vec(),
                max_key: b"m".to_vec(),
                records: 7,
            },
            SegmentMeta {
                file_id: 5,
                min_key: b"n".to_vec(),
                max_key: b"z".to_vec(),
                records: 9,
            },
        ];
        m.save(&path).unwrap();

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.next_table_id, 42);
        assert_eq!(loaded.levels, vec![vec![1, 2]]);
        assert_eq!(loaded.segments, m.segments);
        assert_eq!(loaded.all_ids(), vec![1, 2, 3, 5]);
    }

    #[test]
    fn rejects_overlapping_segments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("MANIFEST");
        // Disjunkt: [a..m] = 61/6d, [n..z] = 6e/7a → ok
        std::fs::write(
            &path,
            "N 10\nL 0 1\nS 1 3 61 6d 7\nS 1 5 6e 7a 9\n",
        )
        .unwrap();
        assert!(Manifest::load(&path).is_ok());
        // Überlappend: [a..z] = 61/7a und [m..z] = 6d/7a → hart abgelehnt
        std::fs::write(&path, "N 10\nS 1 3 61 7a 5\nS 1 5 6d 7a 5\n").unwrap();
        assert!(matches!(
            Manifest::load(&path),
            Err(Error::InvalidFormat(_))
        ));
    }
}