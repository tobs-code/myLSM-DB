# myLSM-DB — Phase H: v1-Readiness / Produktinventur

**Status:** Read-only Inventur. **Kein Code.** Baseline: `5daa675` (v1.0, stabil).
**Frage:** *Was fehlt dieser Embedded-Datenbank, damit ein konkreter Nutzer
sie tatsächlich als v1 einsetzen kann?* — nicht: *Was programmieren wir als
Nächstes?*

**Methode:** Fünf Dimensionen gegen den tatsächlichen Codestand geprüft;
danach **maximal ein** nächster Arbeitszweig ausgewählt.

---

## §1 Capability-Stand (v0.1 → v1.0, evidenzbasiert)

| Ebene | Fähigkeiten | Quelle |
|-------|-------------|--------|
| Datenmodell | Entities, Felder, stabile Collection/Field-IDs, Secondary Index | `schema.rs`, `keycodec.rs` |
| CRUD | `put`/`get`/`delete` (Entity-Ebene + Collection-Handle), Transaktionen (BEGIN/COMMIT/ABORT, RW) | `entity.rs` |
| Query | Filter/Sort/Limit, Index-Scans, Full-Scan-Oracle, Projektion, Aggregationen (Count/Sum/Avg/Min/Max) | `query/*`, `executor.rs` |
| Storage | WAL + Recovery, L0→L1 Compaction (Overlap-Merge, atomar), Orphan-GC | `wal.rs`, `lib.rs`, `compaction.rs` |
| Persistenz | Format-Versionierung (VERSION-Datei, `UnsupportedFormatVersion`), Crash-sicherer Manifest-Commit | `version.rs`, `manifest.rs` |
| Betrieb | `inspect`, `stats`, `compact`, `gc` (CLI) | `src/bin/lsm-admin.rs`, `lib.rs` |

**Bewusste Grenze (dokumentiert):** Single-Writer. Kein Cross-Process-Lock
(`gc`-Doc: In-Process-Exklusivität via `&mut self`; Cross-Process bewusst
nicht implementiert).

---

## §2 Dimensionen-Bewertung

### 2.1 API — *teilweise bereit*
**Geht:** vollständiges CRUD, Index-Queries, Transaktionen, Filter/Sort/Limit,
Projektion, Aggregationen.
**Fehlt:** Partial/Conditional Update (`update id set f=v where version==n`,
CAS, Merge/Patches). Es existiert nur `put_entity` = Vollersatz.
**Aber:** Im **Single-Writer-Modell** ist ein Conditional Update weitgehend
*emulierbar*: `get → Prüfung → put` läuft in dem einen Writer atomar, weil
kein zweiter Writer dazwischenkommen kann. CAS ist hier also primär
**Convenience**, kein Korrektheits-Blocker. (Siehe §3 B.)

### 2.2 Korrektheit — *Verträge sauber, ein HAUPT-CAVEAT + Robustheit offen*
- Compaction-/Recovery-Verträge sind schlüssig: Manifest-Commit (fsync +
  atomarer rename) **vor** Löschen alter SSTables; Tombstone-Drop nur bei
  vollständiger Historie im Merge-Set (Disjunktheits-Invariante).
- **HAUPT-CAVEAT — deferred durability:** Schreiben sind **erst nach
  erfolgreichem `flush()`/`close()` dauerhaft** (`lib.rs:196-199`). Ein Crash
  vor `flush`/`close` verliert die jüngsten Writes. Das ist Design, aber für
  einen Produktionseinsatz **muss die Host-App** flush/close garantieren —
  sonst realer Datenverlust.
- **Robustheit/Readiness-Signal:** `src` enthält **438 `.unwrap()/.expect()`**
  (nicht in Tests). Viele Pfade können bei I/O-Fehlern (Disk voll, Rechte)
  **panic** statt `Result` zurückzugeben. Für eine Embedded-Library, die in
  eine Host-App gelinkt wird, ist das ein echtes Produktionsrisiko. (Siehe §4
  "Hardening"-Beobachtung.)

### 2.3 Betrieb — *gut abgedeckt, eine Lücke*
`inspect`/`stats`/`compact`/`gc` existieren und sind getestet (v1.0).
**Lücke:** keine Möglichkeit, die DB **wegzuspeichern/againherzustellen**
(Backup/Restore). Man kann inspizieren und warten, aber nicht sagen: *"Ich
nehme diesen DB-Ordner und stelle ihn woanders wieder her."*

### 2.4 Persistenz — *klar, mit dokumentiertem Durability-Vorbehalt*
- Format-Versionierung (v0.9) erkennt unbekannte/neuere Formate sauber
  (`UnsupportedFormatVersion`).
- Recovery-Atomicity gegeben.
- Durability = **deferred** (§2.2). Für v1 ausreichend, **wenn** die App
  flush/close verwendet; der Vertrag muss aber im Public API klar dokumentiert
  sein (ist es im Code-Kommentar, sollte ins Public-Doc).

### 2.5 Migration / Deployment — *größte Lücke*
- **Backup/Restore: nicht vorhanden** (kein `backup`/`restore`/`snapshot` im
  Code; das einzige `snapshot()` in `diag.rs` ist Zähler-Diagnose).
- **Verschieben** einer DB auf anderen Rechner/andere Platte ist nur als
  manueller Dateikopiervorgang möglich — **ohne Konsistenzgarantie** (siehe §3 A).
- **Format-Migration:** v0.9 erkennt nur, migriert nicht. Für langfristige
  Evolution irgendwann nötig, aber erst mit einer echten v2.

---

## §3 Kandidaten-Check (gegeneinander)

### A — Backup / Restore  ★ empfohlen
- **Größte operative + deployment-Lücke nach v1.0** (§2.3/§2.5): ein Nutzer
  kann die DB heute nicht sicher sichern, verschieben oder nach Medienausfall
  wiederherstellen. Das ist ein harter Blocker für "echter v1-Einsatz".
- **Im vorhandenen Modell sauber machbar:** Backup = `flush()` (aller Daten in
  SSTables) + konsistenter Kopiervorgang des gesamten Dateisatzes
  `{MANIFEST, SCHEMA, VERSION, wal.log, *.sst}` unter der ohnehin gegebenen
  In-Process-Exklusivität (`&mut self`). Restore = Dateien zurückkopieren
  (Ziel-DB muss geschlossen sein); Versions-Check via v0.9.
- **Limit (bewusst, wie `gc`):** kein Cross-Process-Lock. Konsistent nur bei
  Stillstand bzw. kooperierendem Single-Writer. Für Embedded-Nutzung (eine
  Prozess-Instanz) ist das akzeptabel und dokumentierbar.

### B — Conditional / Partial Update
- Echte API-Lücke (§2.1), aber im **Single-Writer-Modell funktional
  emulierbar** (get-check-put atomar). Liefert keine neue Korrektheitseigenschaft,
  die der Nutzer sonst nicht hätte. Daher **Convenience, kein Blockierer**.
- Erst dann relevant, wenn echte Mehrwriter-/Concurrency-Semantik dazukommt
  (dann aber eher Zweig D).

### C — Format-Migration
- v0.9 erkennt Versionen; Migration ist erst mit einer **realen v2** nötig.
  Heute kein Bedarf. Wird unvermeidbar, sobald das Format tatsächlich
  weiterentwickelt wird — dann als eigener Auftrag.

### D — Snapshot / Read-only Transactions (MVCC)
- Nur bei konkretem Concurrency-/Isolation-Bedarf. Das gewählte
  Single-Writer-Ziel für Embedded-Nutzung kommt ohne aus. **Nicht** starten
  ohne Use-Case.

### E — Query-Ausbau (Composite Index, Group-by, Joins, Textsuche)
- Kein konkreter Bedarf erkennbar. **Nicht** automatisch beginnen.

---

## §4 Zusätzliche Readiness-Beobachtung (kein Feature-Zweig)

**Robustheit/Hardening:** 438 `.unwrap()/.expect()` in `src` bedeuten, dass
I/O-Fehler im Betrieb häufig panic statt sauberem `Result` auslösen. Für einen
Produktionseinsatz als gelinkte Library ist das riskant. Das ist **kein**
Feature im Sinne von A–E, sondern eine querschnittliche Hartung. Es sollte als
eigener, separater Arbeitszweig ("Hardening: Fehlerpropagierung statt
panic") erfasst werden — aber es ist **nicht** der hier auszuwählende
Funktionszweig.

---

## §5 Empfehlung: genau ein Zweig → **A — Backup / Restore**

**Begründung (die Inventur entscheidet, nicht umgekehrt):**
1. Es ist die einzige Lücke, die einen **konkreten Nutzer am echten v1-Einsatz
   hindert** (kein Move/Restore/Medienausfall-Schutz möglich).
2. **B ist im Single-Writer-Modell emulierbar** → sekundär, keine
   Korrektheitslücke. C/D/E sind entweder verfrüht oder bedarfsfrei.
3. A ist im **bestehenden Architekturmodell sauber umsetzbar** (kein neues
   Nebenbau-Theme nötig), mit klar definierbaren Grenzen.

**Scope-Skizze (nur als Vorschlag, hier noch nicht implementiert):**
- `lsm-admin backup <dest>` bzw. `Database::backup(dest)`:
  `flush()` → konsistenter Kopiervorgang des gesamten Dateisatzes unter
  `&mut`-Exklusivität → optional Verifikation (Dateien + Größen + ggf.
  Versions-Check).
- `lsm-admin restore <src> <dest>` bzw. `Database::restore`: Dateien aus
  `<src>` nach `<dest>` kopieren; Ziel-DB muss geschlossen sein;
  Versionskompatibilität via v0.9 (`UnsupportedFormatVersion` bei zu neuer
  Quelle).
- **Bewusste Limitierung (dokumentiert):** kein Cross-Process-Lock; konsistent
  nur bei Stillstand/kooperierendem Single-Writer (identisch zu `gc`).

---

## §6 Nächster Schritt

1. Diese Inventur liegt als `design-phase-h-readiness.md` (untracked) vor.
2. Zweig **A — Backup/Restore** bestätigen (oder begründet korrigieren).
3. Bei Go-Ahead: wie gewohnt **erst Spezifikation (read-only)**, dann
   Implementierung + Regression + Volltest (warnungsfrei) + Checkpoint + Push.
4. **Hardening** (§4) als separaten, später möglichen Zweig mitführen, aber
   nicht mit A vermischen.
