use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};

/// Verfolgt den persistenten Zustand der SSTable-Segmente: welche Tabellen in
/// welchem Level liegen und welche Tabellen-ID als Nächstes vergeben wird.
/// Wird atomar ersetzt (Tmp-Datei + rename), damit ein Crash nie ein halbes
/// Manifest hinterlässt.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    pub levels: Vec<Vec<u64>>,
    pub next_table_id: u64,
}

impl Manifest {
    pub fn new() -> Manifest {
        Manifest {
            levels: Vec::new(),
            next_table_id: 1,
        }
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
                    let ids: Vec<u64> = parts
                        .filter_map(|s| s.parse().ok())
                        .collect();
                    while m.levels.len() <= level {
                        m.levels.push(Vec::new());
                    }
                    m.levels[level] = ids;
                }
                _ => {}
            }
        }
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
        let tmp = path.with_extension("manifest.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(buf.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Alle Tabellen-IDs, unabhängig vom Level.
    pub fn all_ids(&self) -> Vec<u64> {
        self.levels.iter().flatten().copied().collect()
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
        m.levels = vec![vec![1, 2], vec![3, 4, 5]];
        m.save(&path).unwrap();

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.next_table_id, 42);
        assert_eq!(loaded.levels, vec![vec![1, 2], vec![3, 4, 5]]);
    }
}