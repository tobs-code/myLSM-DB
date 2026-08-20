# v0.8 — Query-Erweiterung: Projektion + Aggregation (Spezifikation)

> **Phase: Spezifikation (kein Code).** Basis: `0e62b6e`. Diese Spezifikation
> ist die verbindliche Semantik für Implementierung + Oracle-Tests.

## 0. Scope

**IN v0.8:**
- Projektion als **Query-Ergebnisform** (Produktfeature).
- Aggregationen: `count`, `sum`, `avg`, `min`, `max`.
- Kombination mit dem bestehenden Query-Pfad (Filter / Index-Scan / Sort /
  Limit / Top-K / Residual-Filter).
- Oracle-gestützte Korrektheit.

**OUT of scope (explizit):**
- Projektion als **Storage-/Decode-Optimierung** — der v0.10-Befund gilt
  weiterhin: isolierter Decode-Gewinn nur bei `requested ≪ entity_fields`,
  im Real-Traffic langsamer; **nicht** in v0.8.
- **Format-Versionierung / Migration** — eigener, separater Arbeitsauftrag.
- Join, Group-By, Composite-/Unique-/Text-Index, Concurrency/MVCC,
  Netzwerk/Server.

---

## 1. Ziel

Die Datenbank liefert heute gefilterte, sortierte, limitierte **ganze
Entities**. v0.8 fügt zwei eigenständige, semantisch klar definierte
Ergebnisformen hinzu:

1. **Projektion** — Rückgabe nur benannter Felder (in angeforderter
   Reihenfolge).
2. **Aggregation** — skalare Auswertung über die Ergebnismenge.

Beides sind *Resultat-Formen*, keine Speicher- oder Decode-Pfade.

---

## 2. Projektion (Query-Result-Form)

### 2.1 API-Skizze

```rust
let b = store.query("users")?
    .filter(ge("age", Value::Int(30)))?
    .project(&["name", "email"])?;   // nur diese Felder, in dieser Reihenfolge
let rows: Vec<(String, Entity)> = store.execute_query(b)?;
```

`builder.project(fields: &[&str])` setzt die Projektion; `execute_query`
liefert projizierte Entities.

### 2.2 Feldliste & Reihenfolge

- Es werden **genau die angeforderten Felder** zurückgegeben, die auf der
  Entity **vorhanden** sind.
- Die **Reihenfolge der Ergebnisfelder folgt der Reihenfolge der Anfrage**
  (nicht der natürlichen Feld-Sortierung). Der Executor baut die projizierte
  Entity so auf, dass die Felder in Anfrage-Reihenfolge geliefert werden.

### 2.3 Unbekannte / stale Felder

- Ein angefordertes Feld, das auf einer Entity **fehlt** (nie gesetzt oder
  durch `put` als stale entfernt), wird für **diese** Entity **weggelassen**
  (kein Fehler, keine `null`-Spalte).
- Streng unterschieden wird *absent* (Feld nicht vorhanden → weggelassen)
  vs. *present but `Value::Null`* (Feld vorhanden, Wert Null →
  **eingeschlossen** als `Null`).

### 2.4 Leere Projektion

- Eine **leere Feldliste** ist ein Fehler: `Error::InvalidArgument`
  (`"projection requires at least one field"`).
- (Id-only-Ergebnisse sind nicht Ziel von v0.8; wer nur IDs braucht, nutzt
  den Entity-Key aus dem Ergebnistupel.)

### 2.5 Ergebnistyp

`execute_query` liefert wie bisher `Vec<(EntityId, Entity)>`; die projizierte
`Entity` enthält nur die vorhandenen angeforderten Felder in Anfrage-Reihe.
Bestehende, nicht-projizierende Queries sind **unverändert** (vollständige
Entities).

---

## 3. Aggregationen

### 3.1 Operationen & Ziel-Feld

```rust
let b = store.query("users")?.filter(ge("age", Value::Int(18)))?;
let n: Option<Value> = store.execute_aggregate(b.aggregate(Agg::Count))?;
let sum_age = store.execute_aggregate(b.aggregate(Agg::Sum("age")))?;
```

- `count` — Anzahl der Zeilen der (gefilterten, sortierten, limitierten)
  Ergebnismenge. **Kein** Ziel-Feld.
- `sum` / `avg` / `min` / `max(field)` — benötigen ein **Ziel-Feld** (zum
  numerischen Wert, siehe 3.3).

### 3.2 NULL / fehlende Felder

- Ein Wert, bei dem das Ziel-Feld **absent** ist oder `Value::Null` ist,
  wird bei numerischen Aggregationen **übersprungen** (nicht als 0 gezählt,
  kein Fehler).
- `count` zählt **alle** Zeilen unabhängig vom Vorhandensein des Feldes.

### 3.3 Nicht-numerische Werte

- Hat eine Zeile das Ziel-Feld, aber mit nicht-numerischem Typ
  (`String`/`Bytes`/`Bool`), wird sie bei numerischen Aggregationen
  **übersprungen** (konsistent zu 3.2). Kein Laufzeitfehler.

### 3.4 Integer / Float & Overflow

- `sum` über `Int64`-Werten: Akkumulation in `i128`; liegt das Ergebnis
  außerhalb `[i64::MIN, i64::MAX]`, wird **satiert** auf die jeweilige
  Grenze (deterministisch, kein neuer Fehlercode).
- `avg` liefert immer `Float64`: `avg = (f64)Σ / (f64)Anzahl_non_null`.
  Bei `Anzahl_non_null == 0` → `None`.
- `min` / `max`:
  - rein `Int64`-Eingaben → Ergebnis `Int64` (kein Overflow-Risiko).
  - sobald **ein** `Float64` unter den Werten ist, wird zur `f64` promoviert
    und das Ergebnis ist `Float64`.
  - nicht-endliche Floats (`NaN`, `Inf`) werden wie NULL **übersprungen**.

### 3.5 Ergebnistyp & leere Menge

`execute_aggregate(builder) -> Result<Option<Value>>`:

| Op | Typ (Werte vorhanden) | leere / nur-NULL Menge |
|----|-----------------------|------------------------|
| `count` | `Int64` (≥ 0) | `0` (niemals `None`) |
| `sum` | `Int64` (satiert) | `None` |
| `avg` | `Float64` | `None` |
| `min` / `max` | `Int64` bzw. `Float64` | `None` |

`None` = SQL-artiges NULL (keine Zeile / keine nicht-NULL-Zahlwerte).

---

## 4. Kombination mit bestehendem Query-Pfad

### 4.1 Reihenfolge

```
Scan (Index / Full) -> Filter (Index-Praedikat + Residual) -> Sort / Limit / Top-K -> Projection | Aggregation
```

- **Scan** liefert Kandidaten `(id, entity)`.
- **Filter** reduziert auf passende Zeilen (Index-Praedikat + Residual-Filter).
- **Sort / Limit / Top-K** ordnen / begrenzen die Zeilenmenge.
- **Terminal-Schritt** (genau einer, siehe 4.2): Projektion oder Aggregation.

### 4.2 Terminal-Schritt (Projection vs. Aggregation)

Projektion und Aggregation sind **wechselseitig exklusiv**: ein Query setzt
ENTWEDER eine Projektion ODER eine Aggregation. Beides gleichzeitig ->
`Error::InvalidArgument`. Ohne beides bleibt das Verhalten wie heute
(vollstaendige Entities).

Aggregation wird ueber die **bereits gefilterte, sortierte, limitierte**
Zeilenmenge berechnet. Beispiel: `limit(10)` + `count` = Anzahl der nach
Limit verbleibenden Zeilen (<= 10). `min`/`max` mit `limit` = Extremwert
ueber die begrenzte Menge. Das ist die definierte Semantik der obigen
Reihenfolge.

### 4.3 Index- / Top-K-Optimierung erlaubt

Der Planner **darf** optimieren (z. B. `min`/`max` auf indexiertem Feld ueber
IndexOrderScan in O(1)/O(k); `count` ueber Index-Range; Top-K statt voller
Sortierung). Voraussetzung: das Ergebnis muss identisch zum Oracle (5) sein.
Decode-Projektion (v0.10) ist davon **nicht** betroffen und bleibt out.

---

## 5. Oracle-Strategie

### 5.1 Referenz = naive Auswertung

Die Oracle-Implementierung wertet **naiv** aus:
1. Alle Entities der Collection laden (voller Scan, kein Index).
2. Residual-Filter + Praedikat in-memory anwenden.
3. Sortieren / Limitieren (Top-K durch volle Sortierung).
4. Danach Projektion bauen bzw. Aggregation ueber die finale Zeilenmenge
   berechnen - exakt nach 2 / 3.

Das Oracle ist die **Ground Truth**; es nutzt keine Index-Optimierung.

### 5.2 Planner darf optimieren

Der Produktions-Executor darf Index-Scan, IndexOrderScan, Top-K und
(ausschliesslich als Result-Form) Projektion verwenden. Der Test vergleicht
`Executor-Ergebnis == Oracle-Ergebnis` fuer identische Queries.

### 5.3 Edge-Case-Tests (verpflichtend)

Oracle-Test muss abdecken:
- leere Collection,
- alle Zeilen durch Filter eliminiert,
- Feld bei einigen Entities absent,
- Feld explizit `Value::Null`,
- nicht-numerisches Feld beim Aggregations-Ziel,
- gemischte `Int64` / `Float64`-Werte bei `min` / `max`,
- `sum`-Overflow (Saettigung),
- `avg` bei Division durch 0 (keine non-null Werte -> `None`),
- leere Projektion -> `InvalidArgument`,
- einzelne Zeile, Duplikat-Werte bei `min` / `max`,
- `count` mit und ohne `limit`,
- Projektion mit stale / unknown-Feld (weggelassen, keine Panic).

Bestehende `tests/query.rs` (Oracle-Random) wird um Projektion +
Aggregation erweitert; bestehende Assertionen bleiben unveraendert gruen.

---

## 6. Abgrenzung / Guardrails

### 6.1 Projection != Storage-Optimierung (v0.10 gilt)

> Projektion als **Query-Ergebnisform** ist ein Produktfeature von v0.8.
> Projektion als **Storage- / Decode-Optimierung** ist **nicht** Bestandteil
> von v0.8.

Der v0.10-Befund (isoliertes Decode-Only bringt im Real-Traffic keinen
Gewinn, oft Verlust) wird **nicht** ueber diese Spezifikation revidiert.
Implementierung darf Decode **nicht** nach requested-Feldern einschraenken.

### 6.2 Format-Versionierung out of scope

Format-Versionierung / Migration ist ein **eigener** Arbeitsauftrag und wird
**nicht** in v0.8 gezogen. v0.8 aendert das on-disk-Format nicht.

### 6.3 Keine Performance-Optimierung ohne Messbefund

Wie in der DoD gefordert: keine Optimierung (auch keine Decode-Pfad-
Aenderung) ohne vorherigen, dokumentierten Messbefund. Semantik vor Speed.

---

## 7. Definition of Done (v0.8)

Der Zyklus ist fertig, wenn:
- die Query-Semantik schriftlich feststeht (dieses Dokument),
- Aggregationen + Projektion implementiert sind,
- bestehende Queries unveraendert funktionieren,
- Oracle-Tests die neuen Operationen abdecken,
- Edge Cases definiert und getestet sind,
- `cargo test --release` vollstaendig gruen ist,
- keine Performanceoptimierung ohne Messbefund eingefuehrt wurde,
- und ein neuer Git-Checkpoint gepusht ist.

**Committed-Status bleibt bis zum Checkpoint `0e62b6e`.**
