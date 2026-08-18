# v0.5.2 — API-Härtung (Design, noch nicht implementiert)

Vorab-Architekturreview (v0.1–v0.5.1) identifizierte drei latente Probleme der
öffentlichen Semantik. Dieses Dokument friert die Entscheidungen für v0.5.2
ein. **Kein Implementierungscode vor Freigabe.**

```text
v0.5.2
├── Entity-ID contract
├── Error taxonomy
├── Read-only schema invariant
└── compatibility / migration notes
```

---

## 1. Entity-ID contract

### Entscheidung: `String`/`&str` bleibt die API — validiertes UTF-8.

- **Kleinste API-Änderung.** Alle öffentlichen Methoden nehmen bereits
  `&str`/`String` (CollectionHandle::put/get/delete, Transaction::update/
  delete/get/find, scan_collection, execute_query → `Vec<(String, Entity)>`).
- **Kein stilles `from_utf8_lossy` mehr.** Decode-Stellen, die eine
  Entity-ID aus einem Speicher-Key rekonstruieren, müssen **strikt**
  validieren und bei nicht-UTF-8 einen Fehler liefern — nie ersetzen.
- Nicht-UTF-8 in **gespeicherten** Keys ist **Storage-Korruption** →
  `InvalidFormat` (nicht `InvalidArgument`). Das erfüllt zugleich das
  Kriterium „SSTable-Korruption bleibt Format-/Storage-Fehler“.

### Inventar der betroffenen Stellen

Write-Seite (immer valid, da `&str`. Bisher ok, kein Fix nötig):
- `Transaction::update` entity.rs:621 `entity_id.as_bytes()`
- `Transaction::delete` entity.rs:634, `Transaction::get` entity.rs:641/654
- `CollectionHandle::put/get/delete` entity.rs:793–806

Read-/Decode-Seite (heute **lossy** — zu ersetzen):
- `core_scan_collection` entity.rs:415 `String::from_utf8_lossy(&ee)`
- `index::find_m` index.rs:145 `String::from_utf8_lossy(entity)`
- `query/executor.rs:199` `ScanAssembler` `from_utf8_lossy(ee)`

Nicht-Entity-ID-Decodes (bleiben wie sie sind — bereits strikt):
- `codec.rs:99` / `ordering.rs:227` validieren `Value::String` UTF-8 (Daten, nicht IDs).

Lücke (zu schließen): `EntityStore::put_entity`/`get_entity`/`delete_entity`
(collection_id: u32, entity_id: **&[u8]**, entity.rs:509–534) sind öffentlich
und akzeptieren rohe Bytes. Um die Invariante „gespeicherte IDs sind immer
UTF-8“ zu halten:
- **Write** (`put_entity`, `delete_entity`): Entity-ID auf UTF-8 validieren →
  `InvalidArgument` bei Verstoß (nie persistieren).
- **Read** (`get_entity`): kein Fix nötig (liefert `Entity`, keine ID).

### Contract

1. Eine Entity-ID ist ein nicht-leerer, valid-UTF-8-String.
2. Der Store persistiert nie eine nicht-UTF-8-ID.
3. Beim Lesen wird eine gespeicherte ID **strikt** dekodiert; nicht-UTF-8 →
   `InvalidFormat` (Korruption), kein Lossy-Ersatz.
4. Ein Implementierungs-Helfer bündelt das strikte Decode:
   `fn decode_entity_id(bytes: &[u8]) -> Result<&str>` (z.B. in `keycodec`),
   genutzt von entity.rs:415, index.rs:145, query/executor.rs:199.

---

## 2. Error taxonomy

### Entscheidung: separater `InvalidArgument` für API-Fehler; `InvalidFormat` reserviert für Persistenz-/Encoding-Korruption.

`src/error.rs` (Error enum) bekommt eine neue Variante:

```rust
/// Argument-/Aufruf-Fehler (falsche Nutzung der API).
InvalidArgument(String),
```

Display: `write!(f, "invalid argument: {s}")`.

### Kategorisierung aller aktuellen `InvalidFormat`-Stellen

**A. Bleibt `InvalidFormat` (Korruption / Encoding / Persistenz):**

| Stelle | Bedeutung |
|---|---|
| codec.rs:70–117 | decode gespeicherter Value (leer/zu kurz/unbekannter Tag/kein UTF-8) |
| sstable.rs:138,147 | SSTable zu klein / falsches Magic |
| manifest.rs:37–46 | Manifest-Parse |
| schema.rs:35,249–317 | Schema-Parse, unbekannter Index-Status |
| ordering.rs:61–269 | Ordered-Encoding-decode (Escape/UTF-8/Tag) |
| entity.rs:366 | „bad entity key“ (Struktur des Keys) |
| entity.rs:373,406 | „unknown field id“ (Daten referenzieren Feld, das Schema nicht kennt) |
| index.rs:144 | „bad index key“ (Struktur des Keys) |
| query/executor.rs:194 | „bad field value“ (Value-decode) |
| query/executor.rs:197 | „unknown field id“ (wie entity.rs) |
| **neu** | nicht-UTF-8-Entity-ID im gespeicherten Key (Abschnitt 1) |

**B. Wird `InvalidArgument` (API-/Aufruf-Fehler):**

| Stelle | Bedeutung |
|---|---|
| entity.rs:601 | „transaction is not active“ (Operation auf inaktiver Tx) |
| entity.rs:687, 829 | „unknown field {field}“ (find auf unbekanntem Feld) |
| entity.rs:785, 788 | „read-only view“ (put/delete auf DirectScan) |
| index.rs:134 | „no index on field“ (find auf nicht indexiertem Feld) |
| query/executor.rs:260 | „unknown field {field}“ (Query referenziert unbekanntes Feld) |
| query/executor.rs:263 | „no index on field {field}“ (Query auf nicht indexiertem Feld) |
| **neu** | nicht-UTF-8-Entity-ID als Schreib-Eingabe (Abschnitt 1, Write-Seite) |

### Gate

- SSTable-Korruption (falsches Magic, truncated, kaputtes Index-/Manifest-/
  Schema-Format, unbekannter Value-Tag) erscheint **weiterhin** als
  `InvalidFormat`/`Corrupt` — nie als `InvalidArgument`.
- Jede `InvalidFormat`-Stelle aus Gruppe A bleibt unverändert (nur Docs/kein Code).
- Nur Gruppe B migriert zu `InvalidArgument`.
- Exhaustive-Matches auf `Error` (falls vorhanden) werden angepasst.

---

## 3. Read-only schema invariant

### Entscheidung: reine Reads dürfen **nie** eine Collection/Field-Definition erzeugen oder Schema schreiben.

- `Schema::collection_id(&mut self, …)` (schema.rs:75) **legt an** (mutiert,
  `changed = true`).
- `Schema::lookup_collection_id(&self, …)` (schema.rs:88) ist read-only.
- Gleiches Muster für `field_id` (schema.rs:101) vs `lookup_field_id` (schema.rs:93).

### Verstöße heute (Reads, die `collection_id()` auslösen)

| Stelle | Read | Fix |
|---|---|---|
| `EntityStore::scan_collection` entity.rs:493 | ja | → `lookup_collection_id`, fehlt → `Ok(vec![])`, **kein** `persist_schema` |
| `Transaction::get` entity.rs:640 | ja | → `lookup_collection_id`, fehlt → `Ok(None)` |
| `Transaction::scan_collection` entity.rs:661 | ja | → `lookup_collection_id`, fehlt → `Ok(vec![])` |
| `Transaction::find` entity.rs:682 | ja | → `lookup_collection_id`, fehlt → `Ok(vec![])` (konsistent zu Query-Executor) |

Legitime Mutation bleibt nur auf **Write-/expliziten** Pfaden:
- `EntityStore::collection(name)` entity.rs:442 (Handle explizit anfordern → get-or-create).
- `Transaction::update` entity.rs:614, `Transaction::delete` entity.rs:627 (Schreiben in neue Collection ist erlaubt).
- `core_put_entity` entity.rs:234 `field_id()` (Schreiben eines neuen Feldes).
- `CollectionHandle::create_index` entity.rs:810 `field_id()` (Index auf neuem Feld).

Bereits sauber (kein Fix):
- Query-Pfad nutzt durchgehend `lookup_collection_id`/`lookup_field_id` (query/planner.rs:132,317; query/executor.rs:138,163,196,256,259).
- `CollectionHandle::find` entity.rs:824–829 `lookup_field_id`.

### Invariante (post-v0.5.2)

- Kein Read-Pfad ruft `collection_id()`, `field_id()` oder `persist_schema()`
  auf. Verifikation: `rg` auf die vier Fix-Stellen + Grep nach „auf Read“.
- Reads auf nicht vorhandenen Collections liefern leere Ergebnisse (`None`/`vec![]`),
  ohne das Schema zu berühren (kein persistierter Nebeneffekt).

---

## 4. compatibility / migration notes

- **API-Bruch (bewusst, vor v0.6):** `Error` gewinnt `InvalidArgument`. Jeder
  externe Match auf `Error` muss erweitert werden. Fehlertexte ändern sich an
  den migrierten Stellen (kein Format-Stabil-Versprechen — Teil des Freeze).
- **`from_utf8_lossy`-Entfernung:** Verhalten ändert sich an 3 Decode-Stellen:
  zuvor still ersetzt, jetzt Fehler. Nur für bereits korrupte Daten sichtbar.
- **Read-only-Semantik:** Reads auf unbekannte Collections erzeugen vorher eine
  (leere) Collection im Schema + Schema-Write; danach nie. Für korrekte Aufrufer
  transparent.
- **SSTable-/Persistenz-Korruption** bleibt exakt wie heute im
  `InvalidFormat`/`Corrupt`-Domain — bestandene Korruptions-Tests bleiben grün.
- **Kein neuer Feature-Umfang.** Tx-Queries, Cost-Based Planner, Index-Order/
  Top-K und `Mutator`-Split bleiben **unangetastet** (v0.6+).

---

## Regressionstests (vor Implementierung schreiben, dann grün ziehen)

### Entity-ID
- `put_entity`/`delete_entity` mit nicht-UTF-8-ID → `InvalidArgument`
  (wird abgelehnt, nicht persistiert).
- Fremden/korrupten Key `E|cid|0xFF…|fid` per `Database::put` injizieren →
  `scan_collection`/`find`/Query liefern `InvalidFormat` (nicht lossy).

### Error taxonomy
- `find` auf nicht indexiertem Feld → `InvalidArgument`.
- Operation auf inaktiver (`committed`/`aborted`) Transaktion → `InvalidArgument`.
- SSTable mit falschem Magic öffnen → `InvalidFormat` (bleibt Storage-Domain).
- bestehende Korruptions-Tests (sstable.rs) bleiben grün.

### Read-only schema
- Nach `scan_collection("nope")`: leer + `lookup_collection_id("nope")` ist
  `None` + `is_changed()` ist `false` / kein Schema-Write.
- Nach `tx.get`/`tx.scan_collection`/`tx.find` auf unbekannter Collection:
  `None`/leer, Schema unverändert.

### Gates (Pflicht)
`cargo test` · `cargo test --release` · `cargo fmt --check` ·
`cargo clippy --all-targets` (lib bleibt bei Baseline-Warnings, 0 neue).

## Scope / Nicht-Scope
- **In v0.5.2:** oben (1) Entity-ID-Contract, (2) Error-Taxonomie, (3) Read-only
  Schema-Invariante, Tests, Design-gerechte Migration.
- **Nicht in v0.5.2:** Concurrency/MVCC, Compaction-Umbau, Tx-Queries,
  Cost-Based Planning, Index-Order/Top-K, `Mutator`-Split — erst nach API-Freeze.