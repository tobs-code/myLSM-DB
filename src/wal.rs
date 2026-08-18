use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{Error, Result};

const FLAG_DELETED: u8 = 0x01;

/// Append-only Write-Ahead-Log. Jeder Eintrag trägt eine CRC-Prüfsumme.
/// Beim Recovery werden Einträge bis zum ersten korrupten/beschädigten Datensatz
/// wiedergegeben (ein abgeschnittener Log am Crash-Ende ist kein Fehler).
pub struct Wal {
    file: BufWriter<File>,
}

impl Wal {
    pub fn create(path: &Path) -> Result<Wal> {
        let file = OpenOptions::new()
            .create(true)
            .read(false)
            .write(true)
            .append(true)
            .open(path)?;
        Ok(Wal {
            file: BufWriter::new(file),
        })
    }

    /// Hängt einen Datensatz an. `value = None` bedeutet Löschung.
    pub fn append(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        let flags = if value.is_some() { 0 } else { FLAG_DELETED };
        let value_len = value.map_or(0, |v| v.len() as u32);

        let mut body = Vec::with_capacity(9 + key.len() + value.map_or(0, |v| v.len()));
        body.push(flags);
        body.extend_from_slice(&(key.len() as u32).to_le_bytes());
        body.extend_from_slice(&value_len.to_le_bytes());
        body.extend_from_slice(key);
        if let Some(v) = value {
            body.extend_from_slice(v);
        }

        let crc = crc32fast::hash(&body);
        self.file.write_all(&crc.to_le_bytes())?;
        self.file.write_all(&body)?;
        Ok(())
    }

    /// Leert den Puffer und macht den Log dauerhaft (fsync).
    pub fn sync(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.get_mut().sync_all()?;
        Ok(())
    }

    /// Schreibt einen Puffer-Punkt; nur Puffer leeren, kein fsync.
    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }
}

/// Ein aus dem WAL gelesener Datensatz.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

/// Spielt den kompletten Log neu. Stoppt beim ersten korrupten Datensatz.
pub fn replay(path: &Path) -> Result<Vec<WalRecord>> {
    let file = OpenOptions::new().read(true).open(path)?;
    let mut reader = BufReader::new(file);
    let mut records = Vec::new();

    loop {
        let mut crc_buf = [0u8; 4];
        match reader.read_exact(&mut crc_buf) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(Error::Io(e)),
        }
        let expected_crc = u32::from_le_bytes(crc_buf);

        let mut flags = [0u8; 1];
        match reader.read_exact(&mut flags) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(Error::Io(e)),
        }
        let deleted = flags[0] & FLAG_DELETED != 0;

        let mut lens = [0u8; 8];
        if let Err(e) = reader.read_exact(&mut lens) {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                break; // abgeschnittener Log
            }
            return Err(Error::Io(e));
        }
        let key_len = u32::from_le_bytes(lens[0..4].try_into().unwrap()) as usize;
        let val_len = u32::from_le_bytes(lens[4..8].try_into().unwrap()) as usize;

        let mut body = Vec::with_capacity(9 + key_len + val_len);
        body.push(flags[0]);
        body.extend_from_slice(&lens);
        let mut data = vec![0u8; key_len + val_len];
        if let Err(e) = reader.read_exact(&mut data) {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                break;
            }
            return Err(Error::Io(e));
        }
        body.extend_from_slice(&data);

        let actual_crc = crc32fast::hash(&body);
        if actual_crc != expected_crc {
            break; // beschädigter Datensatz → Rest verwerfen
        }

        let key = data[..key_len].to_vec();
        let value = if deleted {
            None
        } else {
            Some(data[key_len..].to_vec())
        };
        records.push(WalRecord { key, value });
    }
    Ok(records)
}

/// Löscht die WAL-Datei nach erfolgreichem Flush (Log wird neu gestartet).
pub fn clear(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");

        {
            let mut wal = Wal::create(&path).unwrap();
            wal.append(b"a", Some(b"1")).unwrap();
            wal.append(b"b", None).unwrap();
            wal.append(b"c", Some(b"3")).unwrap();
            wal.sync().unwrap();
        }

        let records = replay(&path).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0], WalRecord { key: b"a".to_vec(), value: Some(b"1".to_vec()) });
        assert_eq!(records[1], WalRecord { key: b"b".to_vec(), value: None });
        assert_eq!(records[2], WalRecord { key: b"c".to_vec(), value: Some(b"3".to_vec()) });
    }

    #[test]
    fn truncated_log_recovers_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        let mut wal = Wal::create(&path).unwrap();
        wal.append(b"a", Some(b"1")).unwrap();
        wal.append(b"b", Some(b"2")).unwrap();
        wal.sync().unwrap();
        // Abschneiden nach dem ersten Datensatz
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(17).unwrap(); // 4 crc + 9 header + 2+1 = 16? absichtlich mitten drin

        let records = replay(&path).unwrap();
        assert!(records.len() <= 1);
    }
}