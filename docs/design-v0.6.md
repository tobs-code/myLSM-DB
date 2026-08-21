# v0.6 — Design (verbindlich)

> **Status:** FREIGEGEBEN (eingefroren). Umsetzung in **Teil 1 → Teil 2 → Teil 3**.
> Voraussetzung: **v0.5.2** (API-/Semantik-Härtung) ist fertig und gilt als **API-Freeze** für v0.6.

---

## Kontext & Ausgangslage

v0.5.2 hat die öffentlichen Verträge konsolidiert (strikt-UTF-8-Entity-IDs,
`Error::InvalidArgument`-Taxonomie, Read-only-Schema-Invariante). v0.6 baut auf
diesem Stand auf und verändert **keine** v0.5.x-Verträge, sofern unten nichts
ausdrücklich als v0.6-Entscheidung dokumentiert wird.

Relevanter Ist-Zustand (gegen `main` @ `309a9fa` verifiziert):

- **Executor** (`src/query/executor.rs`): `run(db, schema, plan)` @40 ist an
  `&mut Database` gebunden. `exec_rows` @49 verkettet `Iterator`s (Pull-Modell);
  `FullScan` streamt über `ScanAssembler` @171 aus `db.scan_stream`,
  `IndexScan` erzeugt Kandidaten-IDs via `index_scan` @251 / `index::find`,
  `Fetch` punkt-lookt via `DirectMutator` (`FetchIter` @228), `Sort` blockiert
  (sammelt alle Zeilen, `sort_rows` @274), `Limit` = `take(n)` @96.
- **Planner** (`src/query/planner.rs`): regelbasiert. `pick_index_field` @126
  bevorzugt `Eq`, sonst lexikographisch kleinstes Feldname. **Kein**
  Kostenmodell, keine Cardinality. `plan` @283 → `PhysicalPlan`.
- **Physical-Plan** (`src/query/physical.rs`): `IndexScan`, `FullScan`,
  `UnionIds`, `Fetch`, `Filter`, `Sort`, `Limit` (enum @19).
- **Transaktion** (`src/entity.rs`): `Transaction` @103 hält `pending:
  BTreeMap<Vec<u8>, Option<Vec<u8>>>`; `TxMutator` @113 überlagert committed +
  pending (Read-your-own-writes, **pending gewinnt**, inkl. Tombstones, lazy
  `TxScan`-Merge @150). `TxMutator::scan` liefert bereits eine
  `ScanStream<'s>` (lib.rs:410/418).
- **Mutator-Trait** (`src/lib.rs`): `Mutator` @405 mit `get`/`scan`/`put`/`delete`,
  implementiert von `DirectMutator` @421 (committed) und `TxMutator`.
- **Index** (`src/index.rs`): `find_m<M: Mutator>` @125 ist bereits **generisch**
  über eine Mutator-Sicht und **verifiziert** jede Kandidaten-Entity
  (`field_value_m` @91, `within` @105). `find` @160 = committed-Wrapper.
  Key-Encoding ist **ordnungserhaltend** (`ordering::encode_ordered`, index.rs:72;
  `value_cmp` index.rs:150); Index-Keys sortieren nach `(enc_value, entity_id)`.
- **Oracle** (`tests/query.rs`): `oracle()` @133 = Full-Scan-Referenz für die
  committete Ausführung.

---

## Scope (v0.6)

1. **Transactional Query Execution** — Query-Executor gegen `TxMutator`.
2. **Cost-Based Planning** — einfaches, explizites Kostenmodell (keine Statistik-Infrastruktur).
3. **Index-Order / Top-K** — `ORDER BY indexed_field LIMIT n` als bounded Read.

### Nicht-Ziele (explizit)

- **kein MVCC** — es bleibt genau eine Transaktion pro Store (`&mut`-Borrow).
- **keine Concurrent Writers** — weiterhin Einzelprozess, ein Thread.
- **keine Snapshot-Isolation** — eine Tx sieht committed + eigene Writes (Status quo).
- **keine JOINs, keine Aggregationen, kein SQL, kein Distributed Querying.**

### Übergreifende Invariante (unantastbar)

> **Der Index ist niemals die Wahrheit.**

Auch ein Index-Order-Scan darf nie einfach die ersten `n` Indexeinträge
zurückgeben. Jede Kandidaten-Entity wird gegen ihren echten Wert verifiziert;
Tombstones, stale Index-Einträge und Residual-Prädikate werden korrekt
berücksichtigt (siehe Teil 3).

**Harte Invariante für `IndexOrderScan` (zusätzlich festgeschrieben):**

> **Der Operator darf niemals aufgrund der Index-Order ein Entity-Ergebnis
> erzeugen, das ein Full-Scan-Oracle nicht erzeugen würde.**

Index-Order ist eine **Ausführungsoptimierung, keine neue Semantik**. Die
Presence-Garantie entscheidet nur, ob der Index als vollständige
Kandidatenquelle für das Sortierfeld verwendet werden darf; Predicate-Residuals
werden weiterhin exakt ausgewertet.

---

## Teil 1 — Transactional Query Execution

### Ziel

`store.query(...)`-Semantik auch **innerhalb einer Transaktion**: eine Query
sieht committete Daten **und** die eigenen, noch uncommitteten Writes
(Read-your-own-writes), für Entity- **und** Index-Daten über dasselbe
Pending-Overlay, mit derselben Predicate-Semantik wie committed queries.

### Warum das ohne Mutation an v0.5.x geht

`TxMutator` implementiert bereits das volle `Mutator`-Interface (lazy `scan` mit
committed∘pending-Merge). Die nötigen Bausteine existieren:

- `FullScan` braucht nur `M::scan` über den Collection-Präfix
  (`keycodec::collection_prefix` + `successor`) — funktioniert für
  `DirectMutator` und `TxMutator` identisch.
- `IndexScan` braucht `find_m<M>` (bereits generisch, inkl. Verifikation).
- `Fetch` braucht `core_get_entity(schema, &mut M, ...)` (Punkt-Lookup über
  dieselbe Sicht; `core_get_entity` ist bereits `Mutator`-generisch, entity.rs).

### Architektur: Executor generisch über `M: Mutator`

Der Executor wird von `&mut Database` auf **`&mut M` wo `M: Mutator`**
verallgemeinert (mechanische Änderung der Signaturen):

- `exec_rows<'m, M: Mutator>(m: &'m mut M, schema: &'m Schema, plan) -> Result<RowStream<'m>>`
- `scan_collection_stream` / `ScanAssembler`: bauen über `m.scan(...)` statt `db.scan_stream(...)`.
- `fetch_stream` / `FetchIter`: Punkt-Lookup via `core_get_entity(schema, m, ...)` statt `DirectMutator`.
- `candidate_ids` / `index_scan`: `find_m(m, ...)` statt `index::find(db, ...)`.
- `sort_rows` bleibt unverändert (rein).

Zwei öffentliche Einstiege, gleiche Kernlogik:

```rust
// committed (lazy Pushdown, Status quo):
pub fn run(db: &mut Database, schema: &Schema, plan: &PhysicalPlan)
    -> Result<Vec<(String, Entity)>>;              // hüllt DirectMutator
// transaktional (collectiv, da der Borrow nicht aus der Methode entweichen darf):
pub fn run_tx(m: &mut TxMutator, schema: &Schema, plan: &PhysicalPlan)
    -> Result<Vec<(String, Entity)>>;
```

**Begründung Eager bei Tx:** Eine Tx-Query kann keinen `RowStream` zurückgeben,
der `&mut self` (und damit `store.db` + `pending`) über die Methodengrenze
borrowt — das wäre self-referenziell. Deshalb wird die Tx-Query **eager**
(materialisiert) über `TxMutator` ausgeführt; das entspricht exakt der
bestehenden `Transaction::scan_collection` (entity.rs:668), die ebenfalls eager
ist. Der **lazy Pushdown** (`Limit` = `take`, hört auf zu ziehen) wirkt auch im
eager Pfad weiter, weil die Operator-Verkettung intern dieselbe ist.

> **Freigabe-Bestätigung (Punkt 1):** Committed queries bleiben lazy;
> `Transaction::query()` nutzt den bestehenden Pending-Overlay und darf eager
> sein. **Wichtig:** dieselbe Predicate-/Filter-Semantik wie committed,
> insbesondere **Missing-Field** und **`Not`** (exakt dieselbe `eval`).

### Public API (neu)

Auf `Transaction<'a>` (entity.rs:605-760):

```rust
pub fn query(&mut self, collection: &str) -> Result<QueryBuilder>;
pub fn execute_query(&mut self, builder: QueryBuilder) -> Result<Vec<(String, Entity)>>;
```

`Transaction::execute_query` plant via `query::planner::plan(&self.store.schema, logical)`
und führt über `run_tx` mit `TxMutator { db: &mut self.store.db, pending: &mut self.pending }`
aus. **Schema bleibt read-only** (v0.5.2-Invariante): `plan`/`execute_query` mutieren
kein Schema; unbekannte Collection ⇒ leeres Ergebnis.

### Korrektheit / Crash / Abort

- **Read-your-own-writes:** `TxMutator`-Merge garantiert, dass ein Tx-Write
  (`update`/`delete`) in Entity- und Index-Keys über dem committed Zustand
  gewinnt (pending first, lib.rs:118-145; Tombstone = `None` shadowed).
- **Abort:** `abort()` verwirft `pending` (entity.rs:754). Da die Query eager
  über `TxMutator` lief und nichts Persistentes geschrieben hat, beeinflusst ein
  Abort keine committeten Daten und keine Query-Ergebnisse anderer Pfade.
- **Crash:** Tx-Mutationen sind bis zum WAL-`Commit`-Record nicht durable
  (entity.rs:718-750). Eine Query ist read-only und berührt den WAL nie.

### Testplan Teil 1

- **Tx-Query-Oracle** (neu, `tests/tx_query.rs`): in einer aktiven Tx
  zufällige Mutationen (`update`/`delete`) + `execute_query` gegen einen
  **Full-Scan-Oracle über das Tx-Overlay** (committed∘pending) prüfen —
  gleiche Predicate-Semantik wie das bestehende `tests/query.rs`-Oracle.
- **Read-your-own-writes:** nach `update` sieht eine Tx-Query den neuen Wert,
  nach `delete` keinen Treffer; nach `abort` wieder den committed Wert.
- **Index-pfad:** Tx-Query mit `IndexScan`-Auswahl (via `find_m` über `TxMutator`)
  liefert identische Ergebnisse wie der Full-Scan-Oracle (Index ≠ Wahrheit).
- **Kein Schema-Mutate:** Tx-Query auf unbekannter Collection ⇒ leer, kein
  `SCHEMA`-Write (v0.5.2-Invariante, `tests/hardening.rs`-Stil).

---

## Teil 2 — Cost-Based Planning

### Ziel

Mehrere mögliche Index-Kandidaten pro AND-Klausel durch ein **explizites,
deterministisches Kostenmodell** auswählen; Residual-Filter bleiben korrekt.

### Ist-Zustand vs. v0.6

Heute: `pick_index_field` (planner.rs:126-148) wählt pro Klausel **genau ein**
Feld — `Eq` bevorzugt, sonst lex kleinstes Feldname; die restlichen indexierbaren
Felder landen als Residual in `Filter`. Keine Cardinality.

### Kostenmodell (einfach, explizit, keine Statistik)

`cost(candidate) = base_cardinality * selectivity(shape)`

- `base_cardinality` = konstanter Platzhalter (z. B. `1.0`), **keine**
  Statistiken/Histogramme. Deutlich gemacht: dies ist ein Heuristik-Wert, später
  durch echte Cardinality ersetzbar.
- `selectivity(shape)` je indexierbarem Literal:
  - `Eq` → `0.1` (höchste Selektivität, wenige Treffer)
  - `Between` → `0.25`
  - einseitige Range (`Gt`/`Gte`/`Lt`/`Lte`) → `0.5`
- Bei **gleichem Shape** zweier Range-Kandidaten zählt die **gebundene Enge**:
  das Literal mit dem strikt engeren Bound ist selektiver (nutzt die bereits
  vorhandenen `lower_stronger`/`upper_stronger`, planner.rs:157-209). Z. B. ist
  `age > 60` selektiver als `age > 18`, `age < 30` enger als `age < 100`.
- **Deterministischer Tie-Break:** bei gleichem Cost lex kleinstes Feldname
  (erhält Reproduzierbarkeit der Pläne).

Der Optimizer wählt pro Klausel den Kandidaten mit minimalem Cost.
**Korrektheit unverändert:** alle nicht gewählten indexierbaren Felder + alle
nicht-indexierbaren/negierten Literale bleiben im Residual-`Filter`
(planner.rs:268-272, unverändert). Es gibt **keine** neue Semantik — nur eine
andere Kandidaten-Wahl. `explain_query` (entity.rs:598) zeigt den gewählten
Plan.

### Datei-/Signaturänderungen

- `pick_index_field` wird durch `pick_index_field_cost` ersetzt (Cost-Modell +
  deterministischer Tie-Break). Signatur von `plan` bleibt `(schema, logical)`,
  da das Kostenmodell rein Shape-basiert ist (kein Statistik-Snapshot nötig).

### Testplan Teil 2

- **Plan-Auswahl deterministisch:** bei mehreren Index-Kandidaten liefert
  `explain_query` stets denselben Plan (mehrfach, gleiche Eingabe).
- **Cost-Ranking:** `Eq`-Feld vor Range-Feld; bei zwei einseitigen Ranges das
  mit engerem Bound (z. B. `age > 60 AND salary > 10` wählt `age`).
- **Residual-Korrektheit:** Ergebnisse einer geänderten Planwahl == Ergebnisse
  des Full-Scan-Oracles (bestehendes `random_queries_match_oracle`,
  tests/query.rs:273) — dadurch ist die neue Wahl verifiziert, ohne die
  Semantik zu ändern.

---

## Teil 3 — Index-Order / Top-K

### Ziel

`ORDER BY indexed_field LIMIT 50` als **bounded Read** statt
`Sort(alle N)` → `Limit(50)`.

### Vom Ist zum Soll

Heute erzeugt der Planner `Sort{Limit{...}}` (planner.rs:371-383); `Sort`
blockiert über **allen** N Zeilen. Die Index-Keys sind bereits
ordnungserhaltend nach `(enc_value, entity_id)` sortiert (index.rs:72,150) —
die Sortierung liegt also schon **im Index** vor. Das soll genutzt werden.

### Neuer Operator: `IndexOrderScan`

`PhysicalPlan::IndexOrderScan { collection, field, lower, upper, dir }`
(physical.rs enum erweitern, `kind()` @68, `input()` @56 erweitern).

Semantik:
- Streamt **verifizierte** `(id, Entity)`-Zeilen **in Index-Reihenfolge**
  (`dir`-Richtung), **lazy**, so dass ein vorgeschaltetes `Limit` früh aufhört.
- Für jede Kandidaten-Entity wird der **echte Wert** gelesen und der
  Residual-Prädikat geprüft (Verifikation; Tombstones/stale Einträge werden
  übersprungen). Der Operator ist damit **selbst ein Full-Scan-äquivalentes,
  aber geordnetes und früh abgebrochenes** Zugriffspfad-Primitiv — er darf nur
  emittieren, was wirklich zur Query passt (Invariante).
- **Asc:** Vorwärts-Scan des Index-Ranges.
- **Desc:** benötigt **Reverse-Iteration** über den Index-Range. Es existiert
  aktuell **kein** rückwärts laufender `ScanStream` (nur Vorwärts, lib.rs:410).
  v0.6 führt einen kleinen **Reverse-Adapter** über eine `ScanStream` ein
  (oder einen Helper, der den begrenzten Index-Range in umgekehrter Richtung
  konsumiert), damit auch Desc bounded bleibt.

### Enablement-Regel (Korrektheit, verhindert Semantikänderung)

Ein Index-Order-Scan enthält **nur Entities, die das Feld haben** (Index deckt
kein Missing-Feld ab). Der aktuelle `Sort` (executor.rs:274-306) sortiert fehlende
Felder als kleinste (Asc) bzw. größte (Desc) **mit** — eine unbedingte Umstellung
würde das Ergebnis ändern. Deshalb gilt:

**IndexOrderScan wird nur eingesetzt, wenn das Sortierfeld für jede
möglicherweise treffende Zeile **garantiert vorhanden** ist** — d. h. ein
positives, indexierbares Literal auf diesem Feld in **jeder** DNF-Klausel
auftritt (z. B. `WHERE age > 30 ORDER BY age LIMIT 50`). In dem Fall enthalten
alle Kandidaten das Feld, und der geordnete Index-Scan ist exakt äquivalent.

- **Wenn Regel erfüllt:** Plan wird `Limit{ Filter{ IndexOrderScan } }`
  (ohne `Sort`). Bounded: liest nur bis `K` gültige Zeilen (+ verworfenen
  Kandidaten).
- **Sonst:** unveränderter Fallback `Sort{Limit{...}}` (korrekt, unbounded).
  Das hält die v0.5.x-Ergebnisse für `ORDER BY x LIMIT n` **ohne**
  Vorhandenseins-Garantie byte-identisch.

`sort_rows`-Tie-Break (entity-id, Asc) entspricht der Index-Key-Ordnung
`(value, entity_id)` → identisches Ergebnis für die abgedeckten Fälle.

### Datei-/Signaturänderungen

- `physical.rs`: neuer Variant `IndexOrderScan`.
- `planner.rs`: in Schritt 3 (Sort/Limit, planner.rs:371-383) wird bei
  erfüllter Enablement-Regel `Sort` entfernt und ein `IndexOrderScan`
  (mit `Filter`-Residual + `Limit`) gebaut; sonst Status quo.
- `executor.rs`: neuer Fall `PhysicalPlan::IndexOrderScan` (lazy, geordnet,
  verifizierend) + Reverse-Adapter für Desc.
- `explain.rs`: Kind-String `IndexOrderScan` + ggf. `dir`.

### Testplan Teil 3

- **Bounded / Top-K:** `ORDER BY age LIMIT 50` über genügend große Collection;
  Asser, dass nur `K` Entitäten gelesen/verifiziert werden (Zähler-Sonde im
  Test oder Verifikations-Punkt-Lookup zählt).
- **Korrektheit (Invariante):** Tombstones und stale Index-Einträge werden
  übersprungen; Residual-Prädikat wird angewendet; Ergebnis == Full-Scan-Oracle
  (`ORDER BY age` ohne `LIMIT` == `sort_rows`-Referenz).
- **Missing-Feld:** ohne Vorhandenseins-Garantie greift der Fallback
  (`Sort`); Ergebnis byte-identisch zum v0.5.2-Verhalten (Regressionstest).
- **Desc:** Reverse-Order == `sort_rows(Desc)`-Referenz (mit Vorhandenseins-Garantie).

---

## Übergreifende Kompatibilität (v0.5.x)

- **Committed queries:** `execute_query`/`explain_query` (entity.rs:590/598)
  und deren Ergebnisse bleiben unverändert (Fallback-`Sort` beibehalten;
  `IndexOrderScan` nur unter Enablement-Regel).
- **Tx-API:** `update`/`delete`/`get`/`scan_collection`/`find`/
  `commit`/`abort` (entity.rs:618-759) unverändert; nur **neue** Methoden
  `query`/`execute_query` kommen auf `Transaction` hinzu.
- **Mutator-Trait** (lib.rs:405) bleibt unverändert — nur seine Nutzer im
  Executor werden generalisiert. `ScanStream` @418 unverändert.
- **Fehler-Taxonomie:** keine neuen Error-Varianten. `run_tx` nutzt dieselben
  `Error`-Werte (z. B. `InvalidArgument` für inaktive Tx via `check_active`).
- **Schema-Invariante (v0.5.2):** keine neue Lese-Operation mutiert das Schema;
  `Transaction::execute_query` ist read-only.

## Geänderte / neue Dateien (Vorausschau, nach Freigabe)

- `src/query/executor.rs` — Generalisierung über `M: Mutator`, `run_tx`,
  `IndexOrderScan` (+ Reverse-Adapter).
- `src/query/physical.rs` — neuer `IndexOrderScan`-Variant.
- `src/query/planner.rs` — `pick_index_field_cost`, Enablement-Regel.
- `src/query/explain.rs` — `IndexOrderScan`-Ausgabe.
- `src/entity.rs` — `Transaction::query` / `Transaction::execute_query`.
- `tests/tx_query.rs` (neu) — Tx-Query-Oracle.
- `tests/index_order.rs` (neu) — Top-K/Bounded/Desc-Referenz.
- `README.md` — v0.6-Abschnitt + Roadmap.

## Entscheidungen, die hier festschreiben werden (bitte bestätigen)

1. **Tx-Queries sind eager** (materialisieren über `TxMutator`); lazy bleibt nur
   der committed Pfad. Kein neues Statistik-/Snapshot-Primitiv.
2. **Cost-Modell ist Shape-basiert** mit konstantem `base_cardinality` —
   bewusst **keine** Statistik-Infrastruktur in v0.6.
3. **IndexOrderScan nur bei Vorhandenseins-Garantie** des Sortierfelds;
   sonst Fallback `Sort` (hält v0.5.x-Ergebnisse byte-identisch).
4. **Desc braucht einen Reverse-Adapter** über den Index-Range (neues kleines
   Primitive), damit auch absteigend bounded ist. Alternative (falls Reverse
   nicht erwünscht): v0.6 beschränkt Index-Order auf `Asc`, Desc bleibt `Sort`.
5. **Reihenfolge der Umsetzung:** Teil 1 → Teil 2 → Teil 3 (wie vorgegeben).

---

## Freigabe-Protokoll (v0.6 eingefroren)

Alle fünf Punkte freigegeben, mit dieser Bestätigung zu Punkt 4:

> **Reverse-Adapter jetzt bauen.** Asc und Desc symmetrisch. Der Adapter darf
> klein und ausschließlich für den bestehenden bounded Index-Scan sein — keine
> weitergehende Index-API-Ausweitung. Wichtig: gleiche `[start,end)`-Semantik
> und korrekte Behandlung von Bounds/Ties.

Zusätzlich festgeschriebene harte Invariante (siehe oben): Index-Order darf nie
ein Entity-Ergebnis erzeugen, das ein Full-Scan-Oracle nicht erzeugen würde.
Presence-Garantie entscheidet nur über die Zulässigkeit der Kandidatenquelle.

**v0.6 = Tx-Queries → Shape-Costing → Asc/Desc Index-Order + Top-K**, in genau
dieser Reihenfolge. Ohne MVCC, ohne Statistiksystem, ohne sonstige v0.7-Themen.

---