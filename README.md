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

Seit v0.4 löst `wal::replay` Transaktionen auf: Es wendet nur Transaktionen an, für die ein `Commit`-Record vorhanden ist, und verwirft alle Mutationen von abgebrochenen/abgestürzten Transaktionen. Damit überlebt ein committeter Block einen Crash vollständig, ein uncommitteter nicht.

---

## Datenformate

### WAL-Datensatz (`wal.log`)

Seit v0.4 ist jeder Datensatz typisiert:

```
[u32 crc][u8 type][payload]
```

Die CRC wird über `[type][payload]` berechnet; `fsync` über `sync()`.

| `type` | Bedeutung | Payload |
|---|---|---|
| `0` `Put` | Nicht-transaktionales Schreiben | `[u32 key_len][u32 val_len][key][value]` |
| `1` `Delete` | Nicht-transaktionales Löschen | `[u32 key_len][key]` |
| `2` `Begin` | Transaktions-Start | `[u64 tx]` |
| `3` `TxPut` | Transaktions-Schreiben | `[u64 tx][u32 key_len][u32 val_len][key][value]` |
| `4` `TxDelete` | Transaktions-Löschen | `[u64 tx][u32 key_len][key]` |
| `5` `Commit` | Transaktions-Commit | `[u64 tx]` |
| `6` `Abort` | Transaktions-Abbruch | `[u64 tx]` |

**Recovery-Regel:** Eine Transaktion wird genau dann angewandt, wenn ein `Commit`-Record für ihre ID vorhanden ist. `Begin`/`TxPut`/`TxDelete` einer Transaktion ohne `Commit` (abgebrochen oder abgestürzt) werden verworfen. Nicht-transaktionale Records werden unverändert übernommen.

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
| `EntityStore::open(dir)` | v0.2: öffnet Entity-Store (legt darunter eine KV-Engine an). |
| `store.collection(name)?` | v0.2: Handle auf eine Collection (stabile ID). |
| `col.put(entity_id, &entity)` | v0.2: ersetzt eine Entität (entfernt veraltete Felder). |
| `col.get(entity_id)` | v0.2: liefert `Option<Entity>`. |
| `col.delete(entity_id)` | v0.2: löscht eine Entität. |
| `col.create_index(field)` | v0.3: legt einen geordneten Index auf einem Feld an (auch mit Bestandsdaten). |
| `col.drop_index(field)` | v0.3: löscht Index-Definition + -Daten. |
| `col.find(field, FindOp)` | v0.3: Index-Abfrage, liefert verifizierte `Vec<EntityId>`. |
| `store.transaction()` | v0.4: eröffnet eine Transaktion (exklusiv, solange sie lebt). |
| `store.transaction_with(f)` | v0.4: führt `f` in einer Transaktion aus (committet bei `Ok`, bricht bei `Err`). |
| `tx.update(collection, id, &entity)` | v0.4: Entität innerhalb der Transaktion ersetzt/angelegt. |
| `tx.delete(collection, id)` | v0.4: Entität innerhalb der Transaktion löschen. |
| `tx.get(collection, id)` | v0.4: liest committete Daten + eigene uncommittete Writes. |
| `tx.scan_collection(collection)` | v0.4: scannt committete Daten + eigene Writes. |
| `tx.find(collection, field, FindOp)` | v0.4: Index-Abfrage inkl. eigener uncommitteter Writes. |
| `tx.commit()` | v0.4: committet atomar (WAL `Begin`→`TxPut`/`TxDelete`→`Commit`, fsync, dann MemTable). |
| `tx.abort()` | v0.4: verwirft alle uncommitteten Writes (schreibt nichts Persistentes). |

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
│   ├── iterator.rs   → Merge-Iterator über mehrere sortierte Quellen
│   ├── codec.rs      → v0.2: getypter Value-Codec (Null/Bool/Int/Float/String/Bytes)
│   ├── keycodec.rs   → v0.2: binäres Entity-Key-Encoding + Bereichs-Grenzen
│   ├── schema.rs     → v0.2/v0.3: persistente Collection-/Field-/Index-Registry
│   ├── ordering.rs   → v0.3: order-preserving Encoding für Index-Werte
│   ├── index.rs      → v0.3: Secondary Indexes (create/drop/find/rebuild)
│   └── entity.rs     → v0.2-v0.4: Entity + EntityStore + Transaction (put/get/delete, Reconstruction, Index-Maintenance)
└── tests/
    ├── engine.rs     → Integrationstests (Flush, Recovery, Compaction, ...)
    ├── entity.rs     → v0.2: Smoke-Tests (put/get, Persistenz über Reopen)
    ├── index.rs      → v0.3: Oracle-Test (find vs Full-Scan)
    └── transaction.rs → v0.4: Random-Modell-Oracle (commit/abort/crash/restart/index/entity)
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

- **48 Unit-Tests** (WAL-Roundtrip/Recovery + Transaktions-Replay, MemTable, SSTable-Build/Read, Bloom, Manifest, Merge-Iterator, Compaction, Codec, Schema, Entity + Transactions, Index-Maintenance)
- **6 Integrationstests** (`tests/engine.rs`): put/get/delete, Range-Scan, Flush + Compaction, WAL-Recovery, Recovery nach Flush+Compaction, Overwrite-Neueste-gewinnt, Tombstone-Verhalten.
- **2 Crash-Tests** (`tests/crash.rs`): Clean-Close-Persistenz + Crash-Recovery ohne Korruption.
- **1 Index-Oracle-Test** (`tests/index.rs`): 8000 zufällige Mutationen, `find` vs. Full-Scan identisch.
- **1 Transaktions-Oracle-Test** (`tests/transaction.rs`): Random-Modell-Tester mischt commit/abort/crash/restart/index/entity und prüft get/scan/find gegen ein In-Memory-Modell.

---

## Bekannte Einschränkungen

- **Einzelprozess, ein Thread** — kein MVCC, keine Sperren, keine Nebenläufigkeit. Seit v0.4 existiert genau **eine** Transaktion pro Store (`&mut`-Borrow); keine gleichzeitigen Schreiber.
- **Kein MVCC / keine Snapshot-Isolation** — eine Transaktion sieht committete Daten plus ihre eigenen Writes, aber keine anderen Isolationsstufen.
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

## v0.2 — Entity-Layer

Der **Entity-Layer** baut getypte Entitäten auf der unveränderten v0.1-KV-Maschine auf. Die KV-Engine bleibt "dumm" — sie kennt diese Schicht nicht; die Abhängigkeit geht nur in eine Richtung.

**Scope-Grenze (hart gehalten):** Entity-Layer + Typed Codec + Key-Encoding. **Keine** Sekundärindizes, Query-Sprache, Transactions, MVCC, Netzwerk, Replication.

```rust
use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};

let mut store = EntityStore::open("data")?;

let mut user = Entity::new();
user.insert("name", Value::String("Tobias".into()));
user.insert("age", Value::Int(31));
user.insert("active", Value::Bool(true));

store.collection("users")?.put("123", &user)?;

let got = store.collection("users")?.get("123")?.expect("exists");
assert_eq!(got["age"], Value::Int(31));
```

**Vier neue Bausteine:**

1. **Typed Value Codec** (`codec.rs`) — eindeutiges, versionierbares Binärformat `[type tag][payload]`. Typen: `Null`, `Bool`, `Int64`, `Float64`, `String`, `Bytes`. Später erweiterbar (`Timestamp`, `UUID`, `Array`, `Object`).
2. **Key Codec** (`keycodec.rs`) — kein String-Kleben, sondern length-/type-sicheres Binär-Encoding `[E][collection_id u32][entity_id_len][entity_id][field_id u32]`. Keys bleiben eindeutig auch bei Sonderzeichen in IDs. Liefert außerdem die Bereichs-Grenzen für Entity-Scans.
3. **Persistente Schema-Registry** (`schema.rs`) — weist `collection_id`/`field_id` **stabil und dauerhaft** zu (nicht `Hash(name)`, nicht neu pro Start), sonst würde die Bedeutung eines Keys zwischen Starts wechseln. Atomar gespeichert (`SCHEMA`), nur geschrieben wenn sie sich ändert.
4. **Entity-Reconstruction** (`entity.rs`) — `EntityStore` zerlegt ein `Entity` in Feld-Keys und rekonstruiert es beim Lesen. `put` ersetzt die Entität exakt (entfernt auch veraltete Felder); `get` liefert `None`, wenn keine lebenden Felder mehr existieren.

Intern landet alles weiterhin nur als KV-Zeilen:

```text
E|1|123|0 → [4]["Tobias"]
E|1|123|1 → [2][31]
E|1|123|2 → [1][1]
```

**Durability-Reihenfolge:** Die Schema-Registry wird (fsync) **vor** den Entitätsdaten persistiert, damit eine Feld-ID nach einem Crash nie eine andere Bedeutung hat.

| Komponente | Test-Ergebnis |
|---|---|
| Codec (encode/decode, alle Typen, Unicode, leer, negativ, groß) | ok |
| Key-Codec (roundtrip, Sonderzeichen, Bereichs-Grenzen, Sortierung) | ok |
| Schema (stabile IDs, save/load roundtrip, Reopen) | ok |
| Entity (put/get/delete, Stale-Field-Removal, Reopen, Unicode) | ok |
| Smoke-Tests (`tests/entity.rs`) | ok |

---

## v0.3 — Secondary Indexes

Getypte Secondary Indexes über der KV-Engine. Ein **einzelner, geordneter `Value`-Index** (order-preserving) deckt `=`, `<`, `<=`, `>`, `>=`, `between` ab — keine separaten Index-Implementierungen pro Operator.

```rust
use my_lsm_db::codec::Value;
use my_lsm_db::index::FindOp;

store.collection("users")?.create_index("age")?;

let eq = store.collection("users")?.find("age", FindOp::Eq(Value::Int(31)))?; // Vec<EntityId>
let range = store.collection("users")?
    .find("age", FindOp::Between(Value::Int(18), Value::Int(65)))?;
```

**Index-Key-Format:**

```text
I | collection_id | field_id | encoded_value | entity_id
```

Der `encoded_value` ist **ordnungserhaltend** (`a < b` ⇔ `encode(a) < encode(b)`) und selbst-delimitierend:

| Typ | Encoding |
|---|---|
| `Int64` | `(v as u64) ^ i64::MIN`, Big-Endian |
| `Float64` | monotone Bits; totale Ordnung `-∞ < … < -0 == +0 < … < +∞ < NaN` |
| `Bool` | 1 Byte |
| `String`/`Bytes` | null-freies Escaping (`0x00`→`0x00 0x01`) + Terminator `0x00 0x00` |

**Architektur-Invarianten (in `index.rs` dokumentiert):**

1. **Die Entity ist immer Source of Truth.** Ein Index liefert nur Kandidaten, nie den Entity-Zustand.
2. **Ein Index darf False Positives enthalten, aber NIEMALS False Negatives.** `find()` verifiziert deshalb jede Kandidaten-Entity gegen ihren echten Wert.
3. **Write-Reihenfolge:** `PUT neuer Index-Eintrag → PUT Entity → DELETE alter Index-Eintrag`. Dadurch ist der Index während einer Änderung temporär ein **Superset** (nie ein Subset) der korrekten Einträge — die False-Negative-Invariante gilt **ohne** Transactions.

**Index-Verwaltung:** `create_index` persistiert die Definition als `BUILDING` (fsync), baut den Index aus den vorhandenen Entities auf, dann `READY` (fsync). Ein Crash während des Builds hinterlässt `BUILDING` → beim nächsten `open` wird der Index **vollständig neu** gebaut (idempotent).

**Proof:** Der Oracle-Test (`tests/index.rs`) vergleicht nach **8000 zufälligen Mutationen** (Updates, Deletes, Re-Inserts) plus Flush, Compaction und Restart jede `find()`-Abfrage gegen einen **Full-Scan** der Entity-Daten — beide sind identisch.

| Komponente | Test-Ergebnis |
|---|---|
| `ordering` (order-preserving Codec: Int/Float/String/Bytes/Bool/Null) | ok |
| `ordering` Property-Tests (Byte-Ordnung == Wert-Ordnung, Sortier-Stabilität) | ok |
| Schema (Index-Definitionen, BUILDING/READY, save/load) | ok |
| Index-Key-Codec (roundtrip, Sonderzeichen, Bereichs-Grenzen) | ok |
| `find` (Eq/Gt/Gte/Lt/Lte/Between) + Maintenance (Update/Delete) | ok |
| Oracle-Test (`tests/index.rs`, 8000 Mutationen vs Full-Scan) | ok |

---

## v0.4 — Atomare Transaktionen

Eine **Einzel-Transaktion** über Entity + Indexe: Alle Mutationen eines `commit()` werden atomar sichtbar — über die Entity **und** die Secondary Indexe. Der WAL ist die Autorität für den Commit-Point.

```rust
store.transaction_with(|tx| {
    tx.update("users", "u1", &user)?;   // nur gepuffert
    tx.update("users", "u2", &other)?;
    Ok(())                               // → commit()
})?;

// oder manuell:
let mut tx = store.transaction()?;
tx.update("users", "u1", &user)?;
tx.get("users", "u1")?;                  // sieht den eigenen (uncommitteten) Write
tx.commit()?;                            // atomar, durable
```

**Kernprinzipien:**

- **Read-your-own-writes:** `get`/`scan_collection`/`find` überlagern die committete DB mit einem **Pending-Overlay** (`TxMutator`). Lookup-Reihenfolge: *Pending zuerst, dann committed*; bei `scan` gewinnt Pending (inkl. Tombstones).
- **Nichts persistiert bis `commit()`:** `transaction()` erzeugt nur ein Objekt; `update`/`delete` schreiben in den Pending-Puffer, nicht in den WAL. Ein `abort()`/`drop` ohne Commit hinterlässt **keine** Spur.
- **Commit-Ablauf (WAL ist der Commit-Point):**
  ```
  calculate pending → BEGIN → TxPut/TxDelete… → COMMIT → fsync → MemTable
  ```
  Nach dem `fsync` ist der Block dauerhaft; das anschließende MemTable-Apply ist infallibel. Ein Crash vor dem `fsync` verwirft den ganzen Block, einer danach überlebt ihn.
- **`Mutator`-Abstraktion:** `core_put_entity`/`core_delete_entity`/`find` laufen über `DirectMutator` (committed) oder `TxMutator` (Transaktion) — dieselbe Entity-/Index-Semantik in beiden Pfaden, kein Auseinanderlaufen.
- **TX-ID-Eindeutigkeit:** `next_tx_id` startet nach `max_seen_tx_id + 1` (aus **allen** WAL-Records, auch uncommitteten), sodass nach einem Crash keine ID wiederverwendet wird.

| Komponente | Test-Ergebnis |
|---|---|
| WAL-Typen + tx-aware Replay (committed overlebt, aborted/uncommitted verworfen) | ok |
| Atomarer Commit (Entity + Index konsistent) | ok |
| Abort (keine Spur) / `transaction_with` Fehler-Abbruch | ok |
| Read-your-own-writes (get/scan/find), Write-Sequenz-Matrix | ok |
| Crash-before-commit (verworfen) / Crash-after-commit (überlebt) | ok |
| Random-Modell-Oracle (`tests/transaction.rs`, commit/abort/crash/restart) | ok |

---

## Roadmap

| Version | Inhalt |
|---|---|
| **v0.1** -fertig- | LSM-Engine: `put`/`get`/`delete`/`scan`, WAL, MemTable, SSTable, Bloom, Compaction, Recovery |
| **v0.1.1** -fertig- | Härtung: Clean-Shutdown, Benchmark, Crash-Test, `get`-Optimierung (Punkt-Lookup + Caches) |
| **v0.2** -fertig- | Entity-Layer: Typed Codec, binäres Key-Encoding, persistente Schema-Registry, Entity-Reconstruction |
| **v0.3** -fertig- | Secondary Indexes: order-preserving Codec, geordneter Value-Index, Index-Maintenance, Index-Rebuild, Oracle-Test |
| **v0.4** -fertig- | Transactions: atomarer Commit über WAL (BEGIN/COMMIT), Read-your-own-writes, Index-Konsistenz, Random-Modell-Oracle |
| **v0.5** | Query-Planner / Query-Optimizer |

Das Gesamtkonzept (LSM-Engine + Entity-Modell + Indexes + Query-Optimizer) ist in [`konzept-kombination.md`](../konzept-kombination.md) beschrieben.
