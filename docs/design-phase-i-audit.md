# myLSM-DB — Design Phase I: Production-Readiness-Audit (read-only)

**Status:** Read-only Befund. **Kein Code-Wandel.** `74f8a2f` (v1.1) bleibt
eingefrorener Produkt-Checkpoint.
**Ziel:** Gezielte Prüfung der Panic-/Fehler-Verträge — nicht „438 Stellen
reduzieren". Die Zahl `438` ist bewusst **kein Befund** (siehe §1).

---

## §1 Die „438" sind kein Befund

Verteilung von `.unwrap()` / `.expect()` in `src` (gemessen, Produktion vs.
`#[cfg(test)]`):

| Bereich | Anzahl |
| --- | --- |
| `#[cfg(test)]`-Module (alle `tests/`-Blöcke in `src`) | 371 |
| `src/bin/crash_tester.rs` (Dev-/Crash-Harness, nicht shipped API) | 18 |
| **Produktions-Bibliothekscode** | **49** |
| **Summe** | **438** |

Erkenntnis: ~85 % der „438" sind Test-Code; weitere 18 sind ein
Test-Werkzeug, das bewusst bei Fehler panict. Es bleiben **49 relevante
Stellen in der Bibliothek**. Davon ist keine einzige an einer I/O-Grenze
(§3) und keine in einem öffentlichen API-Einstiegspunkt, der stattdessen
`Result` zurückgeben müsste (§2).

---

## §2 Öffentliche API-Fehlerverträge (Kategorie 1)

Geprüfte Einstiegspunkte: `put`/`get`/`delete`/`scan` (KV), `EntityStore`-
CRUD/Query/Aggregation/Transaction, `backup`/`restore`, `compact_full`/`gc`,
sowie `lsm-admin`-Befehle.

- Alle diese Methoden haben Signatur `-> Result<…>`. Interne `.unwrap()` in
  ihren Bodies existieren **nicht** (siehe 49er-Liste: einzige `lib.rs`-
  Vorkommen sind `table_cache.get(id).expect("cached")` — reine
  Laufzeit-Invariante, siehe §5 C).
- **Einzig erreichbarer Panic-Pfad über eine öffentliche API:**
  `impl Index<&str> for Entity` (`src/entity.rs:54`):
  ```rust
  fn index(&self, name: &str) -> &Value { self.field(name).expect("field not found") }
  ```
  Das ist der `entity["feld"]`-Operator. `Index::index` **muss** per
  Rust-Trait-Vertrag bei fehlendem Schlüssel paniken (kann nicht `Option`
  liefern). Der sichere, nicht-panikende Pfad ist `entity.field("x") -> Option`.
  → **Kategorie C** (idiomatischer Trait-Vertrag; dokumentationswürdig, kein Fix
  nötig).

**Befund Kategorie 1: kein A.** Ein Nutzer kann über normale API-Aufrufe
(`put`/`get`/`delete`/Query/Transaction/Backup/Restore/Admin) **keinen** Panic
auslösen; alle Fehlerwege sind `Result`.

---

## §3 I/O-Grenzen (Kategorie 2)

Geprüft: `File::open`, `.sync()`, `.rename()`, `fs::copy`, `create_dir_all`,
`read_to_string`, `.write_all()`, `.read()`, `.metadata()`, `read_dir` in
Produktionscode.

- **Messung:** kein einziger I/O-Aufruf in Produktion ist mit `.unwrap()` /
  `.expect()` versehen (expliziter Scan: 0 Treffer). Alle propagieren via
  `?` (der `Error`-Typ hat `From<io::Error>`).
- Insbesondere die v1.0/v1.1-Pfade sind sauber: `backup()`/`restore()` nutzen
  `std::fs::copy(...) ?`, `create_dir_all(...) ?`, `remove_dir(...) ?`;
  WAL-/SSTable-/Manifest-I/O geht durchgehend über `Result`.

**Befund Kategorie 3: kein A.** Die I/O-Grenze ist vollständig
fehlerpropagierend — ein Platten-/Permissions-/Crash-Fehler wird zum
`Result`, nicht zum Panic.

---

## §4 Input-/Format-Grenzen (Kategorie 3)

Manifest, Schema, SSTable, WAL, Benutzerwerte, Query-Parameter.

- Decode-Pfade (`codec.rs`, `keycodec.rs`, `sstable.rs`, `wal.rs`,
  `ordering.rs`) nutzen `try_into().unwrap()` auf Slices — **ausschließlich
  nach** vorangehender Längen-/Magic-/CRC-Prüfung, die bei Verletzung
  `InvalidFormat`/`Corrupt` liefert. Beispiele:
  - `sstable.rs:176/185` → `InvalidFormat` vor Footer-Parsing (`sstable.rs:183-192`).
  - `wal.rs:246` CRC-Check vor Payload-Decode.
  - `codec.rs:70-110` → `InvalidFormat` bei zu kurzem Wert.
  Die `unwrap()` sind damit bewiesene Invarianten (C), kein
  ungeprüfter Fremd-Input.
- Query-Parameter: `query/planner.rs:337` `iter.next().expect("clause has literals")`
  ist eine Planner-intern Invariante (C).

**Befund Kategorie 4: kein A.** Korrupte/fehlgeformte Inputs werden an den
Format-Grenzen als `InvalidFormat`/`Corrupt` zurückgewiesen; keine Panic aus
Nutzer-Input.

---

## §5 Admin-/Backup-Pfade (Kategorie 4) + Panic-Taxonomie (Kategorie 5)

### Panic-Primitive in Produktion (vollständig, excl. Test/Crash-Harness)
- `src/bin/lsm-admin.rs:99` `_ => unreachable!()` — Match-Exhaustiveness-Guard
  nach bereits validiertem Kommando-Satz (C).
- `src/query/executor.rs:555` `Aggregate::Count => unreachable!()` — bewiesene
  Invariante (`Count` wird vorher separat behandelt) (C).
- `assert!` in Produktion: **0** Treffer.

### Taxonomie der 49 Bibliotheks-Stellen
- **A — echter Produktionsfehler (Panic statt Result auf erreichbarem Pfad):**
  **0**.
- **B — fragwürdige Robustheit (Dokumentation/Abwägung):** 1
  (`Entity::Index` panic, §2 — idiomatisch, bewusst beibehalten).
- **C — bewiesene interne Invariante (bewusst behalten):** 48
  (Slice-`try_into` nach Längenprüfung, `table_cache.expect("cached")`,
  Merge-Heap-`unwrap`, `unreachable!`-Guards).

---

## §6 Fazit & Empfehlung

Der Audit liefert **keinen A-Befund**. Die ursprüngliche Sorge „438 unwrap"
war eine Metrik ohne Befund: ~84 % davon sind Test-Code, die I/O-Grenze
propagiert vollständig, und alle öffentlichen API-Pfade sind `Result`-basiert.
Das Hardening wird daher **nicht** erzwungen.

**Empfehlung:**
1. `74f8a2f` als Produkt-Checkpoint einfrieren.
2. Kein technischer Fix-Zweig aus Phase I ableiten.
3. Nächster Schritt = **Produktentscheidung**, nicht automatischer Bug-Fix.
   Kandidaten (bewusst als Features, nicht als Bugs):
   - CAS / Partial Updates (`put` mit Version/If-Match),
   - Snapshots / MVCC (Point-in-Time-Reads),
   - Composite / Multi-Column Secondary Indexes,
   - Remote/Object-Storage-Backup (baut auf v1.1 Backup-Root auf).

Sollte künftig durch einen konkreten Nutzerbericht ein A-Befund entstehen
(z. B. ein bestimmter Korruptionsfall, der doch panict statt `Corrupt`
zurückzugeben), erhält dieser seinen eigenen, befundgetriebenen Zyklus —
losgelöst von der abstrakten „438".
