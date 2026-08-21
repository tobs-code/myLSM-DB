# myLSM-DB — Produkt-/Query-Roadmap-Inventur (read-only)

**Status:** Read-only Inventur. **Kein Code.** `c400bb0` (v1.2) bleibt eingefrorener
Produkt-Checkpoint.
**Auftrag:** Kandidaten für die v1.3/v1.5-Produktfläche gegeneinander stellen und
die Priorisierungshypothese (Composite Indexes) bestätigen oder verwerfen.

Checkpoint-Linie:

```text
v1.0  5daa675  Tooling
v1.1  74f8a2f  Backup / Restore
v1.2  c400bb0  CAS + Partial Updates   ← frozen
```

---

## §1 Bestand: Query-/Index-Fläche heute (Forensik)

- **Pipeline:** `Filter → Sort → Limit → Projection → Aggregation` (v0.5/v0.6/v0.8).
- **Index ist streng single-field:** `IndexDef { id, collection_id, field_id, status }`
  (`src/schema.rs:133`), Index-Key `encode_index_key(cid, field_id, encoded(value), eid)`.
- **Planner wählt pro AND-Klausel genau *einen* Index** (`pick_index_field_cost`,
  `src/query/planner.rs:184`, Aufruf :355). Ein zweites Prädikat in derselben Klausel
  wird zum **Residual-`Filter`** (In-Memory über die Index-Kandidaten-IDs). `OR`
  → `UnionIds` aus IndexScans pro Klausel, aber innerhalb einer Klausel immer nur ein
  führender Index.
- **Keine Index-Intersection:** Multi-Feld-`AND` wird nicht über Schnittmenge zweier
  Index-Scans gelöst, sondern über Index-Scan + nachgelagertes Filtern.
- **Index ist nie Wahrheitsquelle:** `IndexScan` liefert *verifizierte* IDs
  (`index::find_m` prüft gegen die Entity, `src/query/executor.rs:273`/`index.rs`).
  Jeder neue Index-Typ erbt diese Verifikation automatisch.
- **Schreibpflege der Indexe** liegt in `core_put_entity` (schreibt Index-Keys im
  Zuge des Entity-Writes, `src/entity.rs` Index-Block). Das ist exakt der Pfad, den
  v1.2 via `core_cas_entity` bereits nutzt — neue Index-Typen hängen dort an, ohne
  einen Sonderpfad.
- **`IndexOrderScan`** (v0.6) nutzt einen sortierten Index für `ORDER BY indexed_field`
  (Enablement-Regel `index_order_enabled`, `planner.rs:418`).

**Fazit der Lücke:** Ein realer Filter `age = 30 AND city = "berlin" AND active = true`
nutzt **höchstens einen** Index; die übrigen Spalten werden zeilenweise im Speicher
gefiltert. Bei großen Collections ist das der nächste messbare Engpass der Query-Fläche.

---

## §2 Kandidaten-Bewertung

### A. Composite Indexes  (Prioritäts-Hypothese)
- **Was:** Sekundärindex über eine *geordnete Liste* von Feldern; Range/Prefix-Lookup
  auf führenden Spalten, `ORDER BY` auf Präfix.
- **Anknüpfung:** `IndexDef.field_id` → `field_ids: Vec<u32>`; neuer Key-Encoding
  (Verkettung der `ordered`-Werte der Komponenten + `eid`); `core_put_entity` schreibt
  zusätzlich Composite-Keys; `find_m` → `find_composite` mit Prefix-Bounds; Planner
  wählt Composite-Index, dessen führende Spalten durch die Klausel constraints sind.
- **Aufwand:** **M** (neuer Index-*Typ* im bestehenden Pfad; keine neue Engine-Ebene).
- **Risiko:** **Niedrig–Mittel.** Erbt Index-Verifikation + WAL-Atomarität; einzige
  heikle Stelle ist die Prefix-Range-Semantik (Bounds über führende Spalten).
- **Nutzen:** **Hoch** für die vorhandene Query-Fläche — hebt Multi-Feld-Filter und
  `ORDER BY` auf mehrere Spalten auf Index-Ebene; synergiert mit `group_by`
  (sortierter Index → Streaming-Gruppierung).
- **Abhängigkeiten:** keine (unabhängig von Concurrency/MVCC).

### B. Snapshots / Read-Consistency
- **Was:** Konsistenter Lese-Zeitpunkt über mehrere Reads.
- **Anknüpfung:** Heute Single-Writer, keine Versionierung → kein natürlicher
  Snapshot-Anker. Bräuchte MVCC (Sequence/Version pro Wert) oder Clone des
  committeten Zustands.
- **Aufwand:** **L** (größter architektonischer Block).
- **Risiko:** **Hoch** (berührt Speicher-/WAL-/MemTable-Semantik, hebelt den
  "Index nie Wahrheit"-Vertrag nicht, aber fügt Versionierung überall hinzu).
- **Nutzen:** **Niedrig im Heute** — bei Single-Writer sind committete Reads bereits
  konsistent; Snapshot lohnt erst mit Nebenläufigkeit.
- **Abhängigkeiten:** Concurrency (noch nicht vorhanden).

### C. Remote Backup
- **Was:** v1.1-Backup-Root streamen/wiederherstellen zu Object-Storage.
- **Anknüpfung:** Baut auf das **stabile v1.1-Backup-Primitive** (konsistenter Root:
  VERSION + MANIFEST + SST + WAL-Checkpoint). Ist im Kern ein I/O-Adapter über ein
  already-consistentes Verzeichnis.
- **Aufwand:** **M–S** (Transport/Adapter, keine Engine-Logik).
- **Risiko:** **Niedrig** (berührt die Engine nicht).
- **Nutzen:** **Operations/Deployment**, nicht Query-Produktfläche.
- **Abhängigkeiten:** keine; kann jederzeit nebenbei erfolgen.

### D. Read-only Transactions / bessere Read-Semantik
- **Was:** Gebündelte, konsistente Reads ohne Write.
- **Anknüpfung:** Bei Single-Writer weitgehend Deckglas über bestehende `get`/Scans.
- **Aufwand:** **S**.
- **Risiko:** **Niedrig**.
- **Nutzen:** **Gering als Alleingang** — liefert nur dann echten Mehrwert, wenn es
  einen Snapshot gibt (→ hängt an B). Sonst Convenience, keine neue Fähigkeit.
- **Abhängigkeiten:** effektiv Snapshots (B).

### E. Weitere Query-Funktionen (`group_by`, mehr Aggregationen, komplexe Ausdrücke)
- **Was:** Gruppierung, erweiterte Aggregation, boolesche/arithmetische Ausdrücke.
- **Anknüpfung:** Erweitert den bestehenden Executor/Planner; `group_by` profitiert
  stark von einem **sortierten** Index (Composite oder Single) → Streaming statt
  Hash-Materialisierung.
- **Aufwand:** **M** (inkrementell, baut auf v0.8 auf).
- **Risiko:** **Niedrig–Mittel**.
- **Nutzen:** **Mittel–Hoch**, besonders `group_by` als Ergänzung zur Aggregations-
  fläche.
- **Abhängigkeiten:** optimal kombiniert mit A (Composite Index liefert die
  Sortier-Reihenfolge für `group_by`).

---

## §3 Gegenüberstellung der drei Hauptkandidaten

| Kandidat | Arch-Block | Risiko | Nutzen (Query) | Sofort machbar? |
|----------|-----------|--------|----------------|-----------------|
| A Composite Index | M (neuer Typ im Pfad) | Niedrig–Mittel | Hoch | Ja (kein MVCC) |
| B Snapshots | L (MVCC) | Hoch | Niedrig (heute) | Nein (braucht Concurrency) |
| C Remote Backup | S–M (Adapter) | Niedrig | Ops, nicht Query | Ja, aber nicht Kern |

---

## §4 Hypothese-Check

> *Composite Indexes sind der nächste sinnvolle Produktschritt, weil der heutige
> Index nur ein Feld adressiert und reale Abfragen mehrere Felder kombinieren.*

**Bestätigt durch Forensik:**
- `IndexDef` ist single-field (`schema.rs:133`) → kein Multi-Feld-Index existiert.
- Planner nutzt pro Klausel **genau einen** Index; weitere Prädikate sind
  Residual-Filter (`planner.rs:184/355`) → Multi-Feld-`AND` wird nicht index-effizient
  bedient.
- Composite hängt sauber am bestehenden `core_put_entity`-Index-Pfad + der
  Index-Verifikation („nie Wahrheit") an → keine Architektur-Umgehung, im Geiste von
  v1.2.

**Einschränkung:** Composite Index ist ein *Index-Typ*-Feature, kein Hebel für
Concurrency oder Durability. Wenn das Produktziel primär „mehrere parallele Leser/
Schreiber" wäre, läge Snapshots/Concurrency vorne — das ist aber heute nicht der
engste Engpass der Query-Fläche.

---

## §5 Empfehlung

1. **v1.3-Kandidat: Composite Indexes** (Index-Typ über `field_ids: Vec<u32>`,
   Prefix-Range-Lookup, Planner-Anpassung). Bestätigt als höchster Hebel für die
   bestehende Query-Fläche bei vertretbarem Aufwand/Risiko.
2. **Begleiter (demselben Zyklus oder direkt folgend): `group_by`** — profitiert von
   Composite-/sortiertem Index (Streaming statt Materialisierung). Wird aber erst im
   späteren Spec-Zyklus entschieden, nicht hier.
3. **Deferred:** Snapshots (B) und Read-only Tx (D) — erst nach Concurrency/MVCC
   sinnvoll; Remote Backup (C) jederzeit operativ, nicht Kern der Query-Fläche.

---

## §6 Nächster Schritt (kein Code)

Bei Freigabe: eigener **read-only Spec-Zyklus für v1.3 Composite Indexes**
(Index-Typ-Modell, Key-Encoding-Prefix-Regel, `core_put_entity`-Erweiterung,
`find_composite`, Planner-Auswahl, Oracle für Multi-Feld-Filter/`ORDER BY`/Präfix).
`c400bb0` bleibt bis dahin unangetastet.
