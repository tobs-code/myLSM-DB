# myLSM-DB

> ⚠️ **Work in Progress** — Dieses Projekt ist **noch nicht fertig** und befindet sich in aktiver Entwicklung. Die API und das Verhalten können sich jederzeit ändern. Nicht für Produktion geeignet.

Eine kleine, eigenständige **LSM-Engine** (Log-Structured Merge Tree) in Rust — die Grundlage einer eigenen Datenbank, geschrieben von Grund auf.

> **Das Ziel von v0.1:** eine zuverlässige Key-Value-Maschine, die `put`/`get`/`delete`/`scan` beherrscht, einen Write-Ahead-Log zur Crash-Recovery nutzt und SSTables kompaktiert. **Keine Entities, keine Query-Sprache, keine Indizes, keine Netzwerk-Schicht** — nur das solide Fundament.

---

## Inhaltsverzeichnis

- [Architektur](#architektur)
- [Schreibpfad](#schreibpfad)
- [Lesepfad](#lesepfad)
- [Compaction](#compaction)
- [Crash-Recovery](#crash-recovery)
- [Datenformate](#datenformate)
- [Public API](#public-api)
- [Konfiguration](#konfiguration)
- [Projektstruktur](#projektstruktur)
- [Verwendung](#verwendung)
- [Tests](#tests)
- [Bekannte Einschränkungen](#bekannte-einschränkungen)
- [Roadmap](#roadmap)

---

## Architektur

```
             put / get / delete / scan
                        │
                        ▼
              ┌─────────────────────┐
              │      Database       │   lib.rs (öffentliche API)
              └─────────┬───────────┘
                        │
         ┌──────────────┼──────────────┐
         ▼              ▼              ▼
     MemTable        Merge-Iterator    Manifest
     (RAM)              │              (Level-Liste)
         │              │
         ▼              ▼
      flush ◄─────   SSTables (sstable.rs)
                        │
                ┌───────┴───────┐
                ▼               ▼
            WAL (wal.rs)    Compaction (compaction.rs)
                │
                ▼
             DISK
```

**Grundidee:** Neue Schreibzugriffe landen zuerst in der MemTable (RAM) und im Write-Ahead-Log (Platte). Wird die MemTable zu groß, wird sie als unveränderliche **SSTable** (Sorted String Table) geflusht. Viele kleine SSTables werden im Hintergrund zu größeren zusammengeführt (**Compaction**). So werden zufällige Schreibzugriffe in sequenzielle umgewandelt — schnell für write-heavy Workloads.

---

## Schreibpfad

```
db.put(key, value)
   │
   ▼ 1. Append ans WAL (crc-gesichert)
   ▼ 2. In die MemTable (byte-sortiert)
   │
   ├─ MemTable voll?
   │     └─ ja → flush():
   │           │  SSTable schreiben (Level 0)
   │           │  Manifest aktualisieren + atomar speichern
   │           │  WAL leeren
   │           │  MemTable zurücksetzen
   │           └─ Level 0 zu voll? → compact_level(0)
   │
   └─ nein → fertig
```

- **Delete** wird als *Tombstone* (leerer Wert) in MemTable und WAL geschrieben — der Key bleibt solange sichtbar, bis die Compaction ihn endgültig entfernt.

---

## Lesepfad

`get` / `scan` mergen die Quellen **neueste zuerst**, damit der neueste Wert gewinnt:

1. **MemTable** (neueste Quelle) — autoritativ, wird zuerst geprüft.
2. **Level 0**, **Level 1**, … (älter) — per `TableReader::get` (mit Bloom-Filter) bzw. Merge-Iterator.

Für `scan` werden alle Quellen über einen `MergedIter` zusammengeführt. Bei gleichen Keys gewinnt die **kleinere Quell-Index-Nummer** (frischer).

---

## Compaction

- Nach dem Flush: Wenn **Level 0** ≥ `l0_compact_threshold` Tabellen hat, wird Level 0 in **Level 1** gemergt.
- Die Tabellen beider Ebenen werden gelesen, per Merge-Iterator dedupliziert (neuere Werte gewinnen, Tombstones überschatten alte Werte) und als **eine** neue SSTable in Level 1 geschrieben.
- Die alten Tabellen werden aus Manifest und Platte entfernt.
- Hinweis: v0.1 führt genau **einen** Merge-Schritt pro Flush aus (kein mehrstufiges Level-System).

---

## Crash-Recovery

Beim Start `Database::open`:

1. **Manifest** laden → weiß, welche SSTables existieren und in welchem Level.
2. **WAL** neu einspielen (`wal::replay`) → ungeflushte Schreibzugriffe aus der MemTable rekonstruieren.

Das WAL ist damit der **Durability-Mechanismus**, die SSTables der **persistierte Zustand**. Ein abgeschnittener oder beschädigter Log-Eintrag (typisch bei einem Crash mitten im Schreiben) wird ignoriert — alles davor bleibt gültig.

---

## Datenformate

### WAL-Datensatz (`wal.log`)

```
[u32 crc][u8 flags][u32 key_len][u32 val_len][key][value]
```

- `flags` Bit 0 = gelöscht (dann ist `val_len` = 0).
- CRC wird über den Body berechnet; `fsync` über `sync()`.

### SSTable (`NNNNNN.sst`)

```
[records] [sparse index] [bloom filter] [footer]
```

- **Records:** `[u32 key_len][u32 val_len][u8 flags][key][value]`
- **Sparse Index:** alle `spacing` (16) Records ein Eintrag `[key_len][key][offset]` → binäre Suche für Punkt-Lookups.
- **Bloom-Filter:** `[u32 nbits][u32 k][bits]` → sagt zuverlässig "definitiv nicht enthalten".
- **Footer (44 Bytes):** `index_offset, index_len, bloom_offset, bloom_len, spacing (usize), num_records, magic`. Über die Footer-Offsets wird Index und Bloom direkt adressiert.

### Manifest (`MANIFEST`)

```
N <next_table_id>
L <level> <table_id> ...
```

Atomar ersetzt (Temp-Datei + rename), damit nie ein halbes Manifest entsteht.

---

## Public API

| Methode | Beschreibung |
|---|---|
| `Database::open(dir)` | Öffnet oder erstellt eine DB, führt Recovery durch. |
| `Database::open_with(dir, opts)` | Wie oben, mit eigener Konfiguration. |
| `db.put(key, value)` | Setzt einen Wert. |
| `db.delete(key)` | Löscht einen Key (Tombstone). |
| `db.get(key)` | Liefert `Option<Vec<u8>>` (`None` = nicht vorhanden oder gelöscht). |
| `db.scan(start, end)` | Sortierter Bereichs-Scan, liefert `Vec<(Vec<u8>, Option<Vec<u8>>)>`. |
| `db.flush()` | Erzwingt das Flushen der MemTable als SSTable. |
| `db.close()` | **Sauber schließen:** MemTable flushen, WAL + Manifest synchen. |
| `db.table_count()` | Anzahl bekannter SSTables. |
| `db.level_tables(level)` | Anzahl Tabellen in einem Level. |
| `db.level_count()` | Anzahl Level. |

> **Hinweis zu `close()`:** Das ist der primäre Durability-Mechanismus für einen sauberen Shutdown. `drop(db)` ist nur ein **Best-Effort-Fallback** (flusht beim Verwerfen, ignoriert Fehler) — es ist keine Durability-Garantie. Rufe `close()` bewusst auf.

---

## Konfiguration

```rust
use my_lsm_db::{Database, Options};

let opts = Options {
    memtable_limit: 4 * 1024 * 1024, // Byte, ab wann geflusht wird
    l0_compact_threshold: 4,          // ab wie vielen L0-Tabellen kompaktiert wird
};
```

---

## Projektstruktur

```
my-lsm-db/
├── Cargo.toml
├── src/
│   ├── lib.rs        → Database, Options, öffentliche API, Flush/Compaction
│   ├── error.rs      → Fehlertypen (Io, Corrupt, NotFound, InvalidFormat)
│   ├── wal.rs        → append-only Log, CRC, replay, clear
│   ├── memtable.rs   → byte-sortierte In-Memory-Struktur (BTreeMap)
│   ├── sstable.rs    → TableBuilder/TableReader + BloomFilter + sparse Index
│   ├── manifest.rs   → persistentes SSTable-Set je Level, atomares Speichern
│   ├── compaction.rs → neue SSTable aus sortiertem Stream schreiben
│   └── iterator.rs   → Merge-Iterator über mehrere sortierte Quellen
└── tests/
    └── engine.rs     → Integrationstests (Flush, Recovery, Compaction, ...)
```

---

## Verwendung

Als Dependency in `Cargo.toml`:

```toml
[dependencies]
my-lsm-db = { path = "../my-lsm-db" }
```

```rust
use my_lsm_db::Database;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db = Database::open("./data")?;

    db.put(b"user:1:name", b"Tobias")?;
    db.put(b"user:1:age", b"29")?;
    db.put(b"user:2:name", b"Eva")?;

    assert_eq!(db.get(b"user:1:name")?, Some(b"Tobias".to_vec()));

    for (key, value) in db.scan(Some(b"user:"), Some(b"user;"))? {
        println!("{:?} = {:?}", key, value);
    }

    db.delete(b"user:1:age")?;
    assert_eq!(db.get(b"user:1:age")?, None);

    Ok(())
}
```

---

## Tests

```bash
cargo test
```

- **10 Unit-Tests** (WAL-Roundtrip/Recovery, MemTable, SSTable-Build/Read, Bloom, Manifest, Merge-Iterator, Compaction)
- **6 Integrationstests** (`tests/engine.rs`): put/get/delete, Range-Scan, Flush + Compaction, WAL-Recovery, Recovery nach Flush+Compaction, Overwrite-Neueste-gewinnt, Tombstone-Verhalten.

---

## Bekannte Einschränkungen (v0.1.1)

- **Einzelprozess, ein Thread** — kein MVCC, keine Sperren, keine Nebenläufigkeit.
- `scan` lädt alle Datensätze in den Speicher (kein Streaming-Iterator über die Platte).
- Compaction führt nur **einen** Merge-Schritt pro Flush aus (kein mehrstufiges Level-System, keine selektive Compaction).
- Bloom-Filter sind fest auf `1024` Bit gesetzt, nicht an die Datenmenge angepasst.
- Der `TableCache` hält Reader im RAM; bei sehr vielen Tabellen wächst er entsprechend.

---

## v0.1.1 — Härtungsrunde

Ergänzt nach der initialen v0.1:

- **Clean-Shutdown:** `close()` (primär) + `Drop` (Best-Effort-Fallback).
- **Benchmark-Binary:** `cargo run --release --example bench [n]` misst put / random get / sequential get / scan / flush / reopen+recovery und berichtet `writes/s`, `reads/s`, `MB/s`, DB-/WAL-/SSTable-Größe.
- **Crash-Test-Harness:** `src/bin/crash_tester.rs` (Modi `seed`/`write`/`verify`) + `tests/crash.rs` — schreibt Keys, **killt den Prozess per `abort` an einem zufälligen Punkt** und verifiziert danach, dass keine Korruption entstanden ist (20 Runs, O(N)-Verify via `scan`).
- **`get`-Optimierung:** Behebt ein echtes O(N²)-Problem. Ursprünglich baute jedes `get` eine volle Snapshot (liest alle SSTables in den RAM) → katastrophal langsam. Jetzt:
  - gezielter **Punkt-Lookup** (neueste Quelle zuerst, Bloom + sparse Index), O(Anzahl Tabellen)
  - **`TableReader::lookup`** unterscheidet sauber "vorhanden (auch Tombstone)" von "fehlt"
  - **TableCache** (hält geöffnete Reader, invalidiert bei Flush/Compaction)
  - **Index-Cache** (sparse Index einmalig beim Öffnen geparst)

**Messwerte (Release, 100k Keys, eine SSTable):**

| Metrik | vor Härtung | nach Härtung |
|---|---|---|
| put | 1,5M/s | 1,52M/s |
| random get | ~230 reads/s | **132k reads/s** |
| sequential get | ~250 reads/s | **211k reads/s** |
| scan | 33 MB/s | 43 MB/s |
| reopen+recover | — | 0,16s |

---

## Roadmap

| Version | Inhalt |
|---|---|
| **v0.1** ✅ | LSM-Engine: `put`/`get`/`delete`/`scan`, WAL, MemTable, SSTable, Bloom, Compaction, Recovery |
| **v0.1.1** ✅ | Härtung: Clean-Shutdown, Benchmark, Crash-Test, `get`-Optimierung (Punkt-Lookup + Caches) |
| **v0.2** | Entity-Layer (Wide-Column-Encoding `collection\|entity\|field\|version`) |
| **v0.3** | Secondary Indexes |
| **v0.4** | Transactions (PREPARE/COMMIT über WAL, Index-Konsistenz) |
| **v0.5** | Query-Planner / Query-Optimizer |

Das Gesamtkonzept (LSM-Engine + Entity-Modell + Indexes + Query-Optimizer) ist in [`konzept-kombination.md`](../konzept-kombination.md) beschrieben.