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

Für `scan` werden alle Quellen über einen Merge-Iterator zusammengeführt. Bei gleichen Keys gewinnt die **kleinere Quell-Index-Nummer** (frischer).

Seit **v0.5.1** ist der Lesepfad **lazy**: `scan_stream` materialisiert die Datenmenge nicht, sondern hält nur einen Cursor pro Quelle (MemTable-Kopie + je einen `TableIter` pro SSTable, positioniert per Sparse-Index). Ein `ScanIter` besitzt **exklusiv** `&mut Database` für seine Lebensdauer, wodurch `put`/`delete`/`flush`/`compact` während der Iteration unmöglich sind und die konsistente Snapshot-Sicht durch Borrowing garantiert ist. `scan` ist ein Komfort-Wrapper, der den Stream einsammelt. Dieselben Quellen werden auch im Entity-/Query-Layer gestreamt: `Limit` hört nach `n` Zeilen auf zu ziehen, `Sort` blockiert (muss alles konsumieren).

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
| `db.scan(start, end)` | Sortierter Bereichs-Scan, liefert `Vec<(Vec<u8>, Option<Vec<u8>>)>` (Komfort-Wrapper). |
| `db.scan_stream(start, end)` | v0.5.1: **lazy** Bereichs-Scan, liefert einen `Iterator<Item=Result<...>>` (materialisiert nichts). |
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
| `store.query(collection)?` | v0.5: erzeugt einen `QueryBuilder` (filter/sort/limit). |
| `builder.filter(pred)` / `.sort(field, dir)` / `.limit(n)` | v0.5: kettet Bedingungen an (jeweils `Self`). |
| `store.execute_query(builder)?` | v0.5: führt den Plan aus, liefert `Vec<(String, Entity)>`. |
| `store.explain_query(&builder)?` | v0.5: zeigt den geplanten Physical-Plan als Baum (String). |

> **Hinweis zu `close()`:** Das ist der primäre Durability-Mechanismus für einen sauberen Shutdown. `drop(db)` ist nur ein **Best-Effort-Fallback** (flusht beim Verwerfen, ignoriert Fehler) — es ist keine Durability-Garantie. Rufe `close()` bewusst auf.

---

## Konfiguration

```rust
use my_lsm_db::{Database, Options};

let opts = Options {
    memtable_limit: 4 * 1024 * 1024,  // Byte, ab wann geflusht wird
    l0_compact_threshold: 4,          // ab wie vielen L0-Tabellen kompaktiert wird
    segment_max_records: 30_000,      // deterministische L1-Segment-Split-Regel
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
│   ├── query/        → v0.5: Query-Planner/Executor (ast, expression, logical, physical, planner, executor, explain)
│   └── entity.rs     → v0.2-v0.5: Entity + EntityStore + Transaction + Query (put/get/delete, Reconstruction, Index-Maintenance)
└── tests/
    ├── engine.rs     → Integrationstests (Flush, Recovery, Compaction, ...)
    ├── entity.rs     → v0.2: Smoke-Tests (put/get, Persistenz über Reopen)
    ├── index.rs      → v0.3: Oracle-Test (find vs Full-Scan)
    ├── transaction.rs → v0.4: Random-Modell-Oracle (commit/abort/crash/restart/index/entity)
    └── query.rs      → v0.5: Oracle-Test (query vs Full-Scan)
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
- **1 Query-Oracle-Test** (`tests/query.rs`): 200 zufällige Prädikate (AND/OR/NOT + 6 Vergleiche + fehlende Felder) werden gegen einen **Full-Scan** geprüft — Ausführung und Oracle teilen sich dieselbe `eval`-Semantik. Zusätzlich `basic_queries_match_oracle`, `missing_field_ne_vs_not_eq_consistent` und `explain_prints_tree`.

> **Gesamt: 64 Tests grün.** Die Ausführungs-Orchestrierung (`tests/index.rs`, `tests/transaction.rs`, `tests/query.rs`) dauert im Debug-Modus mehrere Minuten (Volle Full-Scan-Oracle).

---

## Bekannte Einschränkungen

- **Einzelprozess, ein Thread** — kein MVCC, keine Sperren, keine Nebenläufigkeit. Seit v0.4 existiert genau **eine** Transaktion pro Store (`&mut`-Borrow); keine gleichzeitigen Schreiber.
- **Kein MVCC / keine Snapshot-Isolation** — eine Transaktion sieht committete Daten plus ihre eigenen Writes, aber keine anderen Isolationsstufen.
- `scan` lädt alle Datensätze in den Speicher (kein Streaming-Iterator über die Platte).
- Compaction führt nur **einen** Merge-Schritt pro Flush aus (kein mehrstufiges Level-System, keine selektive Compaction).
- Bloom-Filter sind fest auf `1024` Bit gesetzt, nicht an die Datenmenge angepasst.
- Der `TableCache` hält Reader im RAM; bei sehr vielen Tabellen wächst er entsprechend.
- **`limit` ohne `sort` ist undefiniert** (v0.5): Nur mit `sort` ist die Reihenfolge der Ergebniszeilen und damit `limit` deterministisch.

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

## v0.5 — Query-Planner / Query-Optimizer

Eine kleine **deklarative Query-Sprache** über dem Entity-Layer. Die Query wird nicht zeilenweise "einfach so" ausgeführt, sondern vom **Planner** in einen physischen Plan übersetzt — er entscheidet, ob über einen **Secondary Index** gesucht oder **voll gescannt** wird, und was als Rest-Filter übrig bleibt.

```rust
use my_lsm_db::codec::Value;
use my_lsm_db::entity::EntityStore;
use my_lsm_db::query::{SortDir, eq, ge};

let mut store = EntityStore::open("data")?;

let b = store
    .query("users")?
    .filter(ge("age", Value::Int(30)))
    .filter(eq("country", Value::String("DE".into())))
    .sort("age", SortDir::Asc)
    .limit(50);

let rows: Vec<(String, Entity)> = store.execute_query(b)?; // (entity_id, entity)

// oder: den physischen Plan als Baum ansehen:
println!("{}", store.explain_query(&b)?);
```

`explain_query` zeigt, **wie** der Planner sucht (echte Ausgabe):

```text
Limit { n: 50 }
  Sort { field: age, dir: Asc }
    Filter { predicate: country = "DE" }
      Fetch { collection: users }
        IndexScan { collection: users, field: age, range: [30,]..∅ }
```

Hier wählt der Planner den Index auf `age` für `age >= 30`; `country = "DE"` wird als **Residual-Filter** auf die Index-Kandidaten angewendet. Die Query-Sprache ist nur die Oberfläche — der eigentliche Wert steckt darunter im Planner.

**Wie der Planner arbeitet (Kurzfassung):**

1. **DNF:** Das Prädikat wird in eine Disjunktive Normalform gebracht (`AND`-Klauseln, die mit `OR` verbunden sind). Negation bleibt als Literal erhalten — sie wird **nie** in inverse Bereichs-Operatoren umgeformt (semantischer Unterschied bei fehlenden Feldern).
2. **Regelbasierte Indexwahl:** Für jede `AND`-Klausel wird **ein** indexierbares Feld gewählt (`Eq` schlägt Bereichs-Vergleiche, sonst lexikografisch kleinstes Feld, deterministisch). Die Index-Bedingungen werden zu einem `(lower, upper)`-Bound-Paar verdichtet (auch gemischte Exklusivität wie `>30 AND <40`).
3. **Residual-Filter:** Alle nicht index-abgedeckten Bedingungen (und die eines `OR` immer das **volle** Prädikat) bleiben als `Filter` stehen. Ein Index liefert nur **Kandidaten**; der Filter re-prüft exakt.
4. **OR → Union:** `OR`-Klauseln werden per `UnionIds` (dedupliziert) zusammengeführt.

**Bewusste Einschränkungen (v0.5-Scope):**

- **Read-only:** `query`/`execute_query`/`explain_query` planen und lesen nur — sie mutieren nie. Transaktions-Queries (`tx.query`), `join`, Aggregationen, Projektion und SQL sind **nicht** Teil von v0.5.
- **Ein Index pro `AND`-Klausel:** keine index-order-Sortierung, kein Cost-Based Optimizer, keine Kardinalitäts-Schätzung — der Planner ist bewusst einfach und regelbasiert.
- **`Ne` ist kein Index-Zugriff** (es bleibt ein Residual-Filter); `Ne` wird als `Not(Eq)` ausgewertet — konsistent auch für fehlende Felder.
- **`limit` ohne `sort` = undefinierte Reihenfolge** (dokumentiert; nur mit `sort` ist `limit` deterministisch).
- **Clippy-Warnings** sind hier bewusst nicht Teil des v0.6-Scope; sie stammen aus älteren Bereichen und werden separat angegangen.

| Komponente | Test-Ergebnis |
|---|---|
| AST (Predicate: And/Or/Not/Field, Cmp, Builder `eq/ne/lt/le/gt/ge`) | ok |
| Eval (Missing-Field-Semantik, `Ne ≡ Not(Eq)`, `Not` kompositional) | ok |
| Planner (DNF, Bound-Merging, regelbasierte Indexwahl, OR→Union) | ok |
| Executor (IndexScan/FullScan/Fetch/Filter/Sort/Limit, deterministisch) | ok |
| Explain (Baum-Darstellung) | ok |
| Oracle-Test (`tests/query.rs`, 200 Random-Queries vs Full-Scan) | ok |

---

## v0.5.1 — Lazy-Read-Pfad

Technische Härtung des **Lesepfads**: Vorher materialisierte jeder Read (Entity-Lookup, Collection-Scan, Query ohne Sort, sogar das veraltete Feld-Lesen beim Update) die **gesamte Datenmenge** der Engine (`read_snapshot` → `merge_level` → `TableReader::iter`), also O(ganze DB). Seit v0.5.1 ist das Lesen **lazy** und O(gesuchter Bereich):

- **`Database::scan_stream(start, end)`** liefert einen `ScanIter` (statt `Vec`). Es hält je Quelle **einen** Cursor: eine beschränkte MemTable-Kopie + pro SSTable einen `TableIter`, der sich per Sparse-Index exakt auf den Block mit `start` positioniert und nur die Records des Ranges liest.
- **Snapshot-Konsistenz ohne Lock:** `ScanIter` besitzt **exklusiv `&mut Database`** für seine Lebensdauer (Borrowing). Damit sind `put`/`delete`/`flush`/`compact` während der Iteration unmöglich und der beim Erzeugen festgelegte SSTable-Satz bleibt stabil. Bewusst **kein** Public-`Snapshot`-Struct (Variante B/MVCC ist später als Drop-in denkbar).
- **`MergeIter`** (binärer Heap, O(#Quellen)-Speicher, nie O(#Records)): neueste Quelle gewinnt, ältere Quellen desselben Keys werden vollständig überschattet, Tombstones sind reguläre Werte und verdecken ältere Werte. Derselbe Mechanismus dient auch der Compaction (`merge_vecs`).
- **`Mutator::scan` (Breaking Change)** gibt nun einen lazy `ScanStream` zurück statt `Vec`; `TxMutator` baut daraus einen lazy 2-Wege-Merge (committed ∘ pending, pending gewinnt). Die Entity-Kernfunktionen sammeln ihre schmalen Entity-Ranges weiterhin ein — Logik unverändert.
- **Executor-Pull-Model:** `FullScan` streamt Entities aus `scan_stream`, `Fetch` point-fetcht je Kandidaten-ID, `Limit` = `take(n)` (hört auf zu ziehen), nur `Sort` blockiert. `UnionIds` merge eager (ein `&mut db`) mit Dedup per ID.

| Komponente | Test-Ergebnis |
|---|---|
| `TableIter` (Seek exakt `>= start`, Blockgrenzen, `end` exklusiv) | ok |
| `MergeIter` (newest-wins, Tombstone-Shadowing, leere Quellen) | ok |
| Lazy-vs-Eager Equivalence (MemTable + mehrere SSTables, Überschattungen, Range-Grenzen, leere/nicht vorhandene Ranges) | ok |
| `Mutator`-Migration (Direct + Tx, DirectScan) | ok |
| Bestehende Index-/Transaction-/Query-Oracles | ok (unverändert grün) |

---

## v0.5.2 — API-/Semantik-Härtung

Konsolidierung der **öffentlichen Verträge** — keine neuen Features, keine Vereinfachung der v0.5.1-Lazy-Read-Oracles:

- **Entity-ID = strikt UTF-8:** Die ID bleibt `&str`/`String`. Nicht-UTF-8-Bytes werden **beim Schreiben** (`put_entity`/`delete_entity`) als `Error::InvalidArgument` abgelehnt, nie persistiert. Im Speicher gefundene, nicht-UTF-8-IDs (Korruption) liefern über den neuen `keycodec::decode_entity_id`-Helfer `Error::InvalidFormat` — kein `from_utf8_lossy`-Ersatz mehr auf dem ID-Decode-Pfad.
- **Fehler-Taxonomie:** Neues `Error::InvalidArgument` für Nutzungs-/Aufruf-Fehler (inaktive Transaktion, unbekanntes Feld ohne Index, read-only-View). `InvalidFormat` bleibt **ausschließlich** Persistenz-/Encoding-Korruption (Codec, SSTable, Manifest, Schema-Parse, Keys).
- **Read-only-Schema-Invariante:** Lese-Operationen (`scan_collection`, `Transaction::get`/`scan_collection`/`find`) mutieren das Schema **nie** mehr — sie nutzen `lookup_collection_id` statt des mutierenden `collection_id()` und persistieren das Schema nicht. Unbekannte Collection ⇒ konsistent leer (`Ok(vec![])` / `Ok(None)`), kein `SCHEMA`-File wird erzeugt.

| Regressionstest (`tests/hardening.rs`) | Ergebnis |
|---|---|
| Nicht-UTF-8-ID bei `put_entity`/`delete_entity` ⇒ `InvalidArgument` | ok |
| Im Speicher korrupte (nicht-UTF-8) ID ⇒ `InvalidFormat` | ok |
| Lese-Operationen auf unbekannter Collection leer + kein `SCHEMA`-Write | ok |
| Bestehendes Schema bleibt nach Reads byte-identisch | ok |
| Persistenz-Korruption bleibt `InvalidFormat` | ok |

---

## v0.6 — Query-Ausführung, Cost-Based Indexwahl, Index-Order / Top-K

v0.6 erweitert die Query-Schicht, ohne die Storage-Architektur umzubauen:

- **Transaktionale Queries:** Der Executor läuft über `Mutator`, sodass `get`, `scan_collection`, `find` und Query-Ausführung dieselben Pending-Writes sehen.
- **Cost-Based Indexwahl:** Der Planner wählt pro DNF-Klausel deterministisch den günstigsten READY-Index über ein kleines Kostenmodell (`Eq < Between < OneSided`), ohne Statistik-Infrastruktur.
- **Index-Order / Top-K:** Wenn das Sortierfeld in **jeder** DNF-Klausel positiv vorhanden ist und ein READY-Index existiert, wird `ORDER BY indexed_field LIMIT n` als `Limit { Filter { IndexOrderScan } }` geplant. Sonst bleibt der bestehende `Sort`-Fallback unverändert blockierend.

Wichtig ist die Invariante: Presence-Garantie schaltet den geordneten Pfad erst frei. Die Entity-Verifikation bleibt trotzdem Pflicht, damit Index-/Entity-Abweichungen nie sichtbar werden.

**`explain()`-Beispiel mit v0.6-Pfad:**

```text
Limit { n: 5 }
  Filter { predicate: age >= 0 }
    IndexOrderScan { collection: users, field: age, range: [0,]..∅, dir: Asc }
```

---

## v0.7 — Lazy-Read-Komplexitäts-Härtung (Write-/Setup-Engpass)

v0.7 ist **kein Feature-Schritt**, sondern die Behebung zweier **O(N²)-Bugs im Lazy-Read-Pfad**, die jeden Entity-Aufbau (und jeden Write mit vorgeschaltetem Scan) quadratisch machten. Beide Fixes sind als eigenständige Commits abgegrenzt, um ihre Einzeleffekte nachvollziehbar zu halten.

Der Auslöser war ein Benchmark-Befund: Der Setup einer indexierten 10k-Collection brauchte **62–68 s**, obwohl die eigentlichen Messungen (`get`, `scan`) schnell waren. Eine Isolation (`setup-diag`) zeigte, dass schon **ohne** Index 10k Writes ~40 s dauerten — das war kein Index-Problem, sondern der allgemeine Write-/Scan-Pfad.

### v0.7.1 — `memtable_source` materialisiert nur den Range (`O(R)` statt `O(N)`)

`memtable_source` erzeugte bei **jedem** Scan eine Vektor-Kopie der **gesamten** MemTable (alle Key/Value-Clones), egal wie klein der angefragte Bereich war. Da `core_put_entity` vor jedem Put einen schmalen Range-Scan ausführt, wurde jeder Put `O(MemTable)` und der Gesamtaufbau quadratisch.

**Fix:** Statt `memtable.iter().collect()` wird der angefragte Bereich via lazy `BTreeMap::range(start, end)` materialisiert. Ein Guard liefert für leere Intervalle (`start >= end`) eine leere Quelle, statt an `BTreeMap::range` zu gehen (das dort panickt).

| Regressionstest (`src/iterator.rs`) | Ergebnis |
|---|---|
| Nur der angefragte Range wird materialisiert | ok |
| Empty-Range (`start >= end`) ⇒ leere Quelle, kein Panic | ok |
| Unbounded ⇒ alle Einträge (unverändert) | ok |

### v0.7.2 — `scan_stream` nutzt den `table_cache` (`O(Tabellen)` nur einmal)

`scan_stream` öffnete bei jedem Scan für jede SSTable eine **neue Datei** und reparte Footer/Index/Bloom erneut. Sobald mehrere SSTables existieren (durch MemTable-Flushes), wurde jeder Scan `O(Tabellen)` und das Setup über viele Puts wieder quadratisch.

**Fix:** Der vorhandene `table_cache` (den der `get`-Pfad schon nutzte) wird wiederverwendet. `TableReader` teilt Datei-Handle, sparse Index und Bloom per `Arc`; ein neues `fork()` erzeugt einen billigen Klon mit eigenem Lesepuffer (kein erneuter Datei-/Index-Read). `TableIter::from_reader` baut daraus ohne Neu-Öffnen einen Iterator.

| Regressionstest (`src/lib.rs`) | Ergebnis |
|---|---|
| Nach dem 1. Scan ist der Cache mit **allen** Tabellen befüllt | ok |
| Folge-Scans lassen die Cache-Größe nicht mehr wachsen | ok |
| Scan-Ergebnis bleibt über wiederholte Scans stabil | ok |

### Eingefrorene Post-Fix-Baseline (100k Entities)

| Workload | Wert | Bemerkung |
|---|---|---|
| `get` | **1.28 M reads/s** | skaliert gesund (kein Kollaps) |
| `scan` (full) | **29.5 MB/s** | konstant über 10k/50k/100k |
| `index-eq` | **21.4 k lookups/s** | kaum Degradation |
| `index-range` | **54.3 k lookups/s** | 49010 Hits, validiert |
| `top-k` | **10.4 k rows/s** | 10 Rows, korrekt |
| setup (mit Index) | **5.86 s** | zuvor 62–68 s bei nur 10k |
| recovery | 0.27 s | unkritisch |
| flush | 0.07 s | unkritisch |

Hinweis: Messungen mit <20 ms Messzeit (z. B. 13 ms bei kleinen Ranges) gelten als Rauschen und werden **nicht** für Performance-Vergleiche herangezogen.

Der Kern des Storage-Read-Pfads ist nach den Fixes bei 100k gesund: kein Grund, blind Reader-Cache, Bloom oder Compaction umzubauen. Die verbleibende Auffälligkeit (Setup-Kurve 10k 0.11 s → 50k 1.48 s → 100k 5.86 s) ist Gegenstand weiterer Instrumentierung, bevor ein dritter Hebel gewählt wird.

---

## Roadmap

| Version | Inhalt |
|---|---|
| **v0.1** -fertig- | LSM-Engine: `put`/`get`/`delete`/`scan`, WAL, MemTable, SSTable, Bloom, Compaction, Recovery |
| **v0.1.1** -fertig- | Härtung: Clean-Shutdown, Benchmark, Crash-Test, `get`-Optimierung (Punkt-Lookup + Caches) |
| **v0.2** -fertig- | Entity-Layer: Typed Codec, binäres Key-Encoding, persistente Schema-Registry, Entity-Reconstruction |
| **v0.3** -fertig- | Secondary Indexes: order-preserving Codec, geordneter Value-Index, Index-Maintenance, Index-Rebuild, Oracle-Test |
| **v0.4** -fertig- | Transactions: atomarer Commit über WAL (BEGIN/COMMIT), Read-your-own-writes, Index-Konsistenz, Random-Modell-Oracle |
| **v0.5** -fertig- | Query-Planner/Optimizer: deklarative Queries, DNF, regelbasierte Indexwahl, Residual-Filter, Explain, Full-Scan-Oracle |
| **v0.5.1** -fertig- | **Lazy-Read-Pfad:** `scan_stream`/`ScanIter` (Snapshot via exklusivem Borrow), `TableIter`-Seek, Lazy-Merge, Pull-Model-Executor (`Limit` = `take`), kein O(DB)-Materialisieren auf dem Lesepfad |
| **v0.5.2** -fertig- | **API-/Semantik-Härtung:** strikt-UTF-8-Entity-IDs (Write ⇒ `InvalidArgument`, Korruption ⇒ `InvalidFormat`), `Error::InvalidArgument`-Taxonomie, Read-only-Schema-Invariante (Reads mutieren kein Schema) |
| **v0.6** -fertig- | Query-Ausführung, Tx-Queries, Cost-Based Indexwahl, Index-Order / Top-K |
| **v0.7.1** -fertig- | **Lazy-Read-Komplexität:** `memtable_source` materialisiert nur den angefragten Range (`BTreeMap::range`), Empty-Range-Guard — behebt O(N²) beim Entity-Aufbau |
| **v0.7.2** -fertig- | **Lazy-Read-Komplexität:** `scan_stream` nutzt den `table_cache`, `TableReader::fork()`/`Arc`-Reader-Sharing — behebt O(N²) bei vielen SSTables |

Das Gesamtkonzept (LSM-Engine + Entity-Modell + Indexes + Query-Optimizer) ist in [`konzept-kombination.md`](../konzept-kombination.md) beschrieben.

---

## Aktueller Status — Stand Handover-Audit (2026-08-20)

> Kanonische Referenzen (siehe `design-v0.7-compaction-v2.md`):

| Referenz | Bedeutung |
|---|---|
| `d6d5916` | **Produktionsbaseline** — unverändert, kein produktiver Code von diesem Stand abgewichen |
| `6b65191` | **Diagnose-Freeze v0.9/v0.10** — v0.10 Field-Projection untersucht und verworfen (Real-Workload: 3–4 Felder/Entity, 100 % Full-Reads) |
| `cf382ca` | **Handover-Audit** — Vorgänger-LLM-Arbeit übernommen und klassifiziert (§27–§28) |

**Abgeschlossene Diagnosekette (eigene Linie):**
- **v0.9 → v0.9a → v0.9b:** Read-Hotspot lokalisiert (`get` = 82 % der Read-Kosten, skaliert mit Feldanzahl).
- **v0.10:** Field-Projection-Prototyp gebaut (G1–G8 Korrektheitsgates grün), aber **wirtschaftlich verworfen** — isolierter Gewinn nur bei `requested_fields << entity_fields`; im realen Traffic (volle Reads) langsamer als `get`.

**Handover-Audit der Vorgänger-Arbeit (§27–§28):**
- `value_cache` (v0.7-write-cache): technisch korrekt (Oracle-Tests grün, Invalidation vollständig), aber **isoliert 0–11 % Cache-Effekt** im non-tx Write-Pfad → wirtschaftlich nicht relevant, kein Produktionskandidat.
- `compaction_v2` (12/12 Regressionstests): testet exakt die `d6d5916`-Compaction-Architektur, keine neue Logik.
- `diag.rs` + `bench.rs`: funktionierende Diagnose-Infrastruktur, keine Produktionskopplung.
- `bench-ir*`, `bench-v2`, `bench-v8`: historische Klasse-C-Artefakte (aktuell wegen fehlender Feature-Definitionen nicht baubar).

**Invarianten:**
- `d6d5916` bleibt Produktionsbaseline.
- Kein Produktions-Merge aus der Diagnose/aus dem Audit.
- Nächster Sprint erst bei **neuem konkreten Befund**, nicht vorab als "v0.11" deklariert.
- Vorgänger-Working-Tree (modifizierte `src/`-Dateien, temporäre Artefakte) bleibt bewusst unangetastet.

---

## Produktions-Handover-Checkpoint — `0e62b6e` (2026-08-20)

> Abschluss der Correctness-/Recovery-/GC-/Durability-Diagnose. **Kein offener
> technischer Arbeitsauftrag.** Nächster Zweig ausschließlich bei neuem realem
> Befund. `0e62b6e` ist die **aktuelle Code-Baseline** (superseded `d6d5916`
> für die aktive Linie) und == `origin/main`.

**Kanonische Details:** `design-v0.7-compaction-v2.md` §30.9–§30.12.

**Abgeschlossene Zweige (Design-Doc + Commit-History nachvollziehbar):**
- **E.8** — Compaction-Correctness: `merge_ids`/`table_bounds` propagieren
  Lese-/Iter-Fehler; `compact()` bricht vor `manifest.save` ab statt stillen
  Datenverlusts. (`e9a5f80`, `tests/merge_ids_fault.rs`)
- **E.10** — Orphan-GC: `db.gc()` (explizit, `&mut self`, **kein** Auto-GC beim
  Open) räumt unreferenzierte `*.sst` + `.manifest.tmp` auf.
  (`33b6a98` + `5ebc9ce`, `tests/orphan_gc.rs`)
- **F.3–F.5** — Manifest-`L`-Zeilen-Parsing strikt: korrupter ID-Token in
  `L`-Zeile → `InvalidFormat` statt stillem Datenverlust.
  (`0725057`, `tests/manifest_corruption.rs`)
- **F-WAL** — WAL-Durability-Audit: kein Correctness-Bug, kein
  Vertragsbruch; deferred durability explizit dokumentiert. (`0e62b6e`, §30.12)

**Bewusste Designentscheidungen (kein Fix nötig):**
- Direct `put`/`delete`: **deferred durability** — nicht durable beim Return,
  durable nach `flush()`/`close()`. Transaktionen durable nach `commit()`
  (per-commit `fsync`). **Kein** `fsync` pro Direct-Write ( Performance-/Architektur-Entscheidung, kein Bug).
- `MANIFEST` atomar (tmp + `sync_all` + rename); `L0`/Segmente werden vor
  deren Löschung committet (Crash-Fenster erzeugen nur harmlose Orphans).
- `merge_ids`/`table_bounds` brechen bei unlesbarer manifestierter SSTable ab
  (kein stiller Datenverlust mehr).
- `gc()` nur explizit, id-basiert; rührt Manifest/Compaction nicht an.

**Offene Befunde:** **keine** A/B-Correctness-/Durability-Befunde. Performance-
Themen (WAL-fsync, Reader-Cache, Compaction-Strategie) bewusst nicht
angetastet — kein nachgewiesener Nutzen.

**Regel:** Kein neuer technischer Zweig ohne konkreten neuen Befund. Bei realem
Workload (get / Entity-Put / Compaction / Speicherverbrauch) startet daraus der
nächste Zyklus: **Befund → Forensik → Regression → minimaler Fix → Volltest → Push.**

