# v0.7 — Storage / Performance Design

> Status: **Arbeitsentwurf**. Dieses Dokument definiert die nächste Architekturphase
> nach `v0.6.0`. Kein Implementierungscode vor Freigabe.

---

## Ziel

v0.7 soll den Storage-Leseweg für deutlich größere Datenmengen belastbar machen.
Der Fokus liegt **nicht** auf neuen Query-Features, sondern auf den Eigenschaften
des bestehenden Storage-Unterbaus unter realistischen Lasten.

Primäres Ziel:

- belastbare Performance für Bereiche von **100k bis 10M Entities**
- realistische Antwort auf die Frage, ob der aktuelle Lazy-Reader für größere
  Datenmengen ausreichend ist oder ob er gezielt nachgeschärft werden muss

---

## Ausgangslage nach v0.6.0

Die aktuelle Architektur hat bereits:

- **Lazy Reads** über `scan_stream`
- **Pull-basierte Queries**
- **Index-Order / Top-K** als Ausführungspfad
- **Transaktionssicht** über `Mutator`
- **deterministische Iterator-Semantik** mit Verifikation auf Entity-Ebene

Die Storage-Schicht ist damit funktional korrekt, aber noch relativ schlicht:

- Reader werden aktuell eher einfach geöffnet und genutzt
- es gibt keinen expliziten Reader-Lifecycle- oder Eviction-Plan
- die Messung ist noch nicht systematisch für große Datenmengen aufgestellt
- der aktuelle Benchmark deckt nur einen Teil der relevanten Workloads ab

---

## Messwerte aus dem aktuellen Stand

Mit dem vorhandenen Benchmark-Runner (`examples/bench.rs`) auf dem aktuellen
Stand:

### 100k Entities

- `put`: ~566k writes/s
- `random get`: ~41k reads/s
- `sequential get`: ~53k reads/s
- `scan`: ~12.2 MB/s
- `reopen+recover`: ~0.260s
- `db size`: ~3.184 MB

### 1M Entities

- `put`: ~573k writes/s
- `random get`: ~30.8k reads/s
- `sequential get`: ~39.1k reads/s
- `scan`: ~12.7 MB/s
- `reopen+recover`: ~0.187s
- `db size`: ~32.8 MB

### Vorläufige Lesart

- Schreibdurchsatz ist bereits brauchbar und relativ stabil.
- `get` skaliert sichtbar schlechter als `put`.
- `scan` bleibt funktional, aber ohne separate Storage-Optimierung eher konstant
  auf dem aktuellen Niveau.
- Reopen/Recovery ist unauffällig.

Das reicht noch nicht für eine fundierte 10M-Entscheidung. Dafür brauchen wir
ein sauberer Benchmark- und Messplan mit größeren Datenmengen und mehr
Storage-Metriken.

---

## Kandidaten für v0.7

### Option A: Storage / Performance

Ziel:

- Range-Scans über viele SSTables effizienter machen
- Reader-/Table-Cache und Eviction definieren
- Bloom-Filter-Nutzen messen und ggf. verbessern
- Compaction-Verhalten unter Last verstehen
- Memory-Budget und Iterator-Lebensdauer konkretisieren
- Write Amplification sichtbar machen

Risiko:

- mittel bis hoch, weil der Eingriff tief in den Lesepfad geht

### Option B: Query Planner

Ziel:

- komplexere Query-Pläne
- bessere Heuristiken
- weitere Ausführungstricks oberhalb des Storage-Unterbaus

Risiko:

- mittel

### Option C: API / Concurrency

Ziel:

- mehrere Leser/Writer
- Snapshot-Isolation oder MVCC

Risiko:

- sehr hoch, weil es das aktuelle Borrow-basierte Snapshot-Modell bricht

### Priorität

Für den nächsten Schritt ist **Option A** die richtige Reihenfolge.

---

## Zielinvarianten

1. `get`, Collection-Scan, Index-Scan und `ORDER BY ... LIMIT` bleiben
   semantisch unverändert.
2. Der Index bleibt weiterhin nur Kandidatenquelle, nicht Source of Truth.
3. `IndexOrderScan` bleibt eine Ausführungsoptimierung, keine Semantikänderung.
4. `TxScan`/`Mutator`-Semantik bleibt unangetastet.
5. Kein MVCC und keine Concurrency-Umstellung in diesem Meilenstein.

---

## Nicht-Ziele

- keine neuen Query-Operatoren
- keine Aggregationen
- keine Projektion
- kein SQL
- kein MVCC
- keine Snapshot-Isolation
- keine Multi-Writer-Concurrency
- keine Re-Architektur des Entity- oder Query-Layers

---

## Zu prüfende Storage-Eigenschaften

### 1. Range-Scans über viele SSTables

Fragen:

- Wie stark verschlechtern sich `scan` und `get` bei vielen Tabellen?
- Wie teuer sind Seek und Merge über viele Reader?
- Wie groß ist der Effekt von Tombstones und Überschattungen?

### 2. Reader-Lifecycle / Eviction

Fragen:

- Wie viele geöffnete Reader sind vertretbar?
- Brauchen wir eine explizite Eviction-Strategie?
- Wo liegt die Grenze zwischen Cache-Nutzen und RSS-Wachstum?

### 3. Bloom-Filter-Qualität

Fragen:

- Wie viele unnötige SSTable-Reads werden vermieden?
- Wie groß ist der reale Nutzen für `get` und Index-verifizierende Pfade?

### 4. Compaction-Verhalten

Fragen:

- Wie entwickelt sich Write Amplification über mehrere Flush-/Compaction-Zyklen?
- Wie teuer ist Compaction bei größeren Datenmengen?
- Wann kippt das System in zu viele Tabellen?

### 5. Memory-Budget

Fragen:

- Wie hoch ist Peak RSS bei 100k / 1M / 10M Entities?
- Wie stark wachsen Reader- und Iterator-Objekte?

### 6. Iterator-Lebensdauer

Fragen:

- Wie lange halten Scan- und Query-Iteratoren den exklusiven Borrow?
- Gibt es unnötige Materialisierung innerhalb des Read-Pfads?

---

## Benchmark-Plan

Der Benchmark-Satz soll mindestens diese Größen abdecken:

- `100k`
- `1M`
- `10M` nur wenn lokal/CI praktikabel, sonst als Zielgröße mit begrenzter Teilmessung

### Workloads

1. `get(id)`
2. `put/update(id)`
3. vollständiger Collection-Scan
4. kleiner Range-Scan
5. Index equality
6. Index range
7. `ORDER BY indexed_field LIMIT 50`
8. große Collection + `Sort`
9. Flush / Compaction
10. gemischter Read/Write-Workload

### Metriken

- p50 / p95 / p99
- Throughput
- Peak RSS
- Anzahl gelesener SSTable-Records
- Anzahl geöffneter Reader
- Compaction-Zeit
- Write Amplification

### Messregeln

- Benchmark nur auf `--release`
- feste Seeds für reproduzierbare Zufallsfolgen
- gleiche Datensätze für alle Workloads
- getrennte Messung von warmem und kaltem Zugriff, wo sinnvoll

---

## Test-/Oracle-Strategie

Die v0.7-Implementierung muss weiterhin gegen bestehende Oracles halten:

- `tests/engine.rs`
- `tests/index.rs`
- `tests/query.rs`
- `tests/transaction.rs`
- `tests/index_order.rs`
- `tests/planner_cost.rs`
- `tests/tx_query.rs`

Zusätzlich:

- Storage-orientierte Regressionstests für Cache-/Reader-Verhalten
- Benchmark-gestützte Reproduzierbarkeit der Storage-Metriken

---

## API-Auswirkungen

Der v0.7-Storage-Plan soll **keine** öffentliche Semantik brechen.

Mögliche interne Änderungen sind erlaubt, sofern sie folgende Verträge nicht
verletzen:

- `Database::get`
- `Database::scan`
- `Database::scan_stream`
- `Mutator`
- `EntityStore`
- `Transaction`
- Query-API und Oracles

---

## Recovery / Durability

Es sind keine neuen Recovery-Primitive geplant.

Die bestehende Durability-Reihenfolge bleibt:

- WAL ist Commit-Point für Writes und Transaktionen
- Manifest bleibt die Metadaten-Autorität
- SSTables bleiben immutable

Wenn der Storage-Pfad intern umgebaut wird, muss das Crash-Verhalten
weiterhin dieselben Garantien liefern wie heute.

---

## Freigabe-Kriterien für die nächste Umsetzungsphase

1. Benchmark-Plan steht und ist reproduzierbar.
2. Storage-Bottlenecks sind anhand realer Messwerte eingegrenzt.
3. Der v0.7-Scope ist auf eine konkrete Storage-Optimierung eingegrenzt.
4. Keine verdeckte Ausweitung auf Concurrency/MVCC oder neue Query-Features.

---

## Nächster Schritt

1. Benchmark-Plan finalisieren
2. Metriken für 100k / 1M / 10M fixieren
3. v0.7-Scope einfrieren
4. erst danach Implementierung starten
