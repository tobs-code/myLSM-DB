# myLSM-DB — Design v1.3: Composite Indexes (read-only Spec)

**Status:** Read-only Spezifikation. **Kein Code.** `c400bb0` (v1.2) bleibt
eingefrorener Produkt-Checkpoint.
**Nächster Schritt danach:** bei Freigabe separater Implementierungs-Zyklus.

Scope-Engführung (ausdrücklich): **genau eine neue Kernfähigkeit** — ein
Sekundärindex über mehrere Felder mit Equality/Prefix-Nutzung. `group_by`
wird in der Inventur (`design-v1.3-roadmap-inventory.md`) nur als Synergie
festgestellt, ist **nicht** Teil von v1.3. Ebenfalls außerhalb: MVCC/Snapshots,
Index-Intersection, Unique-Constraints, Text-Index, Remote-Backup, ungefragte
Storage-/Format-Neugestaltung.

---

## §1 Befund: bestehende Index-Architektur (Forensik)

- **Key-Layout Single-Field** (`src/keycodec.rs:18-38`):
  `[I][collection_id u32][field_id u32][encode_ordered(value)][eid_len u32][eid]`.
  `encode_ordered` (`src/ordering.rs:116`) ist **selbst-delimitierend**:
  `Null=1B, Bool=2B, Int/Float=9B, String/Bytes` längenbegrenzt mit `0x00 0x00`-
  Terminator. Dadurch ist jede Konkatenation von `encode_ordered`-Werten weiter
  selbst-delimitierend und **präfix-sortierbar**.
- **`IndexDef`** (`src/schema.rs:133`): `id, collection_id, field_id, status`.
  Persistiert als Textzeile `IX {id} {cid} {fid} {status}` (`schema.rs:214`).
- **Schreibpfad** (`src/entity.rs:586-642`): in `core_put_entity`, 3 Phasen —
  (1) neue Index-Keys `PUT` *vor* dem Entity-Write, (2) Entity-Felder
  schreiben/löschen, (3) alte Index-Keys `DELETE` *nach* dem Entity-Write.
  Invariante (das verhindert False Negatives): während der Änderung ist der
  Index temporär ein **Superset** (`src/index.rs:3-19`).
- **Lesepfad** (`src/index.rs:138` `find_m`): liefert *verifizierte* IDs —
  jeder Kandidat wird gegen die Entity geprüft (`Index ist nie Wahrheit`).
- **Planner** (`src/query/planner.rs:184` `pick_index_field_cost`): wählt pro
  AND-Klausel **genau ein** indexierbares Feld; Rest → Residual-`Filter`.
  Kostenmodell `BASE_CARDINALITY * selectivity` (`planner.rs:231`), deterministisch
  (Shape → engere Bounds → lex Feldname).

Diese Punkte sind die exakten Anknüpfungsstellen für Composite — kein neuer
Pfad, sondern ein neuer **Index-Typ** im bestehenden Pfad (im Geiste von v1.2,
wo `core_cas_entity` ebenfalls nur `core_put_entity` erweiterte).

---

## §2 Index-Modell

- **Kanonisch:** `IndexDef { id, collection_id, field_ids: Vec<u32>, status }`.
  Die Definition ist die **geordnete** Liste `field_ids`; die stabile Identität
  ist `id` (`index_id`).
- **Single-Field** ist der Spezialfall `field_ids.len() == 1` (== bisheriges
  `field_id`).
- **Kompatibilität:** Serialisierung wird zu
  `IX {id} {cid} {status} {f1} {f2} ... {fn}` (Status rückt vor die Feldliste).
  Beim Laden: eine `IX`-Zeile mit **genau 4** Tokens wird als Legacy
  Single-Field erkannt (`IX {id} {cid} {fid} {status}`) → `field_ids = [fid]`;
  sonst neue Form. Bestehende Single-Field-Indexes (v1.0–v1.2) bleiben damit
  lesbar, keine Migration nötig.
- `create_index(collection, &[field, ...])` (EntityStore/CollectionHandle) legt
  den Index mit Status `BUILDING` an; der bestehende BUILDING→READY-Ablauf
  (fsync) wird unverändert genutzt.

---

## §3 Key-Encoding (präzise, kollisionsfrei)

### 3.1 Single-Field (unverändert)
`[I][cid u32 LE][field_id u32 LE][encode_ordered(value)][eid_len u32 LE][eid]`

### 3.2 Composite (neu)
```
[I][cid u32 LE][0xFFFFFFFF u32 LE][index_id u32 LE]
   [encode_ordered(c1)][encode_ordered(c2)] ... [encode_ordered(cn)]
   [eid_len u32 LE][eid]
```
- **`0xFFFFFFFF` im `field_id`-Slot = Composite-Namespace-Marker.** Garantiert
  **keine Kollision** mit Single-Field-Keys, deren `field_id` ein echtes Feld
  ist (`< u32::MAX`); `schema.field_id` reserviert `0xFFFFFFFF` explizit.
  Entity-/User-Keys nutzen `ENTITY_TAG` (`'E'`) ≠ `'I'` → ebenfalls disjoint.
- Jedes `encode_ordered(ci)` ist selbst-delimitierend → die Konkatenation ist
  es auch und bleibt **präfix-sortierbar** (Voraussetzung für Prefix-Range).
- Das abschließende `[eid_len][eid]` terminiert den Key; bei **gleichem
  Composite-Wert** unterscheidet die Entity-ID die Einträge (lexikografisch
  letzte Bytes) → stabile, eindeutige Sortierung.

### 3.3 Decode / Prefix-Helfer
- `decode_index_key`: liefert `(cid, field_id, eid)`; bei `field_id == 0xFFFFFFFF`
  ist es ein Composite-Key (Index-Id steht in den nächsten 4 Bytes).
- `decode_composite_key_value`: parst `index_id`, dann **n** Komponenten via
  wiederholtem `ordered_value_len` (n = `field_ids.len()`), dann `eid`.
- `composite_prefix(cid, index_id)`; `composite_value_prefix(cid, index_id,
  enc_leading_tuple)` für Prefix-Scans.

### 3.4 NULL, absent, gemischte Typen (explizit definiert)
- **NULL** (`Value::Null`): present, kodiert als 1-Byte-Tag, `type_rank = 0`
  → sortiert ganz vorne. Ist ein **normaler, orderbarer Wert** und erzeugt
  einen Index-Eintrag. `field = NULL` matched den NULL-Eintrag.
- **absent** (Feld nicht vorhanden): Die Entity erhält **keinen** Composite-
  Eintrag, wenn **irgendeine** Komponente absent ist. Begründung: (a) konsistent
  mit Single-Field (absent → kein Eintrag), (b) ein Composite-Prädikat auf
  führende Spalten liefert nur Entities, die *alle* Komponenten besitzen — eine
  Entity ohne eine Komponente kann per Definition kein Composite-Match sein.
  **absent ≠ NULL** (hart definiert).
- **Gemischte Typen:** Komponenten werden via `encode_ordered` typ-geordnet
  (`type_rank`: Null<Bool<Int<Float<String<Bytes, `ordering.rs:45`). Innerhalb
  *einer* Komponente ist der Vergleich über `value_cmp` wohldefiniert; über
  Typgrenzen hinweg ist die Ordnung deterministisch (selten, aber exakt
  spezifiziert).

---

## §4 Mutation (im bestehenden `core_put_entity`, kein neuer Pfad)

- `indexed = single_field_indexed ∪ { alle Composite-Komponentenfeld-IDs }`.
- **Old-Value-Erfassung** erweitern: auch Composite-Komponentenfelder erfassen
  (warm: zusätzliche Point-Lookups; cold: ohnehin alle Felder vorhanden).
- Für jeden Composite-Index:
  - `old_tuple` = Werte der Komponenten aus der **alten** Entity, nur wenn
    **alle** Komponenten present; sonst `None` (→ kein alter Eintrag).
  - `new_tuple` = Werte der Komponenten aus der **neuen** Entity, nur wenn
    **alle** present; sonst `None`.
  - Ist `old_tuple != new_tuple`: Phase 1 `PUT(new_key)` (falls `new_tuple`
    `Some`), Phase 3 `DELETE(old_key)` (falls `old_tuple` `Some`).
- Diese Composite-Index-Ops hängen in dieselbe 3-Phasen-Struktur
  (`entity.rs:584-642`) → keine False Negatives, atomar mit der Entity,
  WAL-konsistent. Da `core_cas_entity` (v1.2) und `Transaction`/`DirectMutator`
  denselben `core_put_entity` nutzen, wirken Composite-Indexes **automatisch**
  in CAS und Transaktionen mit — keine Sonderwege.
- `core_delete_entity`: löscht alle Composite-Keys der Entity (über `indexed`,
  analog Single-Field).

---

## §5 Query: `find_composite`

Signature (Entwurf):
```text
find_composite(
    m: &mut impl Mutator, schema, cid, index_id, field_ids,
    leading_bounds: &[(component_index: usize, lower: Bound, upper: Bound)],
) -> Result<Vec<String>>   // verifizierte IDs
```
- `leading_bounds` müssen eine **kontinuierliche Präfix** der Komponenten
  (`0..k`) abdecken. Nicht-kontinuierliche Präfixe sind ein Fehler
  (`InvalidArgument`) — nur führende Spalten sind indexfähig (Standardregel).
- **Bereichs-Konstruktion:** für jede gebundene führende Komponente `encode_ordered`
  von `lower`/`upper` (analog `merge_bounds`, `planner.rs:311`); Prefix =
  `composite_value_prefix(cid, index_id, enc_lower_tuple)`; `start = prefix`,
  `end = successor(prefix_upper)` (bzw. `Unbounded` → `successor(composite_prefix)`).
- **Verifikation:** wie `find_m` — jeder Kandidat wird gegen die echten
  Feldwerte geprüft (`within(bounds)`). Deckt NULL/absent/Race ab; Index bleibt
  Kandidatenquelle.

### 5.1 Indexierbare Prädikate (präzise)
- Eine Komponente `i` ist indexfähig g.d.w. **alle** Komponenten `0..i-1` durch
  ein **Equality**-Prädikat gebunden sind (Prefix-Regel).
- Auf der tiefsten führenden Spalte: `Eq/Gt/Gte/Lt/Lte/Between` → Range nutzbar.
- `Ne` (negierte Literale) → **nicht** indexfähig auf dieser Komponente
  (→ Residual), konsistent mit Single-Field.
- Verbleibende Literale (nicht führend, `Ne`, nicht-indexiert) → Residual-`Filter`.
- **Keine Index-Intersection** in v1.3: pro Klausel höchstens ein Index
  (Composite *oder* Single-Field), Rest Residual.

---

## §6 Planner

- `pick_index_field_cost` (Single-Field) bleibt. Neu: `pick_composite_index`
  enumeriert Ready-Composite-Indexes; für jeden zählt sie, wie viele
  **führende** Komponenten durch indexierbare Literale abgedeckt sind
  (Equality zwingend für Präfix-Spalten außer der letzten Range-Spalte).
- **Kostenmodell:** `cost = BASE_CARDINALITY * Π selectivity(leading components)`
  — ein Composite mit `k≥2` indexierbaren führenden Spalten ist damit
  streng selektiver als jeder Single-Field-Index (`k=1`) → wird bevorzugt.
- **Deterministische Auswahl:** bei gleicher Coverage gewinnt der niedrigere
  `index_id`; Tie-Break ansonsten wie heute (engere Bounds → lex). Abwägung
  Composite vs Single-Field vs FullScan erfolgt in `plan_clause` an einer
  Stelle.
- Erzeugt `Fetch { CompositeIndexScan { index_id, field_ids, leading_bounds } }`;
  Residual = Klausel-Literale, die nicht durch den gewählten Index abgedeckt
  sind. Neuer PhysicalPlan-Knoten `CompositeIndexScan` (überlädt `IndexScan`
  nicht); Executor erhält `composite_index_scan` analog `index_scan`
  (`executor.rs:273`), das `find_composite` ruft und verifiziert.
- **`IndexOrderScan` (ORDER BY) über Composite ist in v1.3 explizit NICHT
  enthalten** — hält den Scope auf eine Kernfähigkeit; späterer Spec.

---

## §7 Recovery / Rebuild

- `index::rebuild` erhält das Pendant `rebuild_composite(db, cid, index_id,
  field_ids)`: löscht alle Composite-Keys dieses `index_id` (via
  `composite_prefix`), scannt dann die ganze Collection, und schreibt für jede
  Entity, die **alle** Komponenten besitzt, den Composite-Key. Idempotent;
  der BUILDING→READY-Ablauf (fsync) bleibt unverändert.
- **Index bleibt abgeleitete Wahrheit**: Query verifiziert ohnehin gegen die
  Entity → ein corrupter/fehlender Composite-Eintrag erzeugt höchstens ein
  False Positive (heimgefiltet), **nie** ein False Negative. Rebuild heilt.

---

## §8 Oracle (unabhängige naive Auswertung)

Naive Referenz: pro Collection eine in-memory `Map<entity_id, Entity>`; eine
Composite-Query wird ausgewertet, indem **alle** Entities geprüft werden und
die Prädikate exakt nach Semantik angewandt werden:
- Komponente absent → Entity erfüllt das Composite-Prädikat nicht (kein Eintrag).
- `NULL` present → matched den NULL-Wert.
- Vergleich via `value_cmp` (type-geordnet).

Erwartete IDs = sortierte Menge; Vergleich mit dem DB-Ergebnis **sowohl über
den Planner-Pfad** (`CompositeIndexScan`) **als auch über einen erzwungenen
FullScan-Oracle** (Residual-Verifikation identisch).

Abdeckung (Test-Matrix):
1. 1-, 2- und 3-Feld-Composite-Indexes.
2. Equality und Prefix (Range auf führender/letzter gebundener Spalte).
3. NULL vs. absent (present-NULL-Eintrag matcht; absent matched nie).
4. Gemischte Typen in Komponenten.
5. Duplicate Values (mehrere Entities mit identischem Composite-Wert).
6. Update eines indexierten Feldes (alter Eintrag weg, neuer da).
7. Delete / Reinsert.
8. Bestehende Single-Field-Indexes **parallel** zu Composite-Indexes.
9. Planner-Pfad vs. Full-Scan-Oracle (Ergebnisidentität).

---

## §9 Umfang (Bestätigung der Grenzen)

Implementiert in v1.3: Composite-Index-Typ (Modell, Encoding, Mutation über
`core_put_entity`, `find_composite`, Planner-Auswahl, Rebuild, Oracle).

Explizit **nicht** in v1.3: MVCC/Snapshots · Index-Intersection · Unique-
Constraints · Text-Index · Remote-Backup-Arbeit · ungefragte Storage-/Format-
Neugestaltung · `group_by` (nur Inventur-Befund) · `ORDER BY` über Composite.

---

## §10 Nächster Schritt (kein Code)

Bei Freigabe: Implementierungs-Zyklus entlang §2–§8 (IndexDef-Erweiterung +
`IX`-Zeilenmigration, Composite-Key in `keycodec`, `core_put_entity`-
Composite-Pflege, `find_composite` + `CompositeIndexScan`, `pick_composite_index`,
`rebuild_composite`, Oracle-Fixtures). `c400bb0` bleibt bis dahin unangetastet.
