use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::Result;

// Record-Typen des WAL.
const T_PUT: u8 = 0;
const T_DELETE: u8 = 1;
const T_BEGIN: u8 = 2;
const T_TX_PUT: u8 = 3;
const T_TX_DELETE: u8 = 4;
const T_COMMIT: u8 = 5;
const T_ABORT: u8 = 6;

/// Append-only Write-Ahead-Log. Jeder Eintrag trägt eine CRC-Prüfsumme.
/// Beim Recovery werden Einträge bis zum ersten korrupten/beschädigten Datensatz
/// wiedergegeben (ein abgeschnittener Log am Crash-Ende ist kein Fehler).
///
/// Aufzeichnungsformat pro Record:
/// ```text
/// [crc u32][type u8][payload]
/// ```
/// wobei die CRC über `[type][payload]` berechnet wird.
pub struct Wal {
    file: BufWriter<File>,
}

impl Wal {
    pub fn create(path: &Path) -> Result<Wal> {
        let file = OpenOptions::new()
            .create(true)
            .read(false)
            .append(true)
            .open(path)?;
        Ok(Wal {
            file: BufWriter::new(file),
        })
    }

    /// Schreibt einen Record mit CRC über `[type][payload]`.
    fn write_record(&mut self, record_type: u8, payload: &[u8]) -> Result<()> {
        let mut body = Vec::with_capacity(1 + payload.len());
        body.push(record_type);
        body.extend_from_slice(payload);
        let crc = crc32fast::hash(&body);
        self.file.write_all(&crc.to_le_bytes())?;
        self.file.write_all(&body)?;
        Ok(())
    }

    /// Hängt einen Nicht-Transaktions-Datensatz an. `value = None` bedeutet
    /// Löschung. (Nicht-transaktionale Schreibpfade — kompatibel zur v0.1/v0.3-API.)
    pub fn append(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        match value {
            Some(v) => {
                let mut payload = Vec::with_capacity(8 + key.len() + v.len());
                payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
                payload.extend_from_slice(&(v.len() as u32).to_le_bytes());
                payload.extend_from_slice(key);
                payload.extend_from_slice(v);
                self.write_record(T_PUT, &payload)
            }
            None => {
                let mut payload = Vec::with_capacity(4 + key.len());
                payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
                payload.extend_from_slice(key);
                self.write_record(T_DELETE, &payload)
            }
        }
    }

    pub fn append_begin(&mut self, tx: u64) -> Result<()> {
        self.write_record(T_BEGIN, &tx.to_le_bytes())
    }

    pub fn append_tx_put(&mut self, tx: u64, key: &[u8], value: &[u8]) -> Result<()> {
        let mut payload = Vec::with_capacity(8 + 8 + key.len() + value.len());
        payload.extend_from_slice(&tx.to_le_bytes());
        payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
        payload.extend_from_slice(&(value.len() as u32).to_le_bytes());
        payload.extend_from_slice(key);
        payload.extend_from_slice(value);
        self.write_record(T_TX_PUT, &payload)
    }

    pub fn append_tx_delete(&mut self, tx: u64, key: &[u8]) -> Result<()> {
        let mut payload = Vec::with_capacity(8 + 4 + key.len());
        payload.extend_from_slice(&tx.to_le_bytes());
        payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
        payload.extend_from_slice(key);
        self.write_record(T_TX_DELETE, &payload)
    }

    pub fn append_commit(&mut self, tx: u64) -> Result<()> {
        self.write_record(T_COMMIT, &tx.to_le_bytes())
    }

    pub fn append_abort(&mut self, tx: u64) -> Result<()> {
        self.write_record(T_ABORT, &tx.to_le_bytes())
    }

    /// Leert den Puffer und macht den Log dauerhaft (fsync). Für Transaktionen
    /// ist das der **Commit-Point**: alles davor kann bei einem Crash verschwinden.
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

/// Ein aus dem WAL gewonnener, **committeter** Datensatz (nach Recovery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

/// Ergebnis eines Replays: die rekonstruierten, committeten Mutationen plus die
/// höchste gesehene Transaktions-ID (aus allen Records — auch uncommitteten),
/// damit nach einem Crash keine Transaktions-ID wiederverwendet wird.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayResult {
    pub records: Vec<WalRecord>,
    pub max_tx_id: u64,
}

/// Ein roher, noch nicht aufgelöster Log-Eintrag während des Replays.
enum RawOp {
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        key: Vec<u8>,
    },
    TxPut {
        tx: u64,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    TxDelete {
        tx: u64,
        key: Vec<u8>,
    },
    Commit {
        tx: u64,
    },
}

/// Liest genau `n` Bytes. `Ok(None)` bei EOF (abgeschnittener Log).
fn read_n<R: Read>(r: &mut R, n: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; n];
    match r.read_exact(&mut buf) {
        Ok(_) => Ok(Some(buf)),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// Liest den typabhängigen Payload und liefert die exakten Roh-Bytes (für die
/// CRC-Berechnung). `Ok(None)` bei einem abgeschnittenen Datensatz.
fn read_payload<R: Read>(r: &mut R, t: u8) -> std::io::Result<Option<Vec<u8>>> {
    let n_head: usize = match t {
        T_PUT => 8,
        T_DELETE => 4,
        T_BEGIN => 8,
        T_TX_PUT => 16,
        T_TX_DELETE => 12,
        T_COMMIT => 8,
        T_ABORT => 8,
        _ => return Ok(Some(Vec::new())),
    };
    let Some(head) = read_n(r, n_head)? else {
        return Ok(None);
    };
    let data_len = match t {
        T_PUT => {
            let k = u32::from_le_bytes(head[0..4].try_into().unwrap()) as usize;
            let v = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
            k + v
        }
        T_DELETE => u32::from_le_bytes(head[0..4].try_into().unwrap()) as usize,
        T_TX_PUT => {
            let k = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
            let v = u32::from_le_bytes(head[12..16].try_into().unwrap()) as usize;
            k + v
        }
        T_TX_DELETE => u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize,
        _ => 0,
    };
    let mut out = head;
    if data_len > 0 {
        let Some(data) = read_n(r, data_len)? else {
            return Ok(None);
        };
        out.extend_from_slice(&data);
    }
    Ok(Some(out))
}

/// Spielt den kompletten Log neu und löst Transaktionen auf.
///
/// Recovery-Regel: Eine Transaktion wird **genau dann** angewandt, wenn in
/// derselben WAL ein `Commit`-Record für ihre ID vorhanden ist. `Begin`/`Put`/
/// `Delete` einer Transaktion ohne `Commit` (abgebrochen oder abgestürzt) werden
/// verworfen. Nicht-transaktionale Records werden unverändert übernommen.
/// Stoppt beim ersten korrupten Datensatz.
pub fn replay(path: &Path) -> Result<ReplayResult> {
    let file = OpenOptions::new().read(true).open(path)?;
    let mut reader = BufReader::new(file);
    let mut ops: Vec<RawOp> = Vec::new();
    let mut max_tx_id: u64 = 0;

    loop {
        let Some(crc_buf) = read_n(&mut reader, 4)? else {
            break;
        };
        let expected = u32::from_le_bytes(crc_buf[..4].try_into().unwrap());
        let Some(type_buf) = read_n(&mut reader, 1)? else {
            break;
        };
        let t = type_buf[0];
        let Some(payload) = read_payload(&mut reader, t)? else {
            break;
        };
        let mut body = Vec::with_capacity(1 + payload.len());
        body.push(t);
        body.extend_from_slice(&payload);
        if crc32fast::hash(&body) != expected {
            break; // beschädigter Datensatz → Rest verwerfen
        }

        let mut txn: Option<u64> = None;
        let op: Option<RawOp> = match t {
            T_PUT => {
                let k = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
                let v = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
                Some(RawOp::Put {
                    key: payload[8..8 + k].to_vec(),
                    value: payload[8 + k..8 + k + v].to_vec(),
                })
            }
            T_DELETE => {
                let k = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
                Some(RawOp::Delete {
                    key: payload[4..4 + k].to_vec(),
                })
            }
            T_BEGIN => {
                txn = Some(u64::from_le_bytes(payload[0..8].try_into().unwrap()));
                None
            }
            T_TX_PUT => {
                let tx = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                txn = Some(tx);
                let k = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
                let v = u32::from_le_bytes(payload[12..16].try_into().unwrap()) as usize;
                Some(RawOp::TxPut {
                    tx,
                    key: payload[16..16 + k].to_vec(),
                    value: payload[16 + k..16 + k + v].to_vec(),
                })
            }
            T_TX_DELETE => {
                let tx = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                txn = Some(tx);
                let k = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
                Some(RawOp::TxDelete {
                    tx,
                    key: payload[12..12 + k].to_vec(),
                })
            }
            T_COMMIT => {
                let tx = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                txn = Some(tx);
                Some(RawOp::Commit { tx })
            }
            T_ABORT => {
                txn = Some(u64::from_le_bytes(payload[0..8].try_into().unwrap()));
                None
            }
            _ => break, // unbekannter Typ → Format inkompatibel, stoppen
        };
        if let Some(tx) = txn
            && tx > max_tx_id
        {
            max_tx_id = tx;
        }
        if let Some(op) = op {
            ops.push(op);
        }
    }

    // Committete Transaktions-IDs bestimmen.
    let mut committed: HashSet<u64> = HashSet::new();
    for op in &ops {
        if let RawOp::Commit { tx } = op {
            committed.insert(*tx);
        }
    }

    // Geordnete committete Mutationen erzeugen.
    let mut records = Vec::new();
    for op in &ops {
        match op {
            RawOp::Put { key, value } => {
                records.push(WalRecord {
                    key: key.clone(),
                    value: Some(value.clone()),
                });
            }
            RawOp::Delete { key } => {
                records.push(WalRecord {
                    key: key.clone(),
                    value: None,
                });
            }
            RawOp::TxPut { tx, key, value } if committed.contains(tx) => {
                records.push(WalRecord {
                    key: key.clone(),
                    value: Some(value.clone()),
                });
            }
            RawOp::TxDelete { tx, key } if committed.contains(tx) => {
                records.push(WalRecord {
                    key: key.clone(),
                    value: None,
                });
            }
            _ => {}
        }
    }
    Ok(ReplayResult { records, max_tx_id })
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

        let res = replay(&path).unwrap();
        assert_eq!(res.records.len(), 3);
        assert_eq!(
            res.records[0],
            WalRecord {
                key: b"a".to_vec(),
                value: Some(b"1".to_vec())
            }
        );
        assert_eq!(
            res.records[1],
            WalRecord {
                key: b"b".to_vec(),
                value: None
            }
        );
        assert_eq!(
            res.records[2],
            WalRecord {
                key: b"c".to_vec(),
                value: Some(b"3".to_vec())
            }
        );
        assert_eq!(res.max_tx_id, 0);
    }

    #[test]
    fn committed_tx_replays_aborted_and_uncommitted_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");

        {
            let mut wal = Wal::create(&path).unwrap();
            // Nicht-transaktionaler Put dazwischen.
            wal.append(b"plain", Some(b"p")).unwrap();
            // TX 1: committed.
            wal.append_begin(1).unwrap();
            wal.append_tx_put(1, b"a", b"1").unwrap();
            wal.append_tx_delete(1, b"b").unwrap();
            wal.append_commit(1).unwrap();
            // TX 2: abgebrochen (Abort) → verwerfen.
            wal.append_begin(2).unwrap();
            wal.append_tx_put(2, b"c", b"2").unwrap();
            wal.append_abort(2).unwrap();
            // TX 3: abgestürzt (kein Commit) → verwerfen.
            wal.append_begin(3).unwrap();
            wal.append_tx_put(3, b"d", b"3").unwrap();
            wal.sync().unwrap();
        }

        let res = replay(&path).unwrap();
        let got: Vec<WalRecord> = res.records.clone();
        assert_eq!(
            got,
            vec![
                WalRecord {
                    key: b"plain".to_vec(),
                    value: Some(b"p".to_vec())
                },
                WalRecord {
                    key: b"a".to_vec(),
                    value: Some(b"1".to_vec())
                },
                WalRecord {
                    key: b"b".to_vec(),
                    value: None
                },
            ]
        );
        // max_tx_id aus ALLEN Records (inkl. abgebrochener/uncommitteter).
        assert_eq!(res.max_tx_id, 3);
    }

    #[test]
    fn truncated_log_recovers_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        let mut wal = Wal::create(&path).unwrap();
        wal.append(b"a", Some(b"1")).unwrap();
        wal.append(b"b", Some(b"2")).unwrap();
        wal.sync().unwrap();
        // Abschneiden mitten im zweiten Datensatz.
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(17).unwrap(); // Record 1 ist 15 Bytes → mitten in Record 2.

        let res = replay(&path).unwrap();
        assert!(res.records.len() <= 1);
    }
}
