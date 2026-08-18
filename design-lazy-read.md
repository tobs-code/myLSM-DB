# Design: Lazy-Read-Pfad (v0.5.1)

> Status: **festgelegt (Architektur-Entscheidung)**. Es wird **noch kein Code geändert**,
> bis dieses Dokument committed ist. Danach ist dieser Entwurf der verbindliche Schnitt
> für den Umbau.

## Motivation

Die aktuelle Read-Architektur ist der eigentliche Flaschenhals. Alle Reads laufen über
`Database::scan` (lib.rs:222), das eine **vollständig materialisierende** `read_snapshot()`
(lib.rs:341) aufruft: `merge_level()` (lib.rs:324) → `reader.iter()` (sstable.rs:265) liest
**alle Records aller SSTables komplett in den RAM**, erst danach wird auf den Range
gefiltert.

Konsequenz: **Jede** Entity-Leseoperation ist O(gesamte Datenmenge) im RAM und in der Zeit:

- `core_get_entity` (entity.rs:295) — ein einzelner Entity-Fetch ist ein Full-Snapshot.
- `core_scan_collection` (entity.rs:328) — komplette Materialisierung.
- `core_put_entity` (entity.rs:171) — der Stale-Field-Read vor jedem Write ist ein Full-Snapshot,
  also sind **auch Writes** O(gesamte Datenmenge).
- Jeder `find`/`IndexScan`.

Der punktbasierte `Database::get` (bloom + sparse Index, lib.rs:190) ist bereits effizient, wird
aber nur für die Index-Verifikation genutzt (`field_value_m`, index.rs:91), nicht für Entity-Reads.

> **Zentrale Erkenntnis:** Die Entity-Funktionen nutzen bereits schmale Bereiche
> (`entity_range(collection_id, entity_id)`, entity.rs:132). Die Ineffizienz sitzt **fast
> ausschließlich** darunter in `Database::scan`. Der Umbau konzentriert sich deshalb auf die
> Storage-Schicht; Teile 4–5 sind vor allem Verifikation, kein Neubau.

## Zielarchitektur

Heute:

```text
SSTables
   ↓
read_snapshot()
   ↓
ALLE Records in RAM
   ↓
BTreeMap
   ↓
Scan / Entity / Query
```

Soll:

```text
SSTables
   ↓
Range-aware iterators  (seek / next / [start, end))
   ↓
k-way Merge            (Binary Heap, newest wins)
   ↓
Lazy stream
   ↓
Entity reconstruction / Query
```

Damit wird z.B. `get_entity` etwa `O(Anzahl relevanter SSTables × Block-Reads)` statt `O(DB)`,
und `scan(collection)` streamt statt die Collection zu materialisieren.

## Architektur-Entscheidungen (festgelegt)

### 1. Snapshot: Variante A

`ScanIter` borgt `&mut Database`; die Snapshot-Semantik ist durch Rusts Borrowing garantiert.
Eine eigene, öffentliche `Snapshot`-Abstraktion wird **zunächst nicht** eingeführt. Variante B
(owned `Snapshot`, kopiert MemTable + Level-Listen) bleibt eine spätere Erweiterung für
Concurrency/MVCC. Die Struktur wird so gehalten, dass B später als reiner Austausch des
MemTable-Zugriffs einstecken kann.

### 2. `Mutator::scan`: Breaking Change jetzt

Kein paralleles `scan_stream`-API neben `scan`. Der Lazy-Pfad ist die Abstraktion, die wir
künftig überall brauchen; einmal sauber migrieren statt zwei Scan-APIs dauerhaft mitschleppen.
`Mutator::scan` liefert einen lazy Iterator statt eines `Vec`.

### 3. Compaction: zunächst nicht umbauen

Lazy Merge zuerst nur für den Read-Pfad. Compaction bekommt später eine eigene Messung; wenn
sich dort der RAM-Verbrauch als Problem zeigt, kann derselbe Merge-Mechanismus wiederverwendet
werden.

## Harte Invarianten

> **Ein `ScanIter` besitzt exklusiven Zugriff auf die Database für seine gesamte Lebensdauer.
> Währenddessen sind `put`, `delete`, `flush` und `compact` nicht möglich. SSTables sind
> immutable; der zum Zeitpunkt der Iterator-Erzeugung festgelegte SSTable-Satz bleibt daher
> konsistent.**

> **Bei identischem Key gewinnt immer die Quelle mit der höheren Aktualität; ältere Quellen
> werden vollständig übersprungen. Eine Tombstone ist dabei ein regulärer Wert und verhindert
> das Durchscheinen älterer Werte.**

## Schichtung

```text
Database
  └── scan_stream(start, end)
        └── ScanIter<'_>                      (besitzt exklusiven &mut Database)
              ├── MemTable cursor              (in-place, unter dem Borrow)
              └── MergeIter                    (Binary Heap)
                    ├── SSTable cursor         (TableIter, lazy geöffnet)
                    ├── SSTable cursor
                    └── ...
```

---

## Teil 1 — SSTable-Iterator API

Alle Primitive existieren in `sstable.rs`; es fehlt nur die Lazy-Kopplung:

- `read_record()` (sstable.rs:203) liest bereits **einen** Record sequenziell und respektiert
  `data_end` (schützt vor Hineinlesen in Index/Bloom/Footer).
- `block_offset_for()` (sstable.rs:184) liefert per binärer Suche den Block-Start für einen Key.

```rust
pub struct TableIter { /* hält &mut TableReader, plus [start, end) */ }

impl TableIter {
    fn seek(&mut self, key: &[u8]) -> Result<()>;
    fn next(&mut self) -> Result<Option<(Vec<u8>, Entry)>>; // stoppt bei >= end
}
```

- `seek` = `block_offset_for(index_entries, key)` → `file.seek(block)` → `next()` bis
  `record >= key`.
- `next` = `read_record()` + Range-Guard. **Null Materialisierung**, Speicher = ein Record.
- `iter()` (sstable.rs:265) bleibt als Komfort-Wrapper (collect von `TableIter`).

## Teil 2 — Lazy k-way Merge

Der heutige `MergedIter` (iterator.rs:26) findet das Minimum per **linearer Suche über alle
Quellen** — O(Anzahl Quellen) pro Schritt, und arbeitet auf materialisierten Vektoren.

Lazy: Jede Quelle ist ein `TableIter` (bzw. ein MemTable-Iterator über `range()`,
memtable.rs:48). Ein **Binary Heap** über `(key, source_idx)` aufsteigend, `source_idx` als
Tie-Break = **newest wins** (0 = MemTable, dann Level 0, 1, …). Bei Duplicate-Key wird der
neueste Wert emittiert und alle älteren Quellen mit demselben Key übersprungen (Heap-Pop +
advance). Tombstones (`Entry = None`) sind nur ein weiterer Wert; die Tombstone-Schattierung
passiert von selbst (identisch zu iterator.rs:102).

```rust
pub struct MergeIter { heap: BinaryHeap<SourceCursor> } // (key, source_idx, pos)
impl MergeIter { fn next(&mut self) -> Result<Option<(Vec<u8>, Entry)>>; }
```

Speicher = O(Anzahl Quellen), nicht O(Datenmenge).

## Teil 3 — Database-Scan-API

- `get()` (lib.rs:190) bleibt Punkt-Lookup (unverändert).
- Neu: `scan_stream(start, end) -> Result<ScanIter>` — lazy, **eine** Snapshot-Sicht pro Aufruf,
  geborgt per `&mut self`.
- `scan()` bleibt als Convenience (collect von `scan_stream`).

```rust
pub fn scan_stream<'a>(&'a mut self, start: Option<&[u8]>, end: Option<&[u8]>)
    -> Result<ScanIter<'a>>;
```

Der Reader-Cache liegt im `ScanIter` (lazy geöffnet, für die Iterator-Lebensdauer gecacht),
nicht im globalen `table_cache` — so sind Wiederholungs-Seeks innerhalb eines Scans billig und
konsistent. SSTables sind immutable und Compaction kann während des Borrows nicht laufen →
"Reader später öffnen" ist konsistent.

## Teil 4 — Entity-Layer

Die Entity-Funktionen nutzen bereits schmale Bereiche (`entity_range`, entity.rs:132) und
bleiben **logisch unverändert**; sie werden nur auf den lazy Scan umgestellt:

- `core_get_entity` (entity.rs:295): O(relevante SSTables × Block-Reads) statt O(DB).
- `core_put_entity` / `core_delete_entity` (entity.rs:171, 258): der Stale-Field-Read ist schon
  ein schmaler Entity-Scan → **Writes verlieren ihr O(DB) ohne Logikänderung.**
- `core_scan_collection` (entity.rs:328): streamt die Collection-Prefix-Grenzen lazy; die
  `BTreeMap`-Gruppierung bleibt (nötig zur Rekonstruktion von Entities aus dem flachen
  Feld-Stream), wird aber aus einem Stream gefüttert.

`Mutator::scan` liefert lazy — koordinierter Breaking Change durch `DirectMutator` (lib.rs:427),
`TxMutator` (entity.rs:125; committed lazy + Pending-Overlay als Zwei-Wege-Merge) und
`DirectScan` (entity.rs:700; adaptiert eine `Vec` zu einem trägen Iterator).

## Teil 5 — Query Executor

Der Executor wechselt von rekursivem `exec()` (das ganze `Rows`-Vektoren zurückgibt,
executor.rs:52) auf ein **Pull-Modell**:

```rust
trait Op { fn next(&mut self) -> Result<Option<(String, Entity)>>; }
```

- `FullScan`: streamt Entities aus einem Collection-Scan statt `Vec` zu bauen.
- `IndexScan → Fetch`: Der Index-Scan streamt Kandidaten-IDs; die Verifikation ist bereits
  Punkt-Lookup (`field_value_m`, index.rs:91). `Fetch` point-fetcht pro ID → kein
  Materialisieren von IDs oder Entities.
- `Limit`: hört auf zu ziehen, sobald `n` erreicht — trivial im Pull-Modell.
- `Sort` bleibt blockierend (muss alles konsumieren); das Pull-Seam ist der Ort, wo v0.6
  `ORDER BY + LIMIT` als bounded Heap oder Index-Scan einhakt.

**Eine Snapshot pro Query:** Der Executor hält `&mut` über die ganze Query; heute macht er pro
`core_get_entity` einen eigenen Scan. Im Lazy-Design erwirbt er **eine** `ScanIter`-Sicht und
fädelt sie durch alle Reads → Konsistenz *innerhalb* der Query plus amortisierte Reader-Caches.

---

## Verifikations-Gates (Pflicht)

- **Lazy-vs-Eager Equivalence:** identische Keys/Entries inkl. Tombstones und
  Duplicate-Key-Shading.
- **Range-Grenzen:** `start` inklusiv, `end` exklusiv.
- **Seek:** vorwärts über Blockgrenzen und exakt am ersten `>= start`.
- **Empty ranges / nonexistent keys.**
- **Mehrere SSTables + MemTable**, inklusive Überschattungen.
- Bestehende **Index-, Transaction- und Query-Oracles** unverändert grün.
- Kein O(DB)-Materialisieren mehr bei:

  - `get_entity`
  - Entity-Update
  - Entity-Delete
  - Collection-Scan
  - Query ohne Sort
  - Index-Fetch.

## Scope

- **In v0.5.1:** Teile 1–5 wie oben + Verifikations-Gates. Kein neuer Feature-Umfang.
- **Nicht in v0.5.1:** Concurrency/MVCC (Variante B), Compaction-Umbau, Tx-Queries,
  Cost-Based Planning, Index-Order-Sortierung / Top-K, Entity-ID/Error/API-Fixes.
  Letztere folgen *nach* diesem Umbau, weil wir dann wissen, welche Interfaces der
  Lazy-Layer tatsächlich braucht.
