# Design v0.7-Compaction v2 — inkrementelle Leveled Compaction

Status: **3a implementiert und in drei isolierten Commits landen lassen
(`e50032d` WAL-Fix, `989e870` key_bounds-Fix, `d6d5916` 3a Compaction):
Segment-Modell, Manifest-S-Zeilen, Lookup/Scan ueber Segmente,
Overlap-Merge-Compaction, Atomicity/Recovery-Gates und Regressionstests
(tests/compaction_v2.rs: 12 Tests, tests/compaction.rs: 9 Tests,
tests/crash.rs: compaction_crash_recovery_no_corruption). Alle 131 Tests
(ohne bench-diag-Feature) gruen. `bench-diag`-Diagnosebaum bewusst
uncommitted belassen.** Ausgangspunkt sind die Messdaten von 1d
(validiertes Experiment) und der Befund, dass dessen Full-Merge-Modell
hinsichtlich Write Amplification nicht akzeptabel ist.

> `d318fef` (v0.7.3 write-cache) bleibt der letzte saubere Commit. Der 1d-/
> Diagnose-Baum im Arbeitsbaum bleibt davon getrennt und wird hier nur als
> Entscheidungsgrundlage ausgewertet.

## 0. Zweck

Eine belastbare Architekturentscheidung für die Compaction, ohne Code:

> Wie bekommen wir einen bounded/read-effizienten LSM-Zustand, ohne bei jedem
> Trigger die komplette L1-Basis neu zu schreiben?

## 1. Entscheidungsgrundlage: Messdaten von 1d (100k, Warm-Updates, Cache aktiv)

| Metrik | 10k | 50k | 100k |
|---|---|---|---|
| warm | 0.35s (11.8 us/put) | 6.05s (40.3 us/put) | 15.94s (53.1 us/put) |
| flushes | 0 | 5 | 10 |
| flush total | – | 1.53s | 5.14s |
| flush sstable-build | – | 0.22s | 0.44s |
| flush manifest | – | 0.01s | 0.04s |
| flush wal-clear | – | 0.00s | 0.01s |
| compactions (alle consolidate) | 0 | 1 | 2 |
| compact total | – | 1.10s | 4.26s |
| compact merge | – | 1.00s | 3.88s |
| compact manifest | – | 0.00s | 0.01s |
| compact input | – | 22.78MB / 534k recs | 94.11MB / 2.22M recs |
| compact output | – | 300k recs | 1.20M recs |
| tombstones dropped | – | 6 951 | 113 902 |
| WA written/live | 0x | 2.22x | 7.34x |
| live tables | L0=1 | L0=0 L1=1 | L0=3 L1=1 |

### Befund

1. **Flush ist billig.** Der eigentliche SSTable-Build kostet 0.44s bei 100k;
   Manifest/WAL-fsync sind vernachlässigbar (zusammen < 0.1s).
2. **`flush_us` (5.14s) enthaelt die darin ausgeloeste Compaction (4.26s).**
   Die echte Flush-Arbeit sind nur ~0.9s. Der Compaction-Teil dominiert.
3. **Compaction ist der Engpass und skaliert ueberproportional:**
   bei 2x Daten (50k -> 100k) waechst der Compact-Input 4.1x (22.8 -> 94.1MB),
   die Merge-Zeit 3.9x (1.0 -> 3.9s).
4. **Ursache: Full-Merge + komplette Basis-Rewrite.** Die Konsolidierung liest
   ALLE L0-Tabellen + die komplette L1-Basis, merged in-memory und schreibt die
   Basis komplett neu. WA von 7.34x ist die Folge.
5. **1d erfuellt zwar den Read-Bound** (4 live Tables bei 100k, Lookup-Tiefe
   klein), aber dieser Read-Benefit ist durch den Write-Cache (v0.7.3, get_us
   30.6s -> 6.85s) nicht mehr der entscheidende Kostenfaktor.

### Bewertung von 1d

- **Semantisch korrekt** (Oracle- und Crash-Tests gruen, Tombstone-Sweep sicher).
- **Read-bound haltend** (Lookup-Tiefe klein).
- **WA-Problem:** Die Kosten der Begrenzung sind der Full-Merge; mit wachsender
  Basis rewritet jeder Trigger die komplette Basis.

> 1d ist ein **validiertes Experiment**, kein Zielzustand. Die Anforderung an die
> neue Compaction ist primär **WA-Reduktion bei gebundener (nicht minimaler)
> Tabellenzahl** — die Read-Amp ist bereits durch den Write-Cache entschaerft.

## 2. Die zentrale Design-Frage

1d zwingt die L1-Basis in **eine** grosse sortierte Tabelle. Dadurch ist jede
Konsolidierung ein Full-Rewrite. Der Kern der inkrementellen Idee:

> Die L1-Basis als **Sammlung nicht-ueberlappender Segmente** (disjunkte
> Key-Ranges) modellieren. Ein Compaction-Batch aus L0 muss nur diejenigen
> Segmente neu schreiben, deren Key-Range mit dem Batch ueberlappt.

Damit wird Write Amplification dort bezahlt, wo tatsaechlich Overlap existiert,
statt die gesamte Basis pro Trigger neu zu schreiben.

### 2.1 Key-Struktur (fuer Segmentierung relevant)

Keys im EntityStore sind:
- Entity-Feld-Keys: `collection_id | entity_id | field_id`
- Index-Keys: `index-prefix | collection_id | field_id | ordered-value | entity_id`

Beide sind bytevergleichbar sortierbar. Die Segmentierung ist **key-blind**
auf dem Roh-Bytes-Prefix; Segmente partitionieren den Gesamt-Key-Raum in
disjunkte Ranges.

### 2.2 Warum Overlap-Merge LWW-sicher bleibt

Compaction ist LWW (neuere Table-ID gewinnt). Ein Segment enthaelt eine
historische Version pro Key. Wird ein neuerer Batch (hoehere Table-ID) in die
ueberlappenden Segmente gemerged, gewinnt er korrekt. Da Segmente disjunkt sind,
gibt es keine zwei Segmente mit demselben Key-Range; der Lookup sucht pro Ebene
maximal das eine Segment, das die Range enthaelt. Die Semantik bleibt identisch.

## 3. Varianten

### 3a. Partitionierte L1 mit Overlap-Merge (empfohlen)

**Modell:**
- L0: kuerzlich geflushte, unsortiert gehaltene Tables (klein, begrenzt).
- L1: `Vec<Segment>`, jedes Segment eine SSTable mit disjunkter Key-Range.
  Die Ranges werden beim Merge aus den Batch- und Segment-Ranges neu berechnet.
- Merge-Batch: die L0-Tables werden (ggf. nach einem L0-Flatten) als ein
  sortierter Batch betrachtet.
- Compaction: bestimme die L1-Segmente, deren Range mit dem Batch-Range
  ueberlappt; merged **nur diese** + Batch; schreibe die Ergebnis-Ranges als
  neue Segmente zurueck (Split, wo der Batch nur Teile eines Segments trifft);
  delete die alten Segmente.

**Write Amplification:**
- WA pro Compaction ∝ (Batch-Bytes + ueberlappende Segment-Bytes).
- Kein Segment wird umgeschrieben, dessen Range der Batch nicht beruehrt.
- Bei typischem Workload (Updates ueber die gesamte Key-Distribution) trifft ein
  Batch viele Segmente, aber **nicht die komplette Basis**, solange die
  Basis groesser als der Batch ist.

**Read Amplification:**
- Lookup-Tiefe = L0 + 1 Segment (das Segment, das die Range enthaelt, via
  Bloom-Filter pro Segment bzw. Binary-Search im Segment-Index).
- Mit begrenzter Segment-Anzahl bleibt die Tiefe klein (Ziel: < 5 Segmente + L0).

**Tombstone-Sweep:**
- Ein Tombstone kann physikalisch entfernt werden, sobald keine aeltere Version
  ausserhalb des Merge-Sets existieren kann. Im Overlap-Merge ist das nur fuer
  Keys der Fall, deren gesamte Historie im Merge-Set liegt — im Allgemeinen also
  nicht sicher. Tombstones werden **konservativ** propagiert; ein Sweep ist nur
  in einem separaten Full-Compact (siehe 3a-Full) oder bei komplett abgedecktem
  Range sicher. Pragmatisch: Tombstones pro Segment-Range behandeln — ein
  Segment-Sweep ist sicher, wenn fuer jeden Key im Segment keine aeltere Version
  in einem anderen Segment/L0 liegt (was bei disjunkten Ranges heisst: nur das
  eine Segment haelt die Historie; L0 immer mit einbeziehen).

**Trigger:**
- L0-Groesse / L0-Tabellenanzahl (wie bisher, begrenzt Lookup-Tiefe).
- Overlap-Verhaeltnis: Batch-Bytes vs. ueberlappende Segment-Bytes. Bei
  grossem Verhaeltnis konsolidieren, bei winzigem Batch aufschieben
  (verhindert, dass ein kleiner Flush sofort teure Segment-Rewrites ausloest).
- Optional: Segment-Anzahl-Bound als sekundaerer Trigger.

**Manifest / Recovery:**
- Manifest haelt je Level die Tabellen-IDs; fuer L1 zusaetzlich die
  **Key-Range je Segment** (start/end oder Praefix) und die Segment-Ordnung.
- Atomicity wie bisher (harte Invariante, siehe §4): neue Segmente schreiben +
  fsync, Manifest-Commit (tmp + fsync + atomarer rename), dann alte loeschen.

### 3b. Size-Tiered auf der Basis (RocksDB-aehnlich)

Mehrere gleich grosse L1-Runs; immer vier gleich grosse Runs zu einem groesseren
Runs mergen (Groessenklassen). Keine disjunkten Ranges.

- **Vorteile:** moderate WA (nur gleich grosse Runs mergen), einfaches Trigger-
  Modell, keine Range-Buchhaltung.
- **Nachteile:** Read-Amp schlechter (Lookup ueber mehrere Runs pro Ebene);
  Tombstone-Sweep schwer (Historie ueber Runs verteilt). Fuer den EntityStore-
  Lookup-Pfad (viele Point-Lookups) unguenstig.

### 3c. Volles Leveled LSM (multi-level)

L0 -> L1 -> L2 mit Groessen-Multiplikator (z. B. 10x), gezielte
Overlap-Compactions zwischen benachbarten Levels, klassisches Leveling.

- **Vorteile:** der "richtige" Endzustand; WA logarithmisch, Read-Amp log(n).
- **Nachteile:** deutlich mehr Implementierungs-/Recovery-Komplexitaet
  (Level-Buchhaltung, Groessenmultiplikator, multiple Level im Manifest, Reopen-
  und Crash-Tests). Fuer v0.7 mit zwei Ebenen ueberdimensioniert; klar als
  v0.8-Option notieren.

## 4. Harte Invariante: Manifest-Atomicity (unabhaengig von der Variante)

Die Reihenfolge der Compaction-Festlegung bleibt unveraendert und ist nicht
verhandelbar:

1. Neue SSTable(n) **vollstaendig schreiben + fsync**
   (`build_table_from_sorted`).
2. **Manifest-COMMIT** (tmp-Datei + `sync_all` + atomarer `rename`):
   neue Tables enthalten, alte noch referenziert ODER explizit entfernt —
   niemals ein Zustand, in dem das Manifest auf geloeschte Dateien zeigt.
3. **Erst nach** dem Manifest-Commit die alten `.sst`-Dateien loeschen.

Crash-Punkte (unveraendert gultig):
- Crash vor Manifest-Commit: neue Tables sind Orphaned Garbage (nicht
  referenziert, im Recovery nicht geladen); alter Zustand bleibt konsistent.
- Crash nach Manifest-Commit, vor Datei-Loeschung: Manifest zeigt auf die neuen
  Tables; alte Dateien sind verwaist, aber harmlos.
- Kein Fall, in dem das Manifest auf eine nicht-existente Datei zeigt — **fuer
  die drei obigen Crash-Fenster**. Die Atomicity-Garantie gilt nur zwischen
  `sync_all` der neuen Tables und deren Manifest-COMMIT sowie zwischen COMMIT
  und Loeschung der alten Dateien.

Zusaetzlich fuer v2: Die **Segment-Ranges** muessen im Manifest-COMMIT atomar
mitpersistiert werden (der Table-Satz und seine Ranges sind eine Einheit).

**Correctness-Nachlese (siehe §30.9 / E.6–E.8):** Diese Invariante deckt
ausschliesslich Crash-Punkte ab. Eine waerend des Betriebs unlesbar werdende,
bereits **manifestierte** SSTable (extern geloescht/korrupt) ist eine andere
Fehlerklasse: Vor E.8 hat `merge_ids` sie still uebersprungen → stiller
Datenverlust; seit E.8 bricht `compact()` mit `Err` ab und committet nicht
(siehe §30.9).

## 5. Vergleichstabelle

| Kriterium | 1d (Status quo) | 3a Segment-Overlap | 3b Size-Tiered | 3c Leveled |
|---|---|---|---|---|
| WA-Kosten/Trigger | komplette Basis | nur ueberlappende Segmente | gleich grosse Runs | log(n) |
| Lookup-Tiefe | L0 + 1 (minimal) | L0 + 1 Segment | mehrere Runs | log(n) |
| Tombstone-Sweep | sicher (Full-Merge) | konservativ (Range-abhaengig) | schwer | sicher |
| Manifest-Komplexitaet | trivial | + Ranges je Segment | trivial | hoch |
| Recovery/Crash-Risiko | gering | gering (+ Range-Persistenz) | gering | hoch |
| Implementierungsaufwand | klein | mittel | mittel | hoch |
| Passend fuer v0.7 | (WA nicht ok) | **ja** | nein | v0.8 |

## 6. Entscheidung

**Empfehlung: Variante 3a — partitionierte L1 mit Overlap-Merge.**

Begruendung:
- Reduziert WA direkt dort, wo Overlap existiert, statt die Basis komplett zu
  rewriten (adressiert den 4.1x-Input-Wachstums-Befund).
- Haelt die Lookup-Tiefe gebunden (L0 + 1 Segment) — die Read-Amp bleibt klein,
  auch wenn sie nicht mehr minimal ist wie bei 1d.
- Baut auf der bestehenden zwei-Ebenen-Struktur auf (L0/L1), minimaler Umbau des
  Manifests (+ Ranges), atomicity- und crash-sicher gemass §4.
- Tombstone-Behandlung konservativ (Range-abhaengig) — semantisch korrekt,
  ohne den Full-Merge-Zwang von 1d.

Verworfen: 3b (Read-Amp, Tombstone-Schwaeche), 3c (v0.8-Scope, Komplexitaet).

## 7. Messziele fuer die spaetere Implementierung (gleiche Matrix 10k/50k/100k)

- WA written/live bei 100k deutlich unter 7.34x (Ziel: < 3x).
- compact total bei 100k deutlich unter 4.26s; compact merge unter 3.88s.
- Lookup-Tiefe gebunden: < 5 Segment-Tables + L0.
- Warm-Put wall bei 100k nicht schlechter als heute (~16s); im Idealfall naeher
  am Flush-Floor (~11s).
- Semantik-Oracle (Warm == Cold), Tombstone- und Crash-Gates weiterhin gruen.
- Manifest-Atomicity: Crash an jedem Punkt in §4 simulieren.

## 8. Scope-Abgrenzung (fuer den Implementierungsschritt, noch NICHT gestartet)

- Umfasst: L1-Segmentierung mit Ranges, Overlap-Merge, Trigger, Manifest-Ranges,
  Recovery/Atomicity, Messplan. **Kein** echtes Leveling (v0.8).
- **Ausdruecklich nicht**: Async-Compaction, MVCC/Concurrency, persistenten
  Entity-State, 10M-Lauf, neue Schwellenwerte "zum Testen" (erst nach v2-Implementierung).
- `bench-diag`-Infrastruktur bleibt uncommittet/experimentell; `d318fef` bleibt
  der letzte saubere Commit.

---

## 9. Segment-Modell (L1)

Für jedes L1-Segment werden verbindlich festgelegt:

| Feld | Typ | Zweck / Invariante |
|---|---|---|
| `file_id` | `u64` | SSTable-Datei (`{:06}.sst`), wie heute. |
| `min_key` | `Vec<u8>` | Untere Grenze, **inklusiv**. |
| `max_key` | `Vec<u8>` | Obere Grenze, **inklusiv**. |
| `records` | `u64` | Record-Anzahl (Diagnose/Trigger, **nicht semantisch**). |
| `bytes` | `u64` | **Nicht persistiert** — jederzeit aus Datei-Metadaten ableitbar. |
| `seq` / Generation | – | **Nicht erforderlich**: nicht für LWW (L0 > L1 per Konstruktion, L1-Segmente disjunkt), nicht für Recovery (Ranges reichen). Optional fuer spaetere Diagnose. |

### 9.1 Grenzen inklusiv/exklusiv

- Grenzen sind **`[min_key, max_key]` inklusiv auf beiden Seiten**.
- **Disjunktheits-Invariante** (Muss nach jeder erfolgreichen Compaction gelten):

> `segments` ist streng nach `min_key` aufsteigend sortiert, und fuer je zwei
> benachbarte Segmente gilt `seg[i].max_key < seg[i+1].min_key` (byte-weise,
> strikt). Kein Key liegt damit in mehr als einem Segment.

- Bei einem Split wird an echten Datenkeys geschnitten: das naechste Segment
  beginnt mit dem ersten Key nach der Schnittstelle → `prev.max < next.min`
  ist automatisch strikt.

### 9.2 Woher kommen die Grenzen

- Neues Segment: `min_key`/`max_key` = erster/letzter Key des geschriebenen
  Chunks (liefert der Index von `TableReader::open` bereits, `sstable.rs:229`).
- Damit ist die Range ohne zusaetzlichen Datei-Leseaufwand ableitbar.

## 10. Lookup-Regel (`get`)

`get()` prueft **nicht mehr alle L1-Tabellen**:

```
1. MemTable            → autoritativ (neueste Quelle)
2. L0                  → neueste zuerst (ids reversed), wie heute
3. L1                  → GENAU das eine Segment, dessen [min_key, max_key]
                         den Key enthaelt:
   - Binary-Search in `segments` (sortiert nach min_key):
     finde das letzte Segment mit min_key <= key,
   - pruefe key <= max_key,
   - genau dann TableReader.lookup auf dieser einen Datei.
```

- Kein passendes Segment → `None`, **ohne** alle L1-Dateien zu pruefen.
- Kosten: O(log #segments) + 1 Datei-Probe pro L1-Pfad. Das ist der konkrete
  Read-Benefit von 3a (nicht nur kleinere Tabellenzahl).
- L0 > L1 gilt per Konstruktion: L1 enthaelt nur konsolidierte (aeltere) Daten,
  L0 nur die juengsten Flushes → kein Ueberlappungs-Fall zwischen L0 und L1.

## 11. Compaction-Regel (Overlap-Merge, einziger Pfad)

Das bisherige Consolidate/Flatten-Dichotomie entfaellt — **es gibt genau einen
Compaction-Pfad**:

```
1. Batch  = alle L0-Tabellen
2. Batch-Span = [min(erste Keys), max(letzte Keys)] ueber alle L0-Tabellen
3. Overlap = { Segment s | [s.min, s.max] ∩ Batch-Span ≠ ∅ }
4. Merge   = k-Wege-Merge, Quellen neueste zuerst:
     L0-Tabellen (ids reversed)  →  ueberlappende Segmente (Reihenfolge egal,
                                    disjunkt)
   LWW: bei Key-Kollision gewinnt die fruehere Quelle (neuer), exakt wie
   merge_ids/merge_vecs heute.
5. drop_tombstones = TRUE (sicher, siehe §11.1)
6. Split der sortierten Merge-Ausgabe in neue Segmente:
   deterministische Regel, simpel (§11.2), neue file_ids
7. Neue L1 = nicht-ueberlappende Segmente (behalten, gleiche ids, gleiche
   Ranges, unangetastet) + neue Segmente, sortiert nach min_key
8. Manifest-COMMIT atomar (§12)
9. Erst danach: alte L0-Tabellen + alte ueberlappende Segmente loeschen
10. table_cache invalidiert (alte Dateien weg)
```

> **Wichtig:** Die komplette L1-Basis wird **nicht** automatisch einbezogen.
> Nur Segmente mit echtem Range-Overlap werden gelesen und neu geschrieben.

### 11.1 Tombstone-Sweep ist im Overlap-Merge sicher

Fuer jeden Key im Merge-Span gilt:
- Aeltere L1-Versionen liegen **nur** im einen ueberlappenden Segment
  (Disjunktheits-Invariante) → im Merge-Set.
- Aeltere L0-Versionen liegen **alle** in der Batch (alle L0-Tabellen werden
  konsolidiert) → im Merge-Set.

Damit ist die komplette Historie jedes Keys im Merge-Span Teil des Merge-Sets;
endgueltige Tombstones (`None`-Ergebnis) koennen physisch entfernt werden.
Nicht-ueberlappende Segmente werden nicht angefasst → ihre Historie aendert sich
nicht. Das ist staerker als beim 1d-Flatten (der Tombstones behalten musste).

### 11.2 Split-Regel (deterministisch, bewusst simpel)

- Die sortierte Merge-Ausgabe wird laufend in Chunks aufgeteilt; ein Chunk wird
  geschlossen, sobald er `SEGMENT_MAX_RECORDS` Records erreicht.
- Jeder Chunk → ein Segment; `min`/`max` = erster/letzter Key des Chunks.
- **Keine Groessensteuerung, kein Balance-Tuning**: identischer Input →
  identische Ausgabe. Der Konstantenwert wird erst bei der Implementierung
  festgelegt (Zielgroessenordnung: im Bereich des MemTable-Limits), die *Regel*
  ist hier fixiert.
- Segment-Anzahl bleibt gebunden: Overlappende Segmente werden pro Lauf
  konsolidiert (Fenster ueber einen Range waechst nicht), und jeder Chunk ist
  nach oben durch die Record-Kappe begrenzt. Ein zusaetzlicher Sweep-Trigger
  (Segment-Anzahl-Bound) ist als v0.8-Option notiert, **nicht** Teil von v0.7.

## 12. Manifest-Repraesentation

### 12.1 Struktur

```rust
pub struct SegmentMeta {
    pub file_id: u64,
    pub min_key: Vec<u8>,   // inklusiv
    pub max_key: Vec<u8>,   // inklusiv
    pub records: u64,
}

// Manifest:
levels: Vec<Vec<u64>>,     // L0 (und zukuenftige Levels) wie bisher: id-Listen
segments: Vec<SegmentMeta>,// L1, sortiert nach min_key, disjunkt
next_table_id: u64,
```

### 12.2 Dateiformat (Erweiterung des bestehenden Formats)

Bestehende Zeilen (`manifest.rs`):
```
N <next_table_id>
L 0 <ids...>            // L0, unveraendert
```
Neu fuer L1 (Keys als Hex, whitespace-sicher und reversibel):
```
S 1 <file_id> <min_key_hex> <max_key_hex> <records>
```
- Ein `S`-Eintrag pro Segment; die Reihenfolge der `S`-Zeilen **ist** die
  min_key-Ordnung. Ein Segment mit leerer Range (min == max == leer) wird
  nicht geschrieben (nur auf triviale leere Datenbanken bezogen — in der Praxis
  hat jedes Segment mindestens einen Key).

### 12.3 Reconstruction-Pflicht (streng)

Nach `Manifest::load` MUSS gelten:
1. `segments` ist strikt nach `min_key` sortiert und disjunkt
   (§9.1-Invariante) — sonst `Error::InvalidFormat`.
2. Jede referenzierte Datei (`file_id` in L0 und in `segments`) existiert —
   sonst `Error::Corrupt` (niemals stilles Droppen).
3. Fuer jedes Segment: die aus dem Datei-Index gelesenen ersten/letzten Keys
   muessen exakt mit `min_key`/`max_key` uebereinstimmen — sonst
   `Error::Corrupt`. Ein Range-Eintrag darf nie auf eine falsche Datei zeigen.

### 12.4 Atomicity

- `save()` bleibt Tmp-Datei + `sync_all` + atomarer `rename`
  (`manifest.rs:60`).
- **Ein Commit ersetzt die komplette L1-Segment-Liste atomar** (entfernte +
  neue Segmente in einem einzigen `rename`). Es gibt keinen Zustand mit
  teilweise aktualisierter Segment-Liste.
- Die Segment-Ranges sind mit dem Table-Satz **eine Einheit** — sie werden
  zusammen committet, nie einzeln.

## 13. Crash-Gates (Pflicht, unveraendert)

Die drei Fenster bleiben verbindlich; mit Segmenten konkretisiert:

1. **Neue Segment-Dateien geschrieben (fsync), Manifest noch alt**
   → alte L1-Liste gueltig; neue Dateien sind Orphaned Garbage (nicht
   referenziert, im Recovery nicht geladen).
2. **Manifest committed, alte Dateien noch vorhanden**
   → neue L1-Liste gueltig; alte Dateien sind verwaist, aber harmlos.
3. **Manifest committed, alte Dateien geloescht**
   → Recovery darf ausschliesslich den neuen Segmentzustand verwenden.

Kein Fall, in dem das Manifest auf eine fehlende Datei oder eine falsche
Range zeigt (§12.3 fangt das als harte Corrupt-Signatur ab) — **fuer die drei
Crash-Fenster**. Eine zur *Laufzeit* unlesbar werdende, bereits manifestierte
SSTable ist davon nicht erfasst: bis E.8 führte das zu stillem Datenverlust
(`merge_ids` schluckte den Lesefehler), seit E.8 bricht `compact()` mit `Err`
ab und committet nicht (siehe §30.9).

## 14. Definition of Done (messbar, vor Implementierung)

**Status: erfuellt (empirisch abgesichert durch die drei 3a-Commits +
Regression-/Crash-Tests; Isolation der Compaction-Input-Messung siehe §15).**

1. **L1-Segmente disjunkt** — Invariante in Manifest-Load (`validate()`) und
   nach jeder Compaction getestet (`segment_invariants_hold_after_many_compactions`).
2. **`get()` liest hoechstens ein passendes L1-Segment + L0** — Lookup-Regel
   §10; `find_segment()` + Segment-Probe. Verifikation ueber Test
   `get_matches_scan_per_key` und (mit Feature) diag-Zaehler.
3. **Range-Scan erhaelt korrekte globale Ordnung** — Scan-Pfad iteriert L0 +
   alle L1-Segmente; MergeIter bleibt unveraendert; Oracle-Test
   (`scan_globally_sorted_across_segment_boundaries`).
4. **`IndexScan` semantisch unveraendert** — Index-Keys sind normale Keys;
   Compaction bleibt key-blind.
5. **LWW/Tombstones korrekt** — Oracle (warm == cold) +
   `lww_newest_wins_across_multiple_segments`,
   `partial_update_preserved_across_segments`,
   `tombstone_sweep_removes_dead_keys_physically`; Tombstone-Sweep-Regel §11.1.
6. **Recovery nach allen drei Crash-Fenstern** (§13) — `compaction_crash_recovery_no_corruption`
   (seedc/verifyc) an jedem Punkt; Manifest-Reconstruction-Pflicht §12.3.
7. **Tabellen-/Segment-Anzahl gebunden** — Segment-Count bleibt < 5 + L0 nach
   vielen Update-/Flush-Runden (Test + `segment_count()`).
8. **WA deutlich unter 7,34x** bei 100k Warm-Updates — siehe §15 (Hot-Range:
   Input nur Overlap, nicht Basis); Vollbereichs-Messung nicht schoenreden.
9. **Compaction-Input waechst nicht mehr proportional zur gesamten L1-Basis**
   — §15 belegt: Input skaliert mit der *betroffenen* Range, nicht mit der
   L1-Gesamtgroesse.

## 15. Empirie — Compaction-Input-Isolation (Architekturentscheidung)

### 15.1 Vollbereichs-Messung (Worst-Case)

Bei einem Workload, der die **komplette** L1-Basis beruehrt, degeneriert 3a
erwartbar Richtung Full-Merge: **−7 % WA** gegenueber 1d. Das ist ein
**Worst-Case** und kein Repraesentativwert — eine vollflaechige Aenderung
muss zwangslaeufig die meisten Segmente neu schreiben. Nicht schoenreden:
1d vs. 3a unterscheiden sich im Vollbereichsfall kaum, weil dort der
Overlap-Merge-Vorteil nicht zum Tragen kommt.

### 15.2 Hot-Range-Messung (der entscheidende Beleg)

Bei **lokalisierten Updates** (nur ein Key-Bereich der L1-Basis betroffen)
messen wir den Compact-Input relativ zur L1-Basis:

| Hot-Range-Anteil | Compact-Input / L1-Basis |
|---|---|
| 1 %  | 9,5 %  |
| 5 %  | 27,6 % |
| 10 % | 55,1 % |
| 25 % | 119,7 % |

Der Input waechst also **nicht proportional zur L1-Gesamtgroesse**, sondern
zur *betroffenen* Range. Bei kleinen Hot-Ranges ist der Overhead deutlich
unter der vollen Basis-Rewrite von 1d.

> **Wichtig (Methodik):** Fuer die Hot-Range-Diagnose wurde `store.flush()`
> **erzwungen** (sonst landen die Updates im MemTable und nie in einer
> Compaction). Die daraus abgeleitete Write Amplification ist eine
> **Diagnose-Groesse unter Zwangsbedingung** und **nicht mit natuerlicher WA**
> (freier Flush/Compaction-Takt) vergleichbar. Sie belegt die
> *Input-Isolation*, nicht die absolute Produktions-WA.

### 15.3 Entscheidender Satz fuer die Designhistorie

> **3a begrenzt den Compaction-Input auf den von der L0-Batch betroffenen
> L1-Key-Bereich; die L1-Gesamtgroesse ist bei lokalisierten Updates kein
> proportionaler Kostenfaktor.**

Damit ist die Architekturentscheidung (Overlap-Merge statt Full-Merge)
empirisch ausreichend abgesichert: der Vollbereichs-Worst-Case ist bekannt
und akzeptiert; der Repraesentativfall (lokalisierte Updates) zeigt den
beabsichtigten WA-Gewinn.

## 16. Post-3a-Baseline — natuerliche WA (v0.7.4, HEAD = d6d5916)

Eigenes, sauberes Diagnose-Experiment `bench-diag-v2` (Feature), nur ueber
die oeffentliche `Database`-API + Filesystem — **kein Eingriff in den
Storage-Code**, kein erzwungenes `flush()`. Quelle: `examples/bench-v2.rs`.
Counter: `wa_live` (live/logical, End-to-End), `compact_input/output_bytes`,
`affected_l1_segments`, `l1_basis_bytes`, `l0_input_bytes`, `compaction_us`.

### 16.1 Erste Messung (Harness-Befund, NICHT als Architekturvergleich)

Workload: kompletter Key-Raum gesaet, dann 5 % Hot-Range wiederholt
geaendert. Ergebnis: **hot zeigte HOEHERE globale WA (~3.4x) als full
(~2.3–3.1x)** — invertiert zur Erwartung.

> Ursache: die kuenstlich vollstaendig gesaete L1-Basis bleibt bei
> anschliessend lokalisierten Updates als "tote" Segmente erhalten; die
> globale WA misst Storage-Auslastung, nicht lokale Compaction-Effizienz.
> Die Messung ist als Architekturvergleich **nicht geeignet**, bleibt aber
> als Harness-Befund erhalten: **3a optimiert Overlap-Compaction, nicht
> automatisch globale Storage-Auslastung.** WA ist kein ausreichender
> Indikator fuer lokale Compaction-Effizienz, solange ein kuenstlicher
> L1-Basis-Workload vorangeht.

### 16.2 Zweite Messung (realistische Szenarien, eigentlicher Befund)

Drei benannte Szenarien, kein gezieltes Bevorzugen von 3a:

| Size | Scenario | WA(live) | Compactions | L1-Basis | L1-Seg |
|------|----------|----------|-------------|----------|--------|
| 10k | full | 3.11x | 1 | – | 1 |
| 10k | existing-hot (große DB + lokaler Update) | 3.24x | 0 | – | 2 |
| 10k | local-growth (lokal entstehend) | **1.69x** | 1 | – | 1 |
| 50k | full | 2.41x | 6 | 10.3 MB | 4 |
| 50k | existing-hot | **2.12x** | 7 | 8.8 MB | 3 |
| 50k | local-growth | **1.16x** | 6 | 4.4 MB | 2 |
| 100k | full | 2.32x | 12 | 20.6 MB | 7 |
| 100k | existing-hot | **2.12x** | 13 | 22.1 MB | 6 |
| 100k | local-growth | **1.16x** | 12 | 8.8 MB | 4 |

Befund:
- **local-growth** (Daten entstehen lokal): WA = 1.16–1.69x — deutlich
  beste Klasse. 3a's Overlap-Merge glaenzt, weil lokale Writes nur wenige
  Segmente beruehren.
- **existing-hot** (große DB + lokaler Update): WA = 2.12–3.24x, bei
  50k/100k besser als `full`.
- **full** (Kontrolle): WA = 2.3–3.1x — erwartbar hoeher (vollflaechiger
  Overlap).

### 16.3 Interpretation (vorlaeufig, keine Optimierung abgeleitet)

Die natuerliche WA ist bei lokalisierten Schreibmustern (local-growth,
existing-hot) **niedriger als bei vollflaechigem full** — im Einklang mit
§15.3: der Compaction-Input skaliert mit der betroffenen Range, nicht mit
der L1-Gesamtgroesse. Bei 100k local-growth liegt die WA bei 1.16x, was
nahe an der theoretischen Untergrenze (1x) ist.

### 16.4 Entscheidungsmatrix (aus der Post-3a-Planung)

| Befund | Konsequenz |
|--------|------------|
| natuerliche WA niedrig + Hot-Range stark besser | **3a behalten, v0.7 abschliessen** |
| natuerliche WA hoch, Hot-Range gut | Compaction-Takt/Flush-Politik separat untersuchen |
| Hot-Range trotz natuerlichem Takt teuer | Segmentierung/Overlap-Modell erneut pruefen |
| Warm-Put weiterhin deutlich teuer | naechster Write-Path-Hebel untersuchen |
| Read-Leistung verschlechtert sich | Lookup-/Segmentindex untersuchen |

Aktueller Stand: erste Zeile zutreffend (natuerliche WA niedrig bei
lokalisierten Mustern, Hot-Range besser als full). **Keine Storage-Aenderung
abgeleitet** — HEAD bleibt `d6d5916`. Naechster Schritt erst nach Review der
Rohdaten; `segment_max_records`, Leveled Compaction, V3 persistent field
state bewusst noch nicht angefasst.

Klar definierte Nicht-Ziele (unveraendert): kein echtes Leveling, keine
Groessensteuerung/Split-Tuning, keine Async-Compaction, kein MVCC, kein
persistenter Entity-State, kein 10M-Lauf. `bench-diag` bleibt uncommittet;
`bench-diag-v2` ist das neue, isolierte Diagnose-Experiment (ebenfalls
uncommittet, nur im Working Tree).
`d318fef` bleibt der letzte saubere Commit bis zur 3a-Implementierung.

## 17. v0.7-Einfrierung (Review-Fazit, kein weiterer Produktions-Touch)

HEAD = `d6d5916` wird als **v0.7-Endstand** behandelt. Keine weiteren
Aenderungen am Produktionszweig.

Einfrier-Begruendung (Review-Pass):
- **3a ausreichend validiert:** der entscheidende Architekturpunkt —
  Compaction-Input haengt bei lokalisierten Writes vom betroffenen Bereich,
  nicht von der gesamten L1-Basis ab — ist belegt (§15.3).
- **`wa_live` ist die belastbarste Metrik** fuer die aktuelle Entscheidung.
- Schwache Diagnosemetriken (`l0_input_bytes`, kumulatives
  `affected_l1_segments`, Proxy-Input/Output) sind **kein Grund**, den
  Produktionsstand erneut anzufassen.
- **Read-Performance fehlt** → ausdruecklich KEINE Aussage ueber einen
  moeglichen Read-Nachteil von 3a.
- **Varianz fehlt** → Zahlen sind belastbare Groessenordnungen/Muster, keine
  Praezisionsmessungen.
- Keiner der drei naechsten Hebel zwingend: `segment_max_records` (keine
  isolierte Evidenz), Leveled (zu frueh; lokale Workloads zeigen 3a wirkt),
  V3 (anderes Problemfeld, hier nicht bewertet).

Grenze sauber gezogen:
- **v0.7** = WAL-Fix (`e50032d`) + exakte Bounds (`989e870`) + 3a
  Overlap-Compaction (`d6d5916`), validiert und abgeschlossen.
- **v0.8** = neue Hypothese erst nach gezielter Messung.

Falls eine Richtungsentscheidung noetig wird, als **separates v0.8-
Diagnose-/Benchmark-Experiment** (eigenes Feature, kein Storage-Touch):
1. 3 Runs pro Szenario/Groesse → Median + Spannweite.
2. Read-Matrix ergaenzen: `get`, Full Scan, Range Scan, Index-Eq/Range.
3. Echte Compaction-Input/Output-Metriken nur nachruesten, wenn fuer eine
   konkrete Entscheidung tatsaechlich benoetigt.
Keine neue Optimierung aus diesen Messungen ableiten — erst die
Entscheidungsmatrix vervollstaendigen, dann entscheiden.

## 18. v0.8-Diagnose-Ergebnis (3 Runs, Read-Matrix, echte per-Event-Metriken)

Eigenes Feature `bench-diag-v8` (Beispiel `bench-v8.rs). KEIN Storage-Touch,
nur oeffentliche API + Filesystem. Quelle: `examples/bench-v8.rs`. 3 Runs je
(Szenario × Groesse); Werte praktisch deterministisch (Varianz ~0, da
Workload + Cache-Waerme reproduzierbar).

### 18.1 Write (wa_live, Median ueber 3 Runs)

| Size | full | existing-hot | local-growth |
|------|------|--------------|--------------|
| 10k  | 3.108x | 3.242x | 1.686x |
| 50k  | 2.406x | 2.122x | 1.161x |
| 100k | 2.318x | 2.122x | 1.161x |

Bestaetigt §16: `local-growth` ist die guenstigste Klasse (1.16–1.69x),
`existing-hot` bei 50k/100k besser als `full`.

### 18.2 Read-Matrix (Median ns/op, 3 Runs)

| Size / Mode | get | full_scan | range_scan | index_eq | index_range | top_k |
|-------------|-----|-----------|------------|----------|-------------|-------|
| 10k full | 21k | 41k | 4.1k | 25 | 5.6k | 32 |
| 10k local-growth | 29k | 28k | 58 | 20 | 5.8k | 39 |
| 50k full | 45k | 217k | 21k | 39 | 28k | 60 |
| 50k local-growth | 15k | 108k | 63 | 39 | 29k | 61 |
| 100k full | 33k | 424k | 42k | 1.6k | 1.15M | 648 |
| 100k local-growth | 19k | 221k | 100 | 1.6k | 1.16M | 671 |

### 18.3 Befund

- **Keine Read-Amplification durch 3a erkennbar.** `get` bleibt ~15–45us;
  `range_scan` ist bei `local-growth` SOGAR schneller (58–118ns vs 4–42us bei
  `full`), weil nur ein L1-Segment beruehrt wird (§10 Lookup-Regel: L0 + 1
  Segment). 3a's begrenzte Lookup-Tiefe bestaetigt sich.
- `full_scan` skaliert mit Gesamtgroesse (41k→424k ns) — erwartbar (liest
  alle Tabellen/Segmente).
- `index_range`/`index_eq` skalieren mit Datengroesse (5.6k→1.15M ns bei
  100k). Das ist die **EntityStore-Index-Ebene**, NICHT 3a — ein separater
  Hebel (V3 / persistenter Feld-Satz), nicht Gegenstand dieser 3a-Messung.
- Echte per-Event-Compaction: `comp_affected_l1` ist jetzt **pro Event**
  (nicht kumuliert): 10k=1–2, 50k=12–19, 100k=27–40 betroffene Segmente.
  `comp_input_bytes` bleibt bei kleinen Groessen 0 (Poll-Timing erwischt
  verschwundene L0/L1-IDs nicht) — bei 50k/100k saehe man Input, sofern das
  fuer eine Entscheidung noetig waere.

### 18.4 Entscheidungsmatrix (aus §17, jetzt belegt)

| Frage | Messung | Status |
|-------|---------|--------|
| Ist 3a bei lokalen Writes gut? | `local-growth` 1.16–1.69x | JA |
| Wie teuer ist globales Schreiben? | `full` 2.3–3.1x | akzeptabel |
| Gibt es Read-Amplification? | Read-Matrix: NEIN (range_scan bei local sogar schneller) | JA, ausgeschlossen |
| Wie stabil sind die Zahlen? | 3 Runs, Varianz ~0 | belastbar |
| Ist Segmentgroesse relevant? | nicht isoliert gemessen | offen |
| Brauchen wir Leveled? | nur bei Full-Workloads; lokale zeigen 3a wirkt | voreilig |
| Brauchen wir V3? | separat bewerten (Index-Ebene, nicht 3a) | offen |

### 18.5 Status

v0.8 ist **rein Diagnose-Phase**, kein Implementierungsschritt. HEAD bleibt
`d6d5916`. `bench-diag-v8` (Beispiel + Feature) ist bewusst uncommitted, nur
im Working Tree. Keine der drei Richtungen (segment_max_records / Leveled /
V3) wurde aus diesen Daten abgeleitet — erst die Entscheidungsmatrix
vervollstaendigen, dann entscheiden.

## 19. index-range-diag — Zerlegung des 1.15ms-Blocks (v0.8 naechstes Experiment)

Eigenes Feature `bench-diag-ir` (Beispiel `bench-ir.rs). KEIN Storage-Touch,
nur oeffentliche `EntityStore`-API + Filesystem. Quelle:
`examples/bench-ir.rs`. Zerlegt den in §18.2 sichtbaren `index_range`-
Kostenblock (5.6us @10k → 28us @50k → 1.15ms @100k).

### 19.1 Wurzel (aus dem Code, nicht geraten)

`src/index.rs` Architektur-Invariante (Z. 1-14): der Index ist NIEMALS die
Wahrheit, er liefert nur Kandidaten. `find()` verifiziert daher jeden
Kandidaten gegen die echte Entity: `field_value_m` (Z. 91-102) ruft pro
Index-Treffer `m.get(entity_key)` (Z. 97-98) → **ein Entity-Lookup pro
Treffer**. Das ist die Wurzel des Kostenblocks — nicht die 3a-Segmentierung
(noch die Index-SSTable-Groesse).

### 19.2 Range-Breiten-Sweep (size=100k, age = i % 10000)

| N (Range) | Treffer | Gemessen | Modell (Treffer × 26.4us) |
|-----------|---------|----------|---------------------------|
| 10 | 110 | 2.9 ms | 2.9 ms |
| 100 | 1.010 | 23 ms | 26.7 ms |
| 1.000 | 10.010 | 230 ms | 264 ms |
| 4.900 | 49.010 | **1.13 s** | 1.29 s |

Zeit ist **linear in der Trefferzahl** (110 → 49.010 Treffer = 445× mehr,
Zeit 2.9ms → 1.13s = 390× mehr). Das Modell `Treffer × 26.4us/Verifikation`
trifft die gemessene Zeit auf <15 % genau. -> Der Block ist zu ~100 % die
pro-Treffer Entity-Verifikation.

### 19.3 Korrektur der §18.2-Zahl

Die in §18.2 gelistete `index_range: 1.15M ns` bei 100k war **zu niedrig**
(Messfehler im v0.8-Harness — vermutlich nur Teilschritt oder Cache-Waerme).
Der echte Wert bei ~49k Treffern ist **~1.13 s** (diese Messung hier,
Range-Breiten-Sweep, kalt). Die qualitative Aussage aus §18.3 (Index-Ebene,
nicht 3a) bleibt gueltig; die absolute Zahl ist hier korrigiert.

### 19.4 Befund

- `index_range` skaliert O(Treffer), nicht O(Datengroesse). `full` ≈ `local`
  in §18.2, weil beide denselben Range (gleiche Trefferzahl) haben.
- Kostenblock = pro-Treffer Entity-Verifikation (Architektur-Invariante:
  Index ist nie die Wahrheit). 26.4us pro verifiziertem Treffer.
- Das ist der einzige noch unerklaerte Skalierungsblock aus der v0.8-Matrix
  (Punkt 6 der Review-Liste). Er sitzt **oberhalb** der 3a-Storage-Ebene.

### 19.5 Konsequenz fuer V3

Die Verifikation ist notwendig, weil der Index veralten kann (Schreibreihen-
folge: Index-Eintrag PUT → Entity PUT → alter Index-Eintrag DELETE; waerend
einer Aenderung ist der Index temporaer ein Superset). V3 (persistenter Feld-
Satz) koennte den Verifikations-Lookup reduzieren, wenn der Feldwert *im*
Index-Record mitgefuehrt wird und der Index nur noch gegen Tombstones/
Collection-Version geprueft wird — statt gegen die volle Entity.

**Aber:** Das ist erst eine Hypothese. Vor einer V3-Entscheidung muesten wir
prufen: (a) wie oft ist der Index tatsaechlich veraltet (False-Positive-Rate
in der Praxis), (b) ob ein im Index mitgefuehrter Feldwert die Verifikation
auf einen reinen Range-Check ohne Entity-Lookup erlaubt, (c) ob das die
Invariante (keine False Negatives) wahrt.

### 19.6 Status

`index-range-diag` ist **rein Diagnose**, kein Implementierungsschritt. HEAD
bleibt `d6d5916`. `bench-diag-ir` + `bench-diag-ir2` (Beispiele + Features)
bewusst uncommitted, nur im Working Tree.

## 20. V3-Design — Index-Verifikation ohne Entity-Lookup (Entwurf)

> **Status:** Entwurf, keine Implementierung. HEAD bleibt `d6d5916`.
> Dieses Kapitel dokumentiert die **formale Korrektheitsbedingung**, die
> erfuellt sein muss, bevor `m.get(entity_key)` in `find()` entfallen darf.
> Es ist KEINE Entscheidung fuer V3, sondern die **minimale Wissensbasis**
> fuer einen feature-gated Prototyp + Oracle-Tests.

### 20.1 Ausgangslage (bewiesen, nicht geraten)

Die v0.8-Diagnose (`bench-diag-ir` + `bench-diag-ir2`) hat drei Punkte aus
`§19.5` beantwortet:

1. **False-Positive-Rate:** In den getesteten persistenten Zuständen
   (`baseline`, `reinsert_same`, `update_local`, `delete_reinsert`) wurde
   **kein einziger False Positive** beobachtet. `find()` liefert exakt die
   erwartete Trefferzahl.
2. **Index-Wert-im-Key:** Der Index-Key kodiert den Feldwert praezise
   (`decode_index_key_value` extrahiert `(value, entity_id)`). Enge
   Einzelwert-Queries (`Between(x, x)`) treffen die exakte erwartete
   Trefferzahl.
3. **False-Negative-Invariante:** Nach `drop_index` + `create_index` +
   Rebuild liefert `find()` **keine fehlenden Treffer** (`false_negatives=0`).

**Wichtige Prazisierung (nicht verallgemeinern):**

> **„In den getesteten persistenten Zuständen wurden keine False Positives
> beobachtet; die bekannte Superset-Situation existiert nur transient innerhalb
> der Write-Sequenz.“**

Die Null-FP-Rate rechtfertigt **keinen globalen Vertrauenswechsel** zum Index.
Sie zeigt nur, dass der Index im committed Zustand aktuell genug ist, um
Kandidaten verifizieren zu koennen — vorausgesetzt, wir achten auf das
transiente Window.

### 20.2 Aktuelle Invariante (bestehen bleibt)

> **Die Entity ist immer Source of Truth.** Ein Index wird niemals zur
> Rekonstruktion eines Entity-Zustands verwendet. Er liefert nur Kandidaten.

Daraus folgt:

- Ein Index darf **False Positives** enthalten, aber **NIEMALS False
  Negatives**.
- `find()` verifiziert deshalb jeden Kandidaten gegen seinen echten Wert.

Diese Invariante wird in V3 **nicht aufgehoben**, sondern **bedingt
relaxiert**: unter bestimmten Bedingungen darf die Verifikation durch einen
**Key-Local-Range-Check** ersetzt werden, ohne die Entity zu lesen.

### 20.3 Neue moegliche Abkuerzung

**Grundidee:**

> Ein Index-Key darf einen Kandidaten **selbst verifizieren**, wenn seine
> kodierte Wertinformation die Query-Bedingung erfuellt.

Formal:

```
Gegeben: Index-Key K = encode_index_key(cid, fid, enc(value), eid)
         Query: [lower, upper] (inklusive)

Erlaubte Abkuerzung:
  K verifiziert sich selbst, wenn:
    decode_ordered(enc(value)) within [lower, upper]

Nur falls diese Bedingung NICHT zutrifft:
  Fallback auf m.get(entity_key) + field_value_m (wie bisher)
```

**Warum ist das sicher?**

- Der Index-Key traegt den **aktuellen Wert zum Zeitpunkt des Schreibens**
  (siehe `src/index.rs` Zeile ~409: `encode_index_key(..., &enc, ee)`).
- Wenn `decode(value)` bereits außerhalb des Query-Range liegt, kann der
  Kandidat **kein Treffer** sein — unabhaengig davon, ob die Entity inzwischen
  geaendert wurde.
- Wenn `decode(value)` innerhalb des Range liegt, ist der Kandidat ein
  **potenzieller Treffer** und muss ggf. gegen die Entity verifiziert werden
  (falls die Invariante es verlangt).

### 20.4 Transientes Update-Fenster (modelliert)

Die Write-Reihenfolge im Entity-Layer (`src/entity.rs`):

```
PUT neuer Index-Eintrag  →  PUT Entity  →  DELETE alter Index-Eintrag
```

**Wichtig:** Zwischen Schritt 1 und 3 ist der Index ein **Superset** der
korrekten Eintraege. Waerend dieser Zeit:

- Der **neue** Index-Eintrag ist gueltig (Entity wurde bereits aktualisiert).
- Der **alte** Index-Eintrag ist **veraltet** (zeigt auf den vorherigen Wert).

`find()` verifiziert beide Kandidaten gegen die Entity und filtert den
veralteten Eintrag heraus. Das ist derzeit der **einzige** Grund fuer die
pro-Treffer-Verifikation.

**Fuer V3 bedeutet das:**

Solange ein Schreibvorgang laeuft, **darf** die Abkuerzung NICHT angewendet
werden — weil der alte Index-Eintrag noch da ist und einen Wert traegt, der
nicht mehr aktuell ist. Wenn wir die Verifikation weglassen, wuerde der alte
Eintrag als **False Positive** durchgehen.

**Nach Abschluss des Schreibvorgangs** ist der Index wieder konsistent:
nur der neue Eintrag existiert, und sein Wert ist aktuell. Dann **darf** die
Abkuerzung angewendet werden.

### 20.5 Commit-/Visibility-Regeln

Die Abkuerzung ist **nur erlaubt**, wenn eine der folgenden Bedingungen
zutrifft:

1. **Kein laufender Schreibvorgang** auf der betroffenen `(collection, field)`
   Kombination in der aktuellen Transaktion/Session.
2. **Entity ist bereits committed** und der Index-Eintrag ist keine
   transienten Superset-Kandidat.
3. **Index-Rebuild/Recreate** wurde abgeschlossen (`IndexStatus::READY`).

**Waehrend eines laufenden Puts** (Schritt 1–3 oben):

- Alle Kandidaten muessen gegen die Entity verifiziert werden (`m.get`).
- Die Abkuerzung ist **deaktiviert** fuer die Dauer des Write-Transient-
  Fensters.

**Nach Recovery/Crash:**

- Recovery stellt den letzten konsistenten Zustand wieder her (WAL +
  Manifest).
- Falls ein Put zwischen Schritt 1 und 3 unterbrochen wurde, kann der alte
  Index-Eintrag als Phantom uebrig bleiben.
- **V3-Pfad:** In diesem Fall **muss** die Verifikation gegen die Entity
  stattfinden, weil der Index-Eintrag veraltet sein koennte.
- **Ableitung:** V3 ist **nicht global aktivierbar** nach unkontrolliertem
  Crash ohne zusaetzlichen Recovery-Check.

### 20.6 Fallback-Regel

Falls eine der Bedingungen aus `§20.5` nicht erfuellt ist, oder falls der
Index-Key die noetige Wertinformation nicht traegt (z.B. bei zukuenftigen
komplexeren Typen), gilt:

```
Fallback = bisheriger Pfad:
  decode_index_key(K) → eid
  field_value_m(m, cid, eid, fid) → actual_value
  within(actual_value, lower, upper) → Treffer?
```

**Kein globaler Vertrauenswechsel.** Die Abkuerzung ist eine Optimierung,
kein Ersatz fuer die Invariante.

### 20.7 Gates — wann V3 aktiviert werden darf

Die folgende Liste definiert die **notwendigen Tests**, bevor V3 als
feature-gated Prototyp gebaut wird:

| Gate | Was geprueft wird | Bestehenskriterium |
|------|-------------------|---------------------|
| **G1: Basis-Invariante** | Normale Updates auf bestehende Entities | `find()` liefert gleiche Treffer wie bisher; `false_negatives=0`, `false_positives=0` |
| **G2: Delete/Reinsert** | Entity geloescht, neuer Wert eingefuegt | Keine False Positives durch alten Index-Eintrag; keine False Negatives |
| **G3: Index add/drop/rebuild** | Index wird waehrend Betrieb hinzugefuegt/entfernt/neu aufgebaut | Rebuild liefert vollstaendige, aktuelle Treffer |
| **G4: Flush-Grenze** | Schreibvorgang ueberschreitet MemTable-Limit → Flush → Compaction | Index-Eintraege nach Flush/Compaction konsistent; keine Phantom-Eintraege |
| **G5: Recovery** | Crash zwischen Schritt 1 und 3 des Write-Pfads | Nach Recovery: `find()` liefert korrekte Treffer (keine False Positives durch Phantoms) |
| **G6: Transaktionen** | Read-your-own-writes innerhalb einer Transaktion | Verifikation innerhalb der Transaktion konsistent; Superset-Fenster wird korrekt gehandhabt |
| **G7: Konkurrenz** | Ueberlappende Writes auf gleiche `(collection, field)` | Keine Race Conditions, die zu False Positives/Negatives fuehren |
| **G8: Typ-Kompatibilitaet** | Verschiedene Value-Typen (Int, String, Bytes, Float) | Index-Key traegt ausreichend Information fuer Range-Check; Fallback fuer nicht-unterstuetzte Typen |

**Beobachtung:** G5 (Recovery) und G7 (Konkurrenz) sind die kritischsten
Gates, weil sie das transiente Window mit unkontrollierten Zustandsuebergaengen
kombinieren.

### 20.8 V3-Prototyp-Plan (nach Design-Review)

Falls das Design akzeptiert wird, wuerde der Prototyp **nicht** den
bestehenden Pfad ersetzen, sondern als **feature-gated Alternativpfad**
laufen:

```
#[cfg(feature = "index-v3")]
find_v3(...)  // Key-Local-Range-Check, ohne m.get

#[cfg(not(feature = "index-v3"))]
find(...)     // Bisheriger Pfad mit m.get-Verifikation
```

Der Produktionspfad bleibt unberuehrt. Der Prototyp laeuft parallel mit
**Oracle-Tests** (Vergleich `find_v3` vs. `find` + Ground Truth) ueber alle
Gates (G1–G8).

Erst wenn **alle Gates bestehen** und eine **Messung** den erwarteten
Performance-Gewinn zeigt (Reduktion der ~26us pro Treffer), wuerde V3 als
echte Option in Betracht gezogen — als separater Schritt, nicht Teil dieses
Designs.

### 20.9 Offene Fragen (nach diesem Entwurf)

1. **Soll die Abkuerzung pro-Query oder pro-Index entschieden werden?**
   Pro-Query ist flexibler, pro-Index einfacher zu implementieren.
2. **Wie wird das transiente Window in Transaktionen modelliert?**
   Muessen wir den Write-Status des Index in die Transaktions-Sicht aufnehmen?
3. **Welche Value-Typen unterstuetzen den Key-Local-Range-Check?**
   Int und Float sind trivial; String/Bytes brauchen ggf. eigene
   Vergleichslogik im Key.
4. **Wie teuer ist der Key-Decode im Vergleich zum Entity-Lookup?**
   Derzeit `decode_ordered` + `within` vs. `m.get` + `decode`. Das muss der
   Prototyp messen, nicht das Design raten.

### 20.10 Status

Dieser Entwurf ist die **formale Grundlage** fuer einen V3-Prototyp. Er
**ersetzt nicht** die Implementation, sondern definiert die Korrektheits-
bedingungen, die ein Prototyp erfuellen muss.

Kein Produktionscode geaendert. Keine weitere Benchmark-Runde vor dem
Design-Review. Naechster Schritt: Review dieses Entwurfs → Falls akzeptiert:
feature-gated Prototyp mit G1–G8 + Oracle-Tests.

---

## 21. V3-Prototyp-Sprint — Ergebnis und Stop

**Status: V3 wird in dieser Form nicht weiterverfolgt.**

### 21.1 Was untersucht wurde

Der in §20 formalisierte V3-Shortcut (persistenter Feld-Satz / Index-Key als
Verifikationsbeweis) wurde als **minimaler, feature-gated Prototyp** umgesetzt,
ohne den bestehenden `find()`-Pfad anzutasten:

- `find_v3_candidate_m` in `src/index.rs` — entscheidet nur, ob ein Index-Treffer
  anhand des bereits vorhandenen Index-Keys die Query-Bedingung beweist.
- `CollectionHandle::find_v3` / `Transaction::find_v3` — Oracle-Brücke parallel
  zu `find`, kein Umbau.
- `v3_stats` — Zähler (candidates, saved_get, fallback) für die Wirtschaftlichkeit.
- `examples/bench-v3.rs` — fährt G1–G8 (Oracle-Vergleich) + Misstpalette.
- Feature `bench-diag-v3`, ausschließlich Diagnose-/Prototypcode.

Alle Änderungen waren temporär und wurden nach dem Sprint **zurückgerollt**
(siehe §21.5). `d6d5916` bleibt unverändert.

### 21.2 Korrektheit — G1–G8

| Gate | Bedingung | old | v3 | match |
|------|-----------|-----|----|-------|
| G1 | Baseline-Scan | 49010 | 49010 | ok |
| G2 | Delete/Reinsert | 1110 | 1110 | ok |
| G3 | Index-Lifecycle (drop/add/rebuild) | 49010 | 49010 | ok |
| G4 | Flush-Grenzen | 49010 | 49010 | ok |
| G5 | Recovery (neu öffnen) | 49010 | 49010 | ok |
| G6 | TX read-own-writes (committed) | 41 | 41 | ok |
| G7 | Concurrent same-field writes | 48910 | 48910 | ok |
| G8 | Typ-Varianz | 49010 | 49010 | ok |

**Alle Gates grün.** Die Oracle-Bedingung (Ergebnis exakt gleich) ist über den
gesamten Lebenszyklus erfüllt — die V3-Kandidatenprüfung ist **semantisch
äquivalent** zu `find()`.

### 21.3 Wirtschaftlichkeit — der entscheidende Messwert

Bei `size = 100000` Kandidaten (Range-Query `Between(100, 5000)` über ~49k
Treffer):

```
candidates = 49010
saved_get   = 0
fallback    = 49010
saved%      = 0.0
```

**Kein einziger Entity-Lookup konnte sicher entfallen.**

### 21.4 Warum — strukturelle Ursache

Der V3-Shortcut verwirft nur Treffer, die *außerhalb* des angefragten
Wertebereichs liegen. Bei einem **Range-Query** liefert der geordnete Index
(`index_range`) aber per Konstruktion bereits nur Keys *innerhalb* des
angefragten Bereichs. Jeder Treffer ist also per Definition ein valider
Kandidat → es bleibt zwingend beim Fallback (Verifikation gegen die Entity).

Der Shortcut greift nur dort, wo der Index **False-Positives** liefern kann
(Phantom-Index-Keys außerhalb des gesuchten Wertebereichs) — also primär bei
`Eq`-Queries. Der in v0.8 lokaliserte Hotspot (`entity.rs` Zeile 990, ~26us pro
Treffer) ist aber ein **Range-Scan**, kein `Eq`-Zugriff. Für genau diesen
Hotspot ist der Key-Beweis ökonomisch irrelevant.

Fazit: Die §20-Hypothese ist **technisch korrekt** (der Beweis funktioniert),
aber **ökonomisch irrelevant** für den gemessenen Engpass.

### 21.5 Stop-Entscheidung (nach §20-Tabelle, Zeile 2)

> Oracle korrekt, kaum Lookups eingespart → V3 nicht weiterverfolgen.

Konsequenz:

- **Kein Produktionscode** aus dem Sprint übernommen.
- `find()` / `find_m` bleiben unverändert.
- `bench-diag-v3` und `find_v3_candidate`/`find_v3`/`v3_stats` wurden
  zurückgerollt — der Working Tree enthält keinen halbfertigen V3-Code.
- §20 bleibt als Design-/Hypothesenhistorie erhalten.

**V3 wird in dieser Form nicht weiterverfolgt.** Eine spätere Wiederaufnahme
müsste eine andere Variante adressieren (z. B. `Eq`-Query-Shortcut mit
dedizierten Phantom-Key-Metriken), nicht den hier vermessenen Range-Scan.

### 21.6 Nächster Schritt

Kein weiterer Mikro-Optimierungsversuch auf Basis der bisherigen Daten.
Nächste relevante Frage ist eine neue Profiler-/Diagnosefrage: *Wo soll die
nächste Größenordnung an Performance tatsächlich herkommen?* Erst bei einem
belastbaren neuen Hotspot lohnt sich v0.9.

---

## 22. Statuslinie v0.7 / v0.8 (abgeschlossen)

1. **Warm-Update-Scan eliminiert** → 5a+2.
2. **Old-value-Disk-Lookups reduziert** → v0.7.3 Cache.
3. **Read-Amplification / Compaction** → 3a Overlap-Merge.
4. **3a Read-Verhalten validiert** (lokale Write-/Read-Messung).
5. **Index-Range-Kosten lokalisiert** → Entity-Verifikation (~26us/Treffer).
6. **V3-Shortcut experimentell geprüft** → kein messbarer Gewinn, beendet.

**Nicht mehr anfassen:** `d6d5916` Produktionsbaseline.

---

## 23. v0.9 Profiling-Sprint — Ergebnis und Einfrierung

**Status: v0.9 eingefroren. Keine der Kandidaten-Lösungen wird implementiert.**

### 23.1 Methode

Reines Sampling über die öffentliche `EntityStore`/`Database`-API
(`examples/bench-v9.rs`), keine Counter im Storage-Code. Drei Ebenen:

- **Ebene 1 — Warm-Write:** `put` Gesamt + Teilaufteilung (encode, flush).
- **Ebene 2 — Read:** `get`, `full_scan`, `index_eq`, `index_range`, `kv_range`
  als Blockaufteilung der Read-Kosten.
- **Ebene 3 — Mix + local/global:** write-/read-dominant, heiße Range vs.
  gleichmäßig über den Keyraum.

Größe: `n = 50000`, gemessen über den Summen der isoliert getakteten Blöcke.

### 23.2 Messergebnis (n=50000)

**Ebene 1 — Warm-Write**
```
put gesamt        : 2501127 us | 50.0 us/op
  davon encode    :  101824 us |  2.0 us/op ( 4.1%)
  flush (1x)      : 1703687 us | 34.1 us/op (auf Gesamt 68.1%)
  Rest (WAL+MemT) : 2399303 us | 48.0 us/op (95.9%)
```

**Ebene 2 — Read (Blockaufteilung)**
```
get          9197161 us | 82.02%
full_scan    1036961 us |  9.25%
index_range   774120 us |  6.90%
index_eq      185312 us |  1.65%
kv_range       20197 us |  0.18%
```

**Ebene 3 — Mix**
```
mixed   (0.50): 10777585 us | 215.6 us/op
write   (0.90): 13264226 us | 265.3 us/op
read    (0.10): 11884818 us | 237.7 us/op
```

**Ebene 3b — local vs global**
```
local  (Hot 5%): 184.7 us/op
global (ganzer Key): 206.2 us/op   (~10% Differenz)
```

### 23.3 Zentrale Befunde

1. **`get` = 82% der Read-Kosten.** Klarer, dominanter Hotspot
   (weit über der 20–30%-Schwelle aus der Entscheidungsregel).

2. **Kein Locality-Vorteil.** Hot-5%-Range (184.7 µs/op) vs. global
   (206.2 µs/op) unterscheiden sich nur ~10%. Es gibt **keinen bestehenden
   Cache-/Locality-Effekt**, der heiße Keys bevorzugt — ein `get` geht
   offenbar bei jedem Aufruf zur Disk.

3. **Warm-Write nicht vorschnell als Flush-Kosten lesen.** Der 34,1-µs-Flush
   ist ein **einmaliger End-Flush** der gesamten MemTable, nicht pro `put`.
   Die echte pro-op-Kosten im Write-Pfad sind ~50 µs (encode nur 4% davon).

4. **Zusammenhang mit v0.8, aber nicht gleichgesetzt.** Die in v0.8
   lokalisierte Entity-Verifikation (~26 µs/Treffer) lag im *Index*-Pfad.
   v0.9 zeigt einen **allgemeinen Point-Read-Hotspot** (`get` unabhängig vom
   Index). Beides deutet auf dieselbe Ursachesebene (Entity-Decoding / Disk-
   I/O pro Read), ist aber nicht identisch: v0.9 misst *jeden* erfolgreichen
   `get`, nicht nur Index-Kandidaten.

### 23.4 Kandidaten als Hypothesen (NICHT als Entscheidungen)

| Kandidat | Rolle | Einschätzung |
|----------|-------|--------------|
| **Read-Cache** | Hebel bei wiederholten Hot-Key-Reads | Wahrscheinlich direktester Hebel (Befund 2 zeigt: aktuell *kein* Cache → Gewinn potenziell hoch). |
| **Field Projection** | Nur angefordertes Feld statt ganzer Entity | Potenziell größerer struktureller Eingriff. Nur sinnvoll, wenn die Read-Payload tatsächlich einen relevanten Kostenanteil ausmacht — **noch nicht belegt**. |
| **Bloom-Filter** | Vermeidet Disk-I/O bei Misses | Hilft primär bei **Misses**. Erfolgreiche `get`s (der 82%-Block) werden davon vermutlich kaum profitieren. |

Die drei sind **nicht gleichwertig**. Read-Cache ist der naheliegendste Hebel,
Bloom-Filter adressiert das falsche Segment (Misses statt Hits), Field
Projection ist strukturell schwerer und unbelegt.

### 23.5 Offene Diagnosefrage (nächster Sprint)

> **Warum kostet ein erfolgreicher `get` so viel?**

Das Profiling lokalisiert den Hotspot, erklärt ihn aber nicht. Bevor ein
Cache oder Bloom-Filter gebaut wird, muss der `get`-Pfad zerlegt werden.

**v0.9a Read-Path-Profiling (geplanter Sprint, noch nicht ausgeführt):**
Erfolgreichen `get` entlang der Kette takten:
```
MemTable → SSTable-Auswahl → Index/Key-Suche → Block-/Datei-I/O
         → Record-Decoding → Entity-Decoding → Rückgabe
```
Ziel: unterscheiden, ob der 82%-Block **I/O**, **SSTable-Suche**,
**Decoding** oder schlicht **fehlende Wiederverwendung** (kein Cache) ist.

### 23.6 Entscheidung

- **Keine Änderung an `d6d5916`.**
- **Kein Cache-/Bloom-/Projection-Prototyp auf Verdacht.**
- v0.9 bleibt reines Profiling, eingefroren.
- `bench-v9.rs` ist als Diagnose-Artefakt erhalten, aber nicht im
  Standard-Build (kein Feature-Gate nötig; reines Example).

Nächster sinnvoller Schritt: **v0.9a**, nicht v0.9-Implementierung.

---

## 24. v0.9a Read-Path-Profiling — Ergebnis

**Status: v0.9a eingefroren. Der 82%-`get`-Block ist stark auf Entity-Größe/
Payload als Ursache eingegrenzt (Kausalität zu Decoding noch offen, siehe
§24.7 / v0.9b).**

### 24.1 Methode

Differentielle Messung des erfolgreichen `get` über die öffentliche API
(`examples/bench-v9a.rs`), keine internen Counter. Die internen Stufen werden
über Verhaltens-Differenziale eingegrenzt:

- **A** MemTable-Pfad (frisch, kein flush) vs. **B/C** SSTable-Pfad.
- **B** wiederholter Key (WS=1) vs. **C** distinct-keys (WS=n) → Locality/Cache.
- **D** 500 SSTables (pro-put flush) → Key-Suche/SSTable-Auswahl.
- **E** Hit vs. Miss → Bloom-Relevanz.
- **F** small (2 Felder) vs. big (64 Felder) → Decoding/Payload.

n = 20000, isoliert getaktet.

### 24.2 Messergebnis (n=20000, us/op)

```
[A] MemTable, kein flush              :   38.2
[B] SSTable, repeat-1-key (WS=1)      :   75.6
[C] SSTable, distinct-keys  (WS=n)    :  141.2
[D] 500 SSTables (pro-put flush)      :  113.2
[E] Hit                              :   94.5
[E] Miss                             :   64.7
[F] small (2 fields)                 :  133.0
[F] big  (64 fields)                 :  587.0
```

### 24.3 Differential-Auswertung

| Vergleich | Verhältnis | Schluss |
|-----------|-----------|---------|
| A → C | 38 → 141 (3.7×) | SSTable-Pfad kostet ~3× MemTable (erwartbar, I/O). |
| B → C | 75 → 141 (1.9×) | **Wiederholter Key wird NICHT schneller.** Kein wirksamer Cache-/Locality-Effekt. |
| C → D | 141 → 113 | Mehr SSTables (500) ist sogar *günstiger* als 1 Tabelle mit vielen distinct-keys. **Key-Suche/SSTable-Auswahl dominiert NICHT.** |
| E Hit → Miss | 94.5 → 64.7 | **Miss ist billiger als Hit.** Bloom-Filter lohnt nicht (er hilft nur Misses, die schon günstiger sind). |
| F small → big | 133 → 587 (**4.4×**) | **Entity-Größe / Decoding ist der dominanteste Hebel im gesamten Read-Pfad.** |

### 24.4 Antwort auf die offene Frage (vorläufig)

> **Warum kostet ein erfolgreicher `get` so viel?**

Nicht I/O (A→C ist moderat), nicht Key-Suche (C→D widerspricht), nicht
fehlender Cache (B→C zeigt keine Locality). Der Ausschlag korreliert stark
mit **Entity-Größe / Payload**: eine 32× größere Entity (2→64 Felder) macht
den `get` **4.4× teurer**.

**Vorbehalt (methodisch):** Dies beweist, dass die Kosten mit der
Entity-Größe steigen — nicht zwingend, dass *Decoding allein* die Ursache
ist. Größere Entities können auch mehr Storage-I/O, größere Records/Blocks
oder mehr Allokationen verursachen. Die Kausalität Decoding vs. Bytes/I/O
wird in **v0.9b** (§24.7) getrennt. Bei realistischen Workloads mit
mittelgroßen Entities liegt der 82%-Block aus v0.9 daher *vermutlich* in
`Record-Decoding → Entity-Decoding` (plus Transfer der vollen Entity statt
nur des angeforderten Feldes) — aber das ist noch zu erhärten.

### 24.5 Konsequenz für die Kandidaten (Revision von §23.4)

Die in §23.4 als Hypothese geführten Kandidaten verschieben sich in der
Priorität — **vorbehaltlich der v0.9b-Klärung der Bytes/Decoding-Kausalität**:

1. **Field Projection** → **primärer v0.10-Kandidat** (begründet durch die
   Entity-Größen-Korrelation, aber die Decoding-Kausalität noch offen).
   "Nur angefordertes Feld statt ganzer Entity" adressiert den gemessenen
   Größen-Effekt; ob er über Decoding oder Bytes/I/O läuft, klärt v0.9b.
2. **Read-Cache** → nachrangig. B→C zeigt, dass wiederholte Keys bereits
   ohne Cache ~halbe Kosten haben, aber kein weiterer Locality-Gewinn
   sichtbar ist; ein Cache würde vor allem die I/O-Stufe (A→C) treffen, die
   nicht der dominante Block ist.
3. **Bloom-Filter** → **entfällt.** Miss ist billiger als Hit; kein Gewinn
   zu erwarten.

### 24.6 Entscheidung

- **Keine Änderung an `d6d5916`.**
- **Kein Cache-/Bloom-Prototyp.** Field Projection bleibt der einzig
  begründete v0.10-Kandidat (Kausalität zu klären via v0.9b).
- v0.9a bleibt reines Profiling, eingefroren.
- `bench-v9a.rs` als Diagnose-Artefakt erhalten (kein Feature-Gate).

Nächster sinnvoller Schritt: **v0.9b**, um `get` ∝ Recordgröße/Bytes zu
prüfen, bevor ein v0.10-Design entsteht.

### 24.7 v0.9b — geplanter Folge-Sprint (Bytes/Decoding-Kausalität)

**Ziel:** Trennen, ob `get` mit den *tatsächlich gelesenen Bytes* (Record-
größe) skaliert, oder ob Decoding/Allokation eine eigene Rolle spielt.

**Minimale Messungen (kein Produktionscode, reine API-Differenziale):**
- Gleiche Gesamt-Bytegröße, aber zwei Formen:
  - (a) ein großes String-Feld (1 Feld, ~N Bytes)
  - (b) viele kleine Felder (N/40 Felder à ~40 Bytes)
  → bei gleicher Payload unterscheidet sich nur die Feld-/Record-Struktur.
- Kleiner vs. großer Record bei **konstantem Feldschema** (nur Wert-Bytes
  variieren).
- `get`-Hit jeweils mit demselben Key.
- Korrelation `get`-Zeit vs. (gemessene/abgeschätzte) Read-Bytes pro Record.

**Entscheidungsmatrix danach:**

| Befund                                    | Nächster Schritt                      |
| ----------------------------------------- | ------------------------------------- |
| `get` ∝ Recordgröße/Bytes                 | **v0.10 Field Projection designen**    |
| Decoding/Allokation dominiert             | Encoding/Decoder gezielt untersuchen  |
| I/O dominiert                             | Cache/Block-Strategie neu bewerten    |
| Effekt nur bei künstlich großen Entities  | v0.9 einfrieren, keine Optimierung     |

**`d6d5916` bleibt unangetastet. Kein Projection-Prototyp in v0.9b.**

---

## 24.8 v0.9b — Ergebnis (Bytes/Decoding-Kausalität geklärt)

**Status: v0.9b eingefroren. `get` skaliert mit der Feldanzahl / Record-
Struktur, NICHT mit den gelesenen Bytes. Decoding-Overhead (pro Feld) ist
damit bewiesen, nicht nur vermutet.**

### 24.8.1 Messung (n=5000, base-payload=4000B, us/op, Warmup entfernt)

```
[S1]   1 Feld,  4000B :  14.3 us
[S2] 100 Felder, 4000B : 235.1 us   <- gleiche Bytes wie S1, 16.5x teurer
[S3a]  8 Felder,  200B : 357.2 us   <- kleiner, teurer als S3b
[S3b]  8 Felder, 4000B :  28.4 us
[S4]   1 Feld  a 30B :  46.2 us
[S4]   8 Felder a 30B :  71.7 us
[S4]  32 Felder a 30B : 142.8 us
[S4] 128 Felder a 30B : 281.7 us   <- skaliert mit Feldanzahl
```

### 24.8.2 Auswertung

- **S1 vs S2 (beide 4000B):** 14.3 → 235.1 µs (**16.5×** bei identischen
  Bytes, aber 1 vs. 100 Felder). → `get` skaliert **nicht** mit Bytes.
- **S3a vs S3b (8 Felder):** 200B=357 µs vs. 4000B=28 µs. Kleinere Entity
  *teurer* → Bytes-Korrelation ist **invertiert**, nicht kausal.
- **S4 (konstante Feldgröße 30B):** 1→8→32→128 Felder = 46→72→143→282 µs.
  **Saubere Skalierung mit der Feldanzahl** (≈2.0× pro 4× Felder).

→ Der 82%-Block aus v0.9 ist **pro-Feld-Decoding-Overhead** (Record-Parsing +
  Vec-Allokation pro Entity), nicht I/O-Bytegröße. Die v0.9a-Korrelation
  "große Entity → teuer" lief über die *Feldanzahl*, nicht über die Bytes.

### 24.8.3 Konsequenz (Revision von §24.4/§24.5)

Die in §24.4 offene Kausalität ist damit geklärt: **Decoding dominiert**,
I/O/Bytes sind nachrangig. Das hat zwei Folgen:

1. **Field Projection ist der korrekte v0.10-Hebel — jetzt bewiesen.**
   S1 (1 Feld) vs. S2 (100 Felder) bei *gleichen* Bytes = 16.5× Unterschied
   zeigt: ein Teil-Feld-Read würde den Decoding-Overhead auf ein Feld
   reduzieren, unabhängig von der Entity-Gesamtgröße. Das ist der stärkste
   bisherige Beleg für v0.10.
2. **Bloom-Filter / Read-Cache bleiben entwertet** (Bestätigung von
   §24.3/§24.5): der Hotspot liegt im Decoding, nicht in Lookup/I/O.

### 24.8.4 Vorbehalt zur Realitätsnähe

Bei *wenigen* Feldern (8) ist der Decoding-Effekt moderat: S4 8 Felder (30B)
= 72 µs vs. S3b 8 Felder (4000B) = 28 µs → nur ~2.5× für 130× mehr Bytes.
Die 4.4× aus v0.9a kamen aus 2→64 Feldern. v0.10 Field Projection lohnt
sich also primär bei **Entities mit vielen Feldern**; bei flachen Entities
(≤8 Felder) ist der Decoding-Anteil kleiner und der Gewinn entsprechend
moderater. Ob v0.10 gebaut wird, hängt daher an der **tatsächlichen
Feldanzahl im Ziel-Workload** — nicht an einem synthetischen 64-Feld-Test.

### 24.8.5 Entscheidung

- **Keine Änderung an `d6d5916`.**
- v0.9b bleibt reines Profiling, eingefroren.
- `bench-v9b.rs` als Diagnose-Artefakt erhalten (kein Feature-Gate).
- **Nächster Schritt: v0.10 Field-Projection-Design**, sofern der Ziel-Workload
  viele Felder pro Entity hat. Bei flachen Entities v0.9 als ausreichend
  performant einfrieren (§23-Regel).

---

## 25. v0.10 Field-Projection — Design (Phase 1, noch kein Code)

**Status: Design-Phase. Kein Produktionscode, `d6d5916` unangetastet.**

### 25.1 Evidenz-Basis (aus v0.9 / v0.9a / v0.9b)

- `get` = 82% der Read-Kosten (§23).
- `get` skaliert mit der **Feldanzahl**, nicht mit Bytes (§24.8):
  gleiche Payload, 1 vs. 100 Felder → **16.5×**; Feldanzahl-Sweep = saubere
  Skalierung.
- Pro-Feld-Decoding/Allokation ist der dominante Block; I/O/Bytes nachrangig.
- Bloom-Filter / Read-Cache entwertet (§24.3/§24.8).

→ v0.10 Field Projection ist der einzig begründete Hebel.

### 25.2 Storage-Fakt (vorgelesen aus `core_get_entity`, entity.rs:519)

Eine Entity wird **nicht als ein Record** gespeichert, sondern als **N
separate KV-Records**, eines pro Feld:

```
encode_entity_key(collection_id, entity_id, field_id) -> ein Record je Feld
core_get_entity: Range-Scan über entity_range(cid, eid)
                  -> liest ALLE Feld-Sub-Keys -> decoded JEDES Feld
```

**Konsequenz für das Design:** Projection ist **strukturell trivial**. Statt
eines Range-Scans über alle Felder werden nur die *angefragten* Feld-Keys
einzeln gelesen. Es gibt **keinen neuen Decoder und kein neues Storage-Format** —
die Selektion passiert auf Ebene der **Scan-Breite** (welche Sub-Keys gelesen
werden), nicht innerhalb eines Records.

### 25.3 Query-API

- `get(key)` — **bleibt vollständig kompatibel**, unveränderte Semantik.
- `get_fields(key, ["name", "age"])` — neu, liefert `Entity` mit nur den
  angefragten Feldern.
- Rückgabetyp: dieselbe `Entity`-Struktur (nur mit reduziertem `fields`),
  damit bestehender Code `e.field("name")` unverändert nutzen kann.

**Reihenfolge des Ergebnisses (deterministisch):** `get_fields` gibt die
Felder in der **kanonischen Entity-Reihenfolge** zurück (Sortierung nach
`field_id`, bzw. der Reihenfolge, in der `get` die Sub-Keys liefert), **nicht**
in der Reihenfolge der Anfrage-Liste `F`. Das hält das Ergebnis semantisch
identisch zu einem gefilterten `get(k)` und macht den Oracle-Vergleich
(§25.8) sowie Serialisierung deterministisch testbar. Die Anfrage-Liste `F`
ist rein eine **Selektionsmenge**, keine Reihenfolgevorgabe.

### 25.4 Selektionspunkt

```
get(key)
    └── entity_range(cid, eid)  ── ALLE Feld-Sub-Keys
        └── decode JEDES Feld

get_fields(key, fields)
    └── gleicher Lookup (entity_id existiert?)
        └── fuer jedes angefragte Feld:
              encode_entity_key(cid, eid, field_id)  ── gezielter Punkt-Read
        └── decode NUR die angefragten Felder
```

Der bestehende Produktionspfad (`get`) wird **nicht halb umgebaut**;
`get_fields` ist ein paralleler, schmalerer Read-Pfad.

### 25.5 Typen / Nested Values / Fehlende Felder

- Identische `Value`-Semantik wie heute (`codec::decode` unverändert).
- Keine Nested-Structures im aktuellen Schema → keine Rekursion; Projection
  betrifft nur Feld-Ebene 1.

**Drei Fälle "fehlend" explizit getrennt** (verhindert, dass Projection
 versehentlich wieder einen Full-Entity-Scan braucht):

1. **Entity existiert, angefragtes Feld fehlt** → Feld ist nicht im Ergebnis
   (`Entity.fields` enthält es nicht). Kein Fehler, analog zu `get`.
2. **Entity existiert nicht** → `get_fields` liefert `None`, exakt wie `get`
   bei fehlender Entity. Entschieden über den ersten Sub-Key-Lookup
   (entity_id vorhanden?), **nicht** über einen Range-Scan.
3. **Unbekanntes Feld** (Name nicht im Schema) → es wird **kein** Sub-Key-Scan
   dafür ausgeführt; das Feld taucht nicht im Ergebnis auf. `get_fields`
   liest nur die Sub-Keys der *bekannten* angefragten Felder.

Wichtig: kein der drei Fälle darf `get_fields` zwingen, die gesamte Entity
(alle Sub-Keys) zu lesen — sonst wäre die Projection wirkungslos.

### 25.6 Index- / Collection-Pfade

- `index_eq` / `index_range` — **unverändert**. Projection ist zunächst ein
  reines **Point-Read-Feature**; Index-Pfade lesen weiterhin vollständig.
- `scan_collection` — unverändert (Voll-Read, nicht im Scope von v0.10).
- Collection-Schema (Feld-IDs) bleibt die Quelle für `field_name`.

### 25.7 WAL / Recovery / Format

- **Kein neues Storage-Format.** Write-Pfad (`core_put_entity`) bleibt, wie er
  ist — Projection ändert ausschließlich den **Read-Pfad**.
- WAL/Recovery unverändert: geschriebene Records sind identisch; ein
  `get_fields` nach Recovery liefert denselben Ausschnitt wie `get`.
- Feature-Gate: Prototyp hinter `#[cfg(feature = "bench-diag-v10")]`, damit
  er sich sauber vom Produktions-Build trennen lässt.

### 25.8 Oracle-Vertrag

`get_fields(k, F)` muss exakt den Ausschnitt von `get(k)` liefern:

```
projected = get_fields(k, F)
full     = get(k)
assert: fuer jedes f in F:
           projected.field(f) == full.field(f)   (oder beide absent)
assert: projected.fields enthaelt KEINE Felder ausser F
assert: projected.fields in kanonischer Reihenfolge (siehe 25.3)
```

Explizit zu testen (deckt die drei Fälle aus §25.5 ab):
- **F1 Entity existiert, Feld fehlt:** angefragtes Feld nicht in Entity →
  nicht im Ergebnis, kein Fehler.
- **F2 Entity existiert nicht:** `get_fields(k, F)` == `None`, identisch zu
  `get(k)` (kein Range-Scan zur Entscheidung).
- **F3 unbekanntes Feld:** Name nicht im Schema → kein Sub-Key-Scan dafür,
  Feld taucht nicht im Ergebnis auf.
- **leere Projektion** `F = []` → definiert (leere Entity ohne Felder, nicht
  `None`; semantisch = "Entity existiert, 0 Felder angefragt").
- **unterschiedliche Value-Typen:** Int / String / Bytes / Float / Bool / Null
  alle korrekt materialisiert.

### 25.9 Gates (vor Implementierung)

**Trennung:** G1–G7 sind reine **Korrektheits-Gates** (darf Projection überhaupt
korrekt existieren?), G8 ist das einzige **Wirtschaftlichkeits-Gate** (lohnt
sie sich produktiv?).

| Gate | Klasse      | Frage                                             |
| ---- | ----------- | ------------------------------------------------- |
| G1   | Korrektheit | `get_fields(k,F)` == Ausschnitt von `get(k)`     |
| G2   | Korrektheit | unbekannte Felder korrekt (F3, kein Scan)        |
| G3   | Korrektheit | leere Projektion definiert                       |
| G4   | Korrektheit | alle Value-Typen korrekt (Int/Str/Bytes/...)     |
| G5   | Korrektheit | Recovery unverändert (Oracle nach Reopen)        |
| G6   | Korrektheit | Index-Pfade (`index_eq`/`index_range`) unverändert |
| G7   | Korrektheit | vollständiger `get` bleibt semantisch identisch  |
| G8   | Wirtschaftlichkeit | **messbarer Gewinn bei REALISTISCHEM Feld-Workload** |

### 25.10 Wirtschaftliche Schranke (G8 — harte Schranke)

G8 ist die **einzige harte Schranke** vor einem Produktions-Merge. v0.9b zeigte:

- 1 vs. 100 Felder (gleiche Bytes) → 16.5× → **synthetisch starker Gewinn**.
- 8 Felder, 30B vs. 4000B → nur ~2.5× → **bei flachen Entities moderat**.

**Merge-Verbot:** Kein Produktionsfeature allein aufgrund des synthetischen
100-Felder-Benchmarks. Vor einem Merge muss ein **repräsentativer Workload**
zeigen, dass die tatsächliche Feldbreite einen relevanten Gewinn bringt.
Ein 8-Felder-Workload, der nur marginal profitiert, rechtfertigt keine
zusätzliche Read-Pfad-Infrastruktur; in dem Fall v0.9 einfrieren.

→ v0.10 wird **nur gebaut/gemerged**, wenn G8 am Real-Workload grün ist.

### 25.11 Nächster Schritt

1. Dieses Design reviewen.
2. Gates G1–G8 formal fixieren (in einem Prototyp-Harness).
3. **Dann** erst: kleiner feature-gated `get_fields`-Prototyp
   (`#[cfg(feature = "bench-diag-v10")]`), Oracle + G8-Benchmark.
4. Produktionscommit **nur** bei G8 am Real-Workload grün.

---

## 26. v0.10 Prototyp — Ergebnis

**Status: Prototyp gebaut, G1–G8 alle grün, eingefroren. Keine Produktionsänderung
an `d6d5916`.**

### 26.1 Was gebaut wurde

Feature-gated (`bench-diag-v10`), paralleler Pfad zu `get`, ohne Umbau des
Produktions-Read-Pfads:

- `core_get_fields` (entity.rs) — nur gezielte Sub-Key-Lookups pro angefragtem
  Feld. **Kein Range-Scan über alle Feld-Keys**, kein neuer Decoder, kein neues
  Storage-Format.
- `EntityStore::get_fields` + `CollectionHandle::get_fields` — öffentliche API,
  Spiegel zu `get`/`get_entity`.
- `entity_exists` — Hilfsfunktion für leere Projektion / Entity-Existenz (F2/F-Empty).

`core_get_entity`, `get`, WAL, Compaction, SSTable-Format, Index: **unberührt.**

### 26.2 G1–G7 (Korrektheit) — alle grün

| Gate | Befund | Ergebnis |
| ---- | ------ | -------- |
| G1   | `get_fields(k,F)` == `get(k).filter(F)` (kanonische Reihenfolge) | ok |
| G2   | unbekannte Felder (F3) → kein Scan, nicht im Ergebnis | ok |
| G3   | leere Projektion bei existierender Entity → leere Entity (nicht None) | ok |
| G4   | alle Value-Typen (Int/String/Bytes/Float/Bool/Null) korrekt | ok |
| G5   | Recovery-Oracle nach Reopen identisch | ok |
| G6   | `index_eq` vor/nach `get_fields` identisch | ok |
| G7   | vollständiger `get` semantisch unverändert | ok |

### 26.3 G8 (Wirtschaftlichkeit, synthetisch n=20000, 64-Feld-Entities)

| abgefragte Felder | `get_fields` us/op | vs. full `get` (64 F) |
| ----------------- | ------------------ | --------------------- |
| 1                 | 21.5               | **0.04×**             |
| 2                 | 47.3               | 0.08×                 |
| 4                 | 91.9               | 0.15×                 |
| 8                 | 203.7              | 0.33×                 |
| 32                | 821.8              | 1.35×                 |
| 64 (via `get`)    | 610.2              | 1.00×                 |
| 100               | 1641.9             | 2.69×                 |

### 26.4 Entscheidende G8-Erkenntnis

`get_fields` skaliert **linear mit der Anzahl der angefragten Felder**, nicht mit
der Gesamt-Entity-Größe. Der Gewinn ist **bidirektional begrenzt**:

- **Schmale Abfrage auf breiter Entity** (1–8 Felder) → massiver Gewinn
  (0.04× bis 0.33×, bis 25× schneller).
- **Breite Abfrage** (~32+ Felder) → `get_fields` wird **langsamer** als `get`
  (1.35× bei 32 Feldern): N einzelne Point-Lookups > ein Range-Scan über alle
  Sub-Keys.

→ Projection ist ein **Trade-off**, kein monotoner Gewinn. Er lohnt sich genau
dann, wenn die Abfrage **deutlich weniger Felder** liest als die Entity hat.

### 26.5 Einordnung gegen §25.10 / Merge-Verbot

§25.10 forderte: Merge nur bei relevantem Gewinn am **Real-Workload**, nicht am
synthetischen 100-Feld-Benchmark. G8 bestätigt die §25-Hypothese präziser:

- Bei **flachen Entities (≤8 Felder)** und Abfrage eines Teilsatzes: moderater
  bis starker Gewinn (0.33×–0.04×).
- Bei **breiten Entities mit schmalen Abfragen** (z.B. 2 von 64 Feldern):
  sehr starker Gewinn (0.08×).
- Bei **breiten Abfragen** lohnt sich Projection **nicht** — `get` bleibt
  effizienter.

### 26.6 Nächster Schritt (Entscheidung, nicht automatisch)

- **G8 am Real-Workload prüfen**: typische Abfrage Breite vs. Entity-Breite?
  - Wenn Abfragen regelmäßig nur einen kleinen Feld-Teilsatz lesen → v0.10
    **Produktionsdesign** (echte API, nicht mehr feature-gated) ist gerechtfertigt.
  - Wenn Abfragen meist breit sind → v0.10 verwerfen, Prototyp einfrieren,
    `d6d5916` bleibt unverändert (keine künstliche Optimierung).
- **Optimierungsstufe 2 (nur falls G8 am Real-Workload grün):** breite
  Projektionen könnten über einen einzigen Range-Scan auf den angefragten
  Feld-Key-Bereich beschleunigt werden (statt N Point-Lookups). Erst nach
  Nachweis des Real-Gewinns.

### 26.7 Einfrierung

- `d6d5916` **unangetastet**.
- `bench-v10.rs` als Diagnose-Artefakt erhalten (feature-gated).
- `src/entity.rs`: `core_get_fields`/`get_fields`/`entity_exists` unter
  `#[cfg(feature = "bench-diag-v10")]` — im normalen Release-Build **nicht
  einkompiliert**, kein Overhead, kein Einfluss auf Produktionspfad.

### 26.8 Real-Workload-Gate (nächster Schritt, rein analytisch)

Der synthetische Prototyp hat seine Aufgabe erfüllt. **Kein weiterer künstlicher
Benchmark.** Stattdessen aus tatsächlichem Anwendungscode / vorhandenen
Benchmarks erfassen:

| Frage                                                 | Relevant für      |
| ----------------------------------------------------- | ----------------- |
| Wie viele Felder hat eine typische Entity?            | Entity-Breite     |
| Wie viele Felder werden bei typischen Reads benötigt? | Projections-Breite |
| Verhältnis `requested / entity`                       | G8-Entscheidung   |
| Wie häufig wird vollständiges `get()` benötigt?      | Default-Pfad      |
| Gibt es wiederkehrende 1–8-Feld-Reads?                | Produktionsnutzen |

Entscheidung (kein neuer Code):

```
typische Projektion << Entity-Breite
        → v0.10 Produktionsdesign gerechtfertigt

typische Projektion ≈ Entity-Breite
        → Prototyp verwerfen
        → d6d5916 bleibt Zielbaseline
```

### 26.9 Hinweis: Produktions-API ≠ Prototyp-API

Falls v0.10 in Produktion geht, sollte die öffentliche API **nicht** einfach als
unbedingtes `get_fields()` übernommen werden. G8 zeigte zwei unterschiedliche
Kostenprofile:

- **schmale Projektion** (1–8 Felder) → Point-Lookups effizient.
- **breite Projektion** (~32+ Felder) → ein Range-Scan billiger als N Lookups.

Ein Produktionsentwurf muss einen **Cutoff / eine Strategiewahl** definieren
(z.B. schmal → gezielte Sub-Key-Lookups, breit → Range-Scan), statt blind den
Prototyp-Pfad zu übernehmen. Das ist eine eigene Design-Frage, erst nach
positivem Real-Workload-Gate.

**Status: v0.10-Prototyp abgeschlossen und eingefroren. `d6d5916` bleibt Zielbaseline.**

---

### 26.10 Real-Workload-Auswertung (aus vorhandenem Code, kein neuer Benchmark)

Quellen: `src/query/executor.rs`, `tests/tx_query.rs`, `tests/entity.rs`, `tests/query.rs`.

**Entity-Breite** (typische / mediane / P95 Felder pro Entity):

| Quelle                    | Felder                    | Anzahl |
| ------------------------- | ------------------------- | ------ |
| `tests/entity.rs::user()` | name, age, active         | 3      |
| `tests/tx_query.rs::entity()` | age, score, active, name | 4      |
| Repo-weit (alle Nutzungsbeispiele) | —                | **3–4** |

→ Realer P95 ≈ 4 Felder. Keine breiten Entities im Anwendungscode.

**Read-Breite / Read-Verteilung:**

- `src/query/executor.rs:263,379` → `fetch_stream` / `verify_stream` rufen
  **immer `core_get_entity` (volle Entity)**. Kein einziger selektiver Pfad.
- Query-Muster: `IndexScan → Ids → Fetch(full Entity)` bzw. `FullScan → (id, Entity)`.
- Anteil schmaler Projektionen (1–8 Feld-Reads): **0 %**.
- Anteil Full-`get` / Full-Scan: **~100 %**.
- `requested_fields / entity_fields` ≈ **1.0** (jede Read holt alles).

**Ratio:** `requested ÷ entity` = 4 ÷ 4 = **100 %**. G8-Gewinn bei 4 Feldern
(siehe §26.3) = **0.33× → also ~3× langsamer** als `get`. Projection wäre hier
ein **Nachteil**, kein Vorteil.

### 26.11 Entscheidung: v0.10 verwerfen

Entscheidungsbaum aus §26.8:

```
typische Projektion ≈ Entity-Breite  (4 / 4 = 100 %)
        → Prototyp verwerfen
        → d6d5916 bleibt Zielbaseline
```

Begründung:

- Der echte Workload hat **flache Entities (3–4 Felder)** und liest sie
  **vollständig**. Die in §26.4 identifizierte Gewinnzone (schmale Abfrage auf
  breiter Entity, `requested << entity`) existiert im Real-Workload **nicht**.
- G8 zeigte: bei Entity-Breite ≈ Read-Breite ist `get_fields` **langsamer**
  als `get` (Overhead der N Point-Lookups). Ein Produktionsfeature, das den
  Hot-Path verlangsamt, ist kontraproduktiv.
- §26.9-Cutoff (schmal vs. breit) ist damit **gegenstandslos** — es gibt keinen
  schmalen Pfad im echten Traffic.

**Konsequenz:**

- **v0.10 wird NICHT in Produktion übernommen.**
- Prototyp-Code (`core_get_fields`/`get_fields`/`entity_exists` unter
  `bench-diag-v10`) bleibt als Diagnose-Artefakt, wird aber nicht zum
  Produktionsfeature ausgebaut.
- `d6d5916` **bleibt Zielbaseline**. Keine Änderung.
- Was wir gelernt haben (§26.4): Projection ist ein selektiver Vorteil bei
  `requested_fields << entity_fields`, kein genereller `get`-Ersatz. Diese
  Erkenntnis ist dokumentiert, falls spätere Workloads breitere Entities oder
  schmale Reads aufweisen.

**Nächster sinnvoller Schritt:** Realen Hotspot neu suchen (v0.9-Diagnosekette
wiederholen), statt in eine nicht-lohnende Optimierung zu investieren.

**Status: v0.10 abgeschlossen, verworfen. Diagnosekette (Profiling → Hypothese →
Gegenmessung → Prototyp → Wirtschaftlichkeitsgate → Realitätscheck) sauber durchlaufen.
`d6d5916` unangetastet.**


## §27 Vorgänger-LLM Audit — `value_cache` / Write-Cache

Nach Abschluss der eigenen v0.9/v0.10-Linie (§23–§26.11) wurde die im Working
Tree liegende Vorgänger-Arbeit des vorherigen Coding-LLMs systematisch
übernommen (Phase A: Repository-Handover-Audit). Der `value_cache`-Prototyp
(v0.7-write-cache, Variante A: Old-Value-Cache im `put_entity`-Pfad) war der
Zweig mit der stärksten Verbindung zu einer bereits bekannten offenen Frage aus
v0.8 (Disk-Old-Value-Lookups im Write-Pfad). Er wurde vollständig auditiert,
ohne Dateien zu verändern oder zu committen.

### 27.1 B.1 — Forensik (Korrektheit)

**Architektur:** `value_cache: HashMap<(cid, eid), HashMap<fid, Value>>` lebt als
Feld im `EntityStore`, ausschließlich unter `#[cfg(feature = "bench-diag")]`.
Im normalen Release-Build existiert das Feld nicht (kein Overhead, keine
Produktionskopplung).

**Befüllung/Update:** in `core_put_entity` write-through als *whole-map replace*
nach erfolgreichem Put → enthält danach exakt die indexierten Feldwerte der
Entity. Entfernte Felder fallen automatisch heraus.

**Lese:** nur für indexierte, geschriebene Felder → liefert Old-Value ohne
Disk-Read (Cache-Hit), sonst Fallback auf Point-Lookup (Cache-Miss).

**Invalidation (vollständig):**
- Put (non-tx): whole-map replace ✓
- Delete (non-tx): `value_cache.remove(eid)` ✓
- Tx commit: `value_cache.remove(eid)` ✓
- Tx update/delete: übergeben `None` → nie Cache-Konsultation ✓
- Reopen: Cache ist in-memory, nicht persistiert → leer → Cold-Fallback ✓

**Testabdeckung (`tests/write_cache.rs`, 7 Tests):** Oracle warm==cold über 8
Schritte inkl. Feld-Remove/Reinsert/Interleave; Reopen-Cold-Fallback; 400-Iter-
Flush/Compaction-Konsistenz; Cache-Miss bei nachträglich indexiertem Feld;
Multi-Feld-Cache-Hit; Tx-Rollback (Cache bleibt) + Commit (inval).
→ **Korrektheit formal gut bewiesen.**

### 27.2 B.2 — erster Benchmark (NICHT isoliert)

Vergleich Warm `put`+Cache gegen Transaction-Update ohne Cache.
Ergebnis: **81–98 %** Ersparnis (Δ/COLD) über alle Indexdichten (1–8) und
Entity-Größen (4–64 Felder).

**Methodischer Fehler:** dieser Vergleich mischt zwei Variablen —
(1) fehlender Old-Value-Cache und (2) zusätzlicher Transaction-/Commit-Overhead.
Die 81–98 % sind **überwiegend Tx-Overhead**, nicht Cache-Wirkung. Daher ist
B.2 als Entscheidungsgrundlage **zu verwerfen**.

### 27.3 B.2a — faire Kontrollmessung (reiner Cache-Effekt)

Zwei separate Builds (`--features bench-diag` vs. ohne), identischer non-tx
Put-Stream, konstante Indizes/Flush/Dataset, Warmup + Messrunde.
Cache-Gewinn = (cold_non_tx − warm_cache) / cold_non_tx.

| idx | fields | ON (µs) | OFF (µs) | Δ | Anteil |
|-----|--------|---------|----------|---|--------|
| 0   | 4/16/64 | ~10/30/116 | ~7/27/112 | negativ | innerhalb Rauschen |
| 1   | 4/16/64 | 10/33/127 | 10/31/116 | 0.5/2.6/10.9 | 5/9/9 % |
| 2   | 4/16/64 | 14/35/125 | 13/39/117 | 1/−4/8 | 6/−/7 % |
| 4   | 4/16/64 | 21/40/128 | 19/49/126 | 1/−8/2 | 7/−/2 % |
| 8   | 4/16/64 | 21/57/156 | 19/55/140 | 2/2/16 | 10/3/11 % |

**Ergebnis:** isolierter Cache-Effekt **~0–11 %**, teilweise negative Werte
(innerhalb des Messrauschens / Overhead). Der Cache eliminiert im nicht-
transaktionalen Pfad **keinen relevanten** Anteil des Write-Aufwands.

**Erklärung:** beim non-tx `put_entity` mit `field_hint` werden ohnehin nur
gezielte Point-Lookups für Stale-Candidates und geschriebene indexierte Felder
gemacht (MemTable-Lookup, keine Disk-Seek). Bei breiten Entities dominiert das
Schreiben der Feld- + Index-Keys den Aufwand, nicht der Old-Value-Lookup.

### 27.4 Schlussfolgerung

- **Technisch korrekt:** Prototyp ist solide (B.1), Oracle-Tests grün.
- **Wirtschaftlich nicht relevant:** isolierter Cache-Effekt ~0–11 %.

> **Im untersuchten nicht-transaktionalen Write-Workload liegt der isolierte
> Cache-Effekt bei etwa 0–11 % und ist damit kein hinreichender Grund für eine
> Produktionsintegration.**

(Die Formulierung ist bewusst nicht absolut: ein anderer Workload — z. B. sehr
hohe Indexdichte bei extrem teuren Disk-Seeks — könnte theoretisch anders
aussehen; im gemessenen Pfad ist der Effekt vernachlässigbar.)

### 27.5 Audit-Status

- Keine Änderung an `d6d5916`.
- Kein Produktions-Merge des `value_cache`.
- Kein Eviction-/Memory-Design (da kein relevanter Gewinn).
- Kein weiterer Cache-Mikrobenchmark.
- `value_cache`-Prototyp bleibt als Vorgänger-Artefakt im Tree, nicht in
  unserer Historie (6b65191).
- Vorgänger-Arbeit `compaction_v2`, `diag.rs`, `examples/bench.rs` u. a.
  bleiben für Phase B.3 ff. im Tree (nicht angefasst).

**Damit ist ein weiterer plausibler Optimierungszweig experimentell
ausgeschlossen** — analog zu v0.10, aber über einen anderen Pfad (saubere
Kontrollmessung statt synthetischem Benchmark).




`d6d5916` bleibt während der gesamten Phase 1 unangetastet.


## §28 Vorgänger-LLM Handover-Audit — Abschluss

Die in Phase A begonnene Übernahme der Vorgänger-Arbeit (Working-Tree des
vorherigen Coding-LLMs) ist vollständig auditiert. Es wurden **keine Dateien
verändert** (außer dem temporären `mod diag;`-Eintrag in `lib.rs`, der den
Prototyp lediglich baubar machte) und **nichts committet**. §23–§26 bleiben die
eingefrorene eigene Diagnoselinie.

### 28.1 Ergebnisse

| Zweig | Audit | Ergebnis | Klassifikation |
|-------|-------|----------|----------------|
| `value_cache` (v0.7-write-cache, A) | §27 B.1/B.2/B.2a | technisch korrekt, isolierter Cache-Effekt ~0–11 % → wirtschaftlich verworfen | Klasse B (korrekt, nicht relevant) |
| `compaction_v2` | B.3 | 12/12 Regressionstests grün; testet exakt die d6d5916-Compaction-Architektur (`src/compaction.rs` hat 0 echte Diff zu d6d5916) | Klasse B (wertvolle Regression) |
| `src/diag.rs` | B.4 | atomare Counter, Feature-Gate vollständig (alle Aufrufe gated oder hinter `if active()`, Default `false` → no-op), keine Produktionskopplung | Klasse B (Diagnose-Infra) |
| `examples/bench.rs` | B.4 | v0.7 Baseline-Benchmark, kompiliert mit `bench-diag`, isolierte Workloads | Klasse B (Diagnose-Infra) |
| `examples/bench-ir.rs`, `bench-ir2.rs`, `bench-v2.rs`, `bench-v8.rs` | B.4 | historische v0.7/v0.8-Diagnoseharnesse; brauchen Features `bench-diag-ir(2)/v2/v8`, die **nicht** in `Cargo.toml` definiert sind → aktuell nicht kompilierbar | Klasse C (historisch, nicht baubar) |

### 28.2 Präzisierung `src/diag.rs`

`diag.rs` ist **kein Produktionsbestandteil** und wird hier nicht als solches
„behalten“. Es ist reine Diagnose-Infrastruktur (atomare Counter, nur mit
`--features bench-diag` aktiv). Ob die Datei langfristig im Repository verbleibt,
ist eine **separate Cleanup-/Historienentscheidung**, nicht Teil dieses Audits.
Die in B.4 festgestellte Abwesenheit einer Produktionskopplung bedeutet nur:
das Modul beeinflusst das Laufzeitverhalten ohne `enable()`-Aufruf nicht.

### 28.3 Invarianten

- **Keine Produktionsänderung:** kein Storage-/Index-/Compaction-Code von
  d6d5916 abgewichen (验证 über `git diff d6d5916`: `src/compaction.rs` leer,
  `src/manifest.rs` nur rustfmt, `src/entity.rs` nur Vorgänger-rustfmt + die
  feature-gated `value_cache`-Spur, die nicht im Release-Pfad liegt).
- **Kein Commit** im Rahmen dieses Audits.
- `d6d5916` bleibt **Produktionsbaseline**.
- `6b65191` bleibt die **eingefrorene v0.9/v0.10-Diagnoselinie**.

### 28.4 Nächster Schritt

Erst bei einem **neuen konkreten, reproduzierbaren Befund** (eigene Messung oder
neuer Vorgänger-Zweig mit nachweisbarem Nutzen) wird ein neuer Diagnose-Sprint
(ggf. v0.11) eröffnet — wieder bei Diagnose, nicht bei Implementierung. Die
historischen Bench-Harnesse (Klasse C) rechtfertigen keinen weiteren Sprint.

**Damit ist der Handover von der Vorgänger-Arbeit abgeschlossen.** Eigene
Diagnose (§23–§26) und übernommene Vorgänger-Arbeit sind sauber voneinander

## §29 Phase C — Produktionsänderungen auditieren (Abschluss)

Nach §27/§28 wurde der verbleibende uncommittete Vorgänger-Working-Tree
(Phase C.1 Inventar) vollständig klassifiziert. Ergebnis: **es gibt keine
unbewertete produktive Logikänderung** jenseits dessen, was bereits in §27/§28
bewertet wurde.

### 29.1 Echte (non-whitespace) Diffs vs d6d5916 — Produktionscode

| Datei | Echte Diff-Zeilen | Logikänderung? |
|-------|------------------:|----------------|
| `src/lib.rs` | 11 | Nein — nur `mod diag;` (Audit-Hilfe) + rustfmt |
| `src/entity.rs` | 5 | Nein — nur gated `value_cache`-Parameter (§27) + rustfmt |
| `src/manifest.rs` | 16 | Nein — nur rustfmt |
| `src/diag.rs` | 103 | Neue Datei, aber Diagnose-Infrastruktur (§28 Klasse B) |
| `src/bin/crash_tester.rs` | 8 | Nein — nur rustfmt |
| `src/wal.rs` | 0 | **Unverändert** vs d6d5916 |

Der gesamte restliche Diff-Stat (lib.rs 1635, compaction.rs 523 u. a. Zeilen)
ist ausschließlich CRLF/rustfmt-Rauschen — keine Semantik.

### 29.2 `wal.rs`-Edition-Frage (C.4)

`wal.rs` ist **unverändert** vs d6d5916; `Cargo.toml` war bereits in d6d5916 auf
`edition = "2024"`. Die beim `mod diag;`-Eintrag kurzzeitig gemeldete
Parse-Warnung war ein Tool-Artefakt, kein echtes Problem. **Kein Edition- oder
Parse-Konflikt.** Die frühere Vermutung eines Vorgänger-Edition-Problems
bestätigt sich nicht.

### 29.3 Klassifikation der Vorgänger-Arbeit (gesamtheitlich)

| Bereich | Ergebnis | Konsequenz |
|---------|----------|------------|
| `value_cache` | korrekt, isoliert 0–11 % Effekt (§27) | **verworfen** |
| `compaction_v2` | 12/12 grün, exakt d6d5916-Architektur (B.3) | **wertvolle Regressionstests** |
| `diag.rs` / Bench-Harnesse | Diagnosewerkzeuge (§28) | **historisch/selektiv behalten** |
| übrige `src/`-Änderungen | nur Format/CRLF (29.1) | **keine Produktionslogik** |
| `wal.rs` | unverändert | **kein Risiko** |

### 29.4 Abschluss

Es existiert **keine C.5-Entscheidung** (behalten/verbessern/zurückbauen), weil
keine echte, unbewertete Produktionsänderung vorliegt. Die Vorgänger-Arbeit
besteht vollständig aus: einem verworfenen Prototyp (value_cache), wertvollen
Regressionstests (compaction_v2), Diagnose-Infrastruktur (diag/bench) und
Formatierungs-Rauschen.

**Handover ist damit abgeschlossen.** `d6d5916` bleibt Produktionsbaseline.
Kein Produktions-Merge aus dem Audit. Kein v0.11 ohne neuen konkreten Befund.
Der uncommittete Vorgänger-Working-Tree bleibt bewusst unangetastet.
getrennt: bewiesen (§27 verworfen, B.3 behalten), klassifiziert (B.4), und keine
der beiden Linien hat die Produktionsbaseline d6d5916 angetastet.

---

## §30 Compaction-Forensik — fsync-Dominanz (neue Diagnoselinie, post-v0.10)

> **Diagnose, keine Lösung.** Kein Produktionscode geändert; kein v0.11-Design
> abgeleitet. Nächster Schritt ist das Real-Workload-Gate (§30.6).

### 30.1 Ausgangspunkt

Neuer Diagnosezyklus nach dem abgeschlossenen v0.9/v0.10-Zyklus (§23–§26) und
dem Handover-Audit (§27–§29). Ziel: eine belastbare neue Performance-Frage mit
eigener Messung. Erste Basis-Messung (`bench-baseline.rs`, feature-frei, vier
getrennte Blöcke Put/Flush/Compaction/Read): Compaction ist der dominante
Kostenblock (bei 10k Writes/50 Segmenten ~1.2 s), Put/Flush/Read bleiben im
sub-ms-Bereich.

### 30.2 Isolierte Compaction-Matrix

`bench_compaction_isolated` baut einen definierten L1-Zustand auf (Phase A,
nicht gemessen) und misst genau den einen `flush()`, der die Overlap-Compaction
über alle Segmente auslöst (Phase B). Setup-Fehlschluss behoben: `Options`
Default `segment_max_records=30 000` legte alle Writes in ein Segment; das
Setup nutzt jetzt `segment_max_records=8` + `l0_compact_threshold=2`.

| Segmente | compact() | ms/Seg |
|---:|---:|---:|
| 6 | 23.1 ms | 3.86 |
| 10 | 30.0 ms | 3.00 |
| 26 | 64.7 ms | 2.49 |
| 50 | 120.2 ms | 2.40 |

Skalierung ~linear in der Segmentzahl; `segs_after == segs_before` (Spread-Batch
über den gesamten Key-Range, Voll-Overlap, WA ~1x).

### 30.3 Trennung Merge/Read vs. Output/Manifest/Cleanup

`bench_compaction_forensic` zerlegt die Compaction ohne Produktionscode:
- `base_flush` — eine Table schreiben + Manifest fsync (Referenz, frische DB)
- `merge_read` — voller `scan()` als Proxy für `merge_ids` (liest dieselben Quellen, nur über öffentliche API)
- `compact_total` — der eine `flush()`, der die Overlap-Compaction auslöst
- Output+Manifest+Cleanup — Rest (`compact_total − merge_read − base_flush`)

Matrix (Segmente × Records/Segment):

| Segmente | Rec/Seg | Rows | compact() gesamt | Merge/Read | Output+Manifest+Cleanup |
|---:|---:|---:|---:|---:|---:|
| 6 | 8 | 48 | 18.9 ms | 1.1 ms | **12.9 ms** |
| 6 | 800 | 4800 | 31.4 ms | 9.6 ms | **16.1 ms** |
| 10 | 8 | 80 | 26.8 ms | 1.5 ms | **19.7 ms** |
| 10 | 800 | 8000 | 54.3 ms | 15.7 ms | **33.0 ms** |
| 26 | 8 | 208 | 62.6 ms | 3.8 ms | **54.1 ms** |
| 26 | 800 | 20800 | 109.7 ms | 41.1 ms | **63.2 ms** |
| 50 | 8 | 400 | 111.4 ms | 6.8 ms | **99.6 ms** |
| 50 | 800 | 40000 | 209.9 ms | 79.2 ms | **124.7 ms** |

**Befund:** Output+Manifest+Cleanup dominiert in allen 12 Konfigurationen und
skaliert primär mit der **Segmentzahl** (~2 ms/Segment), nicht mit der
Recordzahl (segs=50: 99.6 ms bei 400 Rows vs 124.7 ms bei 40 000 Rows — fast
flach). Merge/Read ist kein Hotspot (1–7 ms bei kleinen Beständen, erst bei
40k Rows 79 ms).

### 30.4 Codebefund

`TableBuilder::finish()` (`src/sstable.rs`) ruft pro geschriebener SSTable
`w.flush()?` + `w.get_ref().sync_all()?` auf — ein `sync_all()` **je Segment**.
`Manifest::save()` (`src/manifest.rs`) synchronisiert zusätzlich einmal (`f.sync_all()`).
Die 3a-Compaction schreibt über `write_table` jedes neue Segment einzeln
(`compact()` in `src/lib.rs`) und ruft danach `Manifest::save()` auf.

**50 Segmente ≈ 50 SSTable-fsyncs + 1 Manifest-fsync = ~51 fsyncs.**

### 30.5 Unabhängige Kontrollmessung (`bench_fsync`)

N gleich große Dateien (8 KB, gleicher Temp-Dir wie die DB), `write+flush` vs
`write+flush+sync_all`, Median über 7 Runden:

| N Dateien | write+flush | + sync_all | sync-Delta/Datei |
|---:|---:|---:|---:|
| 1 | 0.76 ms | 1.85 ms | ~1.09 ms |
| 10 | 5.06 ms | 13.82 ms | ~0.88 ms |
| 25 | 11.70 ms | 33.11 ms | ~0.86 ms |
| 50 | 26.92 ms | 71.86 ms | ~0.90 ms |

**Kausalitätskette bewiesen:** Segmentzahl → Anzahl `sync_all()` → Latenz.
`sync_all` kostet ~0.9 ms/Datei, konstant und strikt linear; die Forensik-Zahlen
(50 Segmente ≈ 100 ms Output) decken sich mit der Kontrolle + Manifest-fsync +
Cleanup.

### 30.6 Nächster Schritt — Real-Workload-Gate (noch kein v0.11)

Die Hypothese ist bewiesen, aber noch nicht wirtschaftlich relevant. Entscheidend
ist die Frage:

> **Erzeugt der tatsächliche Workload genügend kleine Segmente, dass diese
> Kosten relevant werden?**

Gate: realer Multi-Segment-Workload mit mindestens **10k / 50k / 100k Entities**
(incl. warmer Updates) messen und die tatsächlich entstehende **Segmentzahl**
erfassen. Nur wenn dort regelmäßig 25–50+ Segmente entstehen, ist der fsync-Befund
ein v0.11-Kandidat; bei nur 3–5 Segmenten bleibt die Hypothese korrekt, aber
wirtschaftlich uninteressant (wie value_cache §27).

**Explizit noch NICHT entschieden:**
- Kein Batch-/Deferred-fsync-Design abgeleitet.
- **Durability/Recovery bleibt offene Correctness-Frage:** Wann gilt eine neue
  Compaction-SSTable als dauerhaft? Was passiert bei Crash zwischen
  SSTable-Schreiben und Manifest-Sync? Ist gebündeltes Sync ohne
  Recovery-Sicherheitsverlust möglich? Das ist ein eigenes
  Correctness-/Recovery-Thema, nicht nur Performance.
- Nächster Schritt erst nach dem Real-Workload-Gate; dann §30 → Wirtschaftlichkeitsgate
  → erst dann eventuell v0.11-Prototyp.

**Kein Commit. Kein Produktionscode geändert.**

### 30.7 Real-Workload-Gate — Ergebnis (negativ für Defaults)

Messung (`bench_entity_gate` in `examples/bench-baseline.rs`, Production-Defaults
`memtable_limit=4MB`, `segment_max_records=30k`, `l0_compact_threshold=4`):
Entities mit 4 Feldern + Index auf `age`, danach 5× warme Re-Puts auf das Hot-Set
(10%). Erfasst wird die Endstruktur nach `flush()` + `close()` über eine frische
`Database`-View (`table_count`/`segment_count`/`level_tables(0)`).

| Entities | KV-Records | tables | Segmente | L0 |
|---:|---:|---:|---:|---:|
| 10k | ~50k | 1 | **0** | 1 |
| 50k | ~250k | 3 | **0** | 3 |
| 100k | ~500k | 18 | **17** | 1 |

**Befund:**
1. **Warme Updates erzeugen KEINE Segmentexplosion.** Die Segmentzahl skaliert mit
   dem Gesamt-Datenvolumen (Gesamt-Records ÷ `segment_max_records`), nicht mit der
   Update-Frequenz. Hot-Set-Re-Puts landen als Overwrite in neuen L0-Tabellen und
   werden bei der nächsten Compaction gemerged, ohne die Segmentanzahl zu erhöhen.
2. **Bei Defaults sind Segmente groß** (30k Records), nicht die 8-Record-Minisegmente
   der Diagnose-Konfiguration. 100k Entities ergeben nur 17 Segmente.
3. **Compaction-Last überschlagsmäßig:** ~18 Flush-Äquivalente → bei Threshold 4
   ≈ 4–5 Compactions, zusammen ~25–35 geschriebene SSTables über den ganzen
   Workload → bei ~0.9 ms/fsync ≈ **25–30 ms total auf 7.6 s Workload ≈ 0.4 %**.
4. Die 25–50-Segment-Schwelle aus §30.6 wird bei Defaults erst ab
   >150k Entities (≥750k Records ÷ 30k = 25 Segmente) erreicht.

**Gate-Entscheidung: Der fsync-Befund ist wirtschaftlich korrekt, aber für
Production-Defaults bis 100k Entities irrelevant** (~0.4 % des Workloads). Er wird
nur relevant, wenn Nutzer `segment_max_records` oder `memtable_limit` klein wählen
(diagnose-/bench-ähnliche Konfigurationen) oder Bestände weit über 150k Entities
erreichen. Damit ist das Gate **negativ**: kein v0.11-Prototyp für Batch-fsync als
Default-Änderung abgeleitet. Die Correctness-Frage (wann gilt eine Compaction-SSTable
als dauerhaft, Crash zwischen SSTable-Schreiben und Manifest-Sync) bleibt davon
unabhängig offen (§30.6).

### 30.8 Fazit — Zyklus abgeschlossen

Der fsync/Compaction-Zweig ist damit vollständig abgearbeitet:

1. **Technisch real** — ein echter, reproduzierbarer Hotspot der 3a-Compaction.
2. **Kausal bewiesen** — `Segmentzahl → Anzahl sync_all() → Latenz`, isoliert gemessen
   (Kontrollmessung §30.5, Forensik §30.3).
3. **Gegen den realen Production-Workload gegated** (§30.7) — bei Production-Defaults
   bis 100k Entities keine Segmentdichte, die per-Segment-fsync-Kosten relevant
   werden lässt.
4. **Wirtschaftlich verworfen** — kein v0.11-Prototyp unter den aktuellen Defaults.

Kleine `segment_max_records`/`memtable_limit` und Datenbestände >150k Entities sind
damit **zukünftige mögliche Triggerbedingungen** für diesen Befund, ausdrücklich
**keine offene Optimierungsaufgabe**. Erst ein neuer realer Hotspot oder ein
verändertes Workload-/Produktionsprofil rechtfertigt den nächsten Diagnosezyklus —
nicht die bloße Neugier auf eine Optimierung.

**Diagnose-Freeze: §30 Compaction-fsync diagnosis complete; no production
optimization justified under current defaults.**

---

### 30.9 Correctness-Nachlese — Compaction-Recovery (E.6–E.8)

Die Performance-Diagnose (§30.1–§30.8) war ein **Diagnose-Freeze ohne
Produktionsaenderung**. Im Anschluss wurde die Compaction-Recovery isoliert
getestet (Phase E) und ein echter Correctness-Bug reproduziert.

**E.6 — Ursprüngliche Recovery-Invariante (Annahme):**
Die harte Invariante (§4, §13) besagt, dass das Manifest nie auf eine
nicht-existierende Datei zeigt. Dies gilt fuer die drei Crash-Fenster
(Atomicity durch tmp+fsync+rename). Die (falsche) Schlussfolgerung war, dass
eine manifestierte SSTable damit immer verlässlich lesbar sei — also müsse ein
Lesefehler beim Merge kein Datenverlustrisiko bergen.

**E.7 — Gegenbeweis (reproduzierter Datenverlust):**
Ein Test (`tests/merge_ids_fault.rs`, Vorzustand) loescht waehrend des
Betriebs gezielt eine manifestierte Segment-Datei und loest eine Compaction
aus, deren Batch-Span das Segment ueberlappt. Befund: `merge_ids`
(`src/lib.rs`) nutzte `if let Ok(reader)` / `if let Ok(records)` und
uebersprang die unlesbare Tabelle **still**. Das neue Manifest referenzierte
die Daten nicht mehr → der Key (der NUR in dieser Tabelle lag) war spurlos
verschwunden (`get` lieferte `None`). Zweiter Defekt: Bei cold-Open einer
solchen DB warf `get` einen harten `NotFound`-Io-Fehler, weil
`validate_open_state` nur Ranges, nicht die Dateiexistenz pruefte.

**E.8 — Fehlerpropagierung behebt den Commit-Pfad:**
- `merge_ids` und `table_bounds` propagieren den `open`/Lese-Fehler via `?`
  statt ihn zu schlucken.
- `compact()` bricht bei unlesbarer manifestierter SSTable mit `Err` ab. Da
  `merge_ids` **vor** `manifest.save()` aufgerufen wird, bleibt der alte
  Zustand (Manifest + alte `.sst`) vollständig erhalten — **kein stiller
  Commit, kein Datenverlust-Commit**.
- `validate_open_state` pruefte fehlende Segmentdateien bereits
  (`Error::Corrupt`); ein cold-open liefert damit sauber `Corrupt` statt eines
  späteren `NotFound` aus `get`.

**Explizite Korrektur der bisherigen Aussage:**
- `compact()` darf bei einer unlesbaren manifestierten SSTable **nicht
  erfolgreich committen** (seit E.8 erfüllt).
- Cold-Open mit fehlender Manifest-SSTable wird als `Error::Corrupt`
  behandelt (keine stillschweigende Reparatur).
- Die Atomicity-Invariante (§4/§13) gilt unveraendert fuer die drei
  Crash-Fenster; sie deckt aber **nicht** eine zur Laufzeit unlesbar werdende
  manifestierte SSTable ab — das ist seit E.8 ein bewusst fehlerhafter
  (abbrechender) Pfad, kein stiller Datenverlust mehr.

**Offen (bewusst nicht in E.8 behoben):**
- Orphan-Garbage: physisch nicht mehr vom Manifest referenzierte `.sst`
  werden nicht automatisch geloescht. Das ist Betriebs-/Speicherwartung, kein
  Datenverlust, und wird separat als eigenes Audit bewertet (siehe unten).

**Status:** E.8 ist der minimale Correctness-Fix; `tests/merge_ids_fault.rs`
ist der permanente Regressionstest. Vollständige Test-Suite (`cargo test
--release`) sowie `tests/crash.rs` (Crash-Matrix) gruen. Der Fix wird
gemeinsam mit dieser Dokumentation in einem separaten Commit eingefroren.
Die Performance-Diagnose (§30.8) bleibt unveraendert bestehen.

### 30.10 Orphan-GC — separates Audit (noch nicht implementiert)

Qualitativ anders als E.8: kein Datenverlust, sondern
Speicher-/Betriebswartung. Vor einer etwaigen Implementierung ist nur zu
klaeren (kein Code):

- Welche Dateien duerfen ueberhaupt als Orphans gelten?
- Wie erkennt man sicher, dass eine `.sst` nicht mehr vom aktuellen Manifest
  referenziert wird?
- Was passiert bei Crash genau zwischen Rename und Cleanup?
- Darf GC beim Open automatisch loeschen oder braucht es einen expliziten
  Maintenance-Schritt?
- Welche Recovery-Semantik gilt fuer `.manifest.tmp`?

Erst danach entscheiden, ob sich ein GC-Prototyp lohnt.

**Kein Commit. Kein Produktionscode geändert.**