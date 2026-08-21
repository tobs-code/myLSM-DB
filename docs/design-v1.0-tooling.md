# myLSM-DB — Design v1.0: Tooling / Operability

**Status:** Read-only Spezifikation + Inventur. **Kein Code** in diesem Schritt.
**Baseline:** `478cfc3` (v0.9, stabiler Checkpoint, `origin/main`).
**Ziel:** Die Datenbank wird nicht nur funktional, sondern **administrierbar**
(inspect / stats / manuelle Compaction / GC). Backup bewusst **nicht** im
ersten Scope (siehe §7 Gate).

**Reihenfolge:**
```
478cfc3 → Tooling-Inventur → CLI-/Admin-Spezifikation → Scope-Gate
        → Regression/Implementierung → Volltest → Checkpoint
```

---

## §1 Inventur (Ist-Stand)

### 1.1 CLI
- **Kein CLI vorhanden.** `Cargo.toml` deklariert nur die Library
  (`name = "my-lsm-db"`) und drei Benchmarks (`bench-v9*`). Kein `[[bin]]`,
  keine `main.rs`. v1.0 fügt eine schlanke Binärdatei hinzu.

### 1.2 Bereits vorhandene Admin-/Wartungs-Funktionen
| Funktion | Ort | Sichtbarkeit | Verhalten |
|----------|-----|--------------|-----------|
| `flush()` | `lib.rs` | `pub` | MemTable → L0-SSTable, Manifest-Commit, WAL-Truncate, ggf. Auto-`compact()` |
| `gc()` | `lib.rs:583` | `pub` | Löscht verwaiste `.sst`-Orphans (nicht im Manifest referenziert); nie Manifest-Änderung, nie referenced anfassen; `&mut self` |
| `compact()` | `lib.rs:464` | **privat** | L0→L1 Overlap-Merge, nur wenn `l0.len() >= l0_compact_threshold`; sonst `Ok(())` |
| `format_version()` | `lib.rs` | `pub` | v1 (bzw. 1 bei Legacy) |
| `table_count()` | `lib.rs` | `pub` | `manifest.all_ids().len()` (L0 + Segmente) |
| `level_count()` | `lib.rs` | `pub` | `manifest.levels.len()` |

### 1.3 Daten, die zuverlässig ermittelbar sind
- `Manifest` (Felder `pub`): `levels: Vec<Vec<u64>>`, `segments: Vec<SegmentMeta>`,
  `next_table_id: u64`. `SegmentMeta { file_id, min_key, max_key, records }`
  (alle `pub`). → Manifest-Struktur und Segment-Metadaten sind direkt lesbar.
- Pro Tabelle via `sstable::TableReader`: `num_records()`, `key_bounds()`
  (`Option<(&[u8], &[u8])>`). → **Record-Anzahl pro Tabelle zuverlässig**;
  Key-Range (Rohbytes) ebenfalls.
- `num_records()` zählt inkl. Tombstones. **Live-Key-Anzahl** (Tombstones
  abgezogen) wäre nur via vollem `iter()` ermittelbar — bei großen Tabellen
  teuer; daher als *optional* markiert (siehe §4.1).
- Dateigrößen: `MANIFEST`, `SCHEMA`, `VERSION`, `wal.log`, `*.sst` via
  `fs::metadata().len()` → DB-Größe und WAL-Größe zuverlässig.
- `Options`: `l0_compact_threshold`, `segment_max_records` → Compaction-Parameter
  im Inspect ausweisbar.
- **Table-Cache:** ist rein Laufzeit-State (`HashMap` im `Database`). In einer
  frisch geöffneten CLI-DB ist er **kalt/leer** → für Admin-Ausgaben nicht
  aussagekräftig; bewusst **nicht** in Stats aufnehmen (nur als Hinweis).

### 1.4 Lücke
Es fehlen: (a) ein CLI-Einstiegspunkt, (b) öffentliche Getter für
Manifest-Detail (Level-Inhalte, Segmentliste, `next_table_id`), Per-Table-Infos
(Record-Count, Key-Bounds, Dateigröße), WAL-Größe, DB-Größe, (c) eine
**öffentliche, erzwungene Full-Compaction** (heutiges `compact()` ist privat
und nur L0→L1).

---

## §2 Admin-Befehle (Scope v1.0)

Gemeinsam: alle Befehle öffnen die DB unter `<dir>` (Default `.`), führen die
Aktion aus, geben auf `stdout` aus. Fehler → `stderr` + Exit-Code ≠ 0 (§8).

### 2.1 `inspect`
Strukturelle Sicht, rein lesend (kein `close()`/`flush()` → keine
Nebenwirkung auf Platte).
- **Format-Version** (`format_version()`).
- **Manifest**: `next_table_id`, Anzahl Level, pro Level die Tabelle-IDs
  (Count + Liste), Segment-Count.
- **Tabellen/Segmente**: je Tabelle `file_id`, Level (L0 bzw. L1-Segment),
  Pfad, Record-Anzahl (`num_records()`), Key-Range (`key_bounds()` als Hex),
  Dateigröße.
- **Level**: pro Level Anzahl Tabellen + Summe der Dateigrößen.
- *Optional*: Live-Key-Count (nur auf expliziten Flag `--with-live-keys`,
  da teuer).

### 2.2 `stats`
Aggregierte Betriebskennzahlen (rein lesend):
- **DB-Größe** (Summe aller relevanten Dateien).
- **SSTable-Anzahl** (`table_count()`).
- **L0/L1-Verteilung**: Count + Bytes pro Level (L0 = `levels[0]`,
  L1 = `segments`).
- **WAL-Größe** (`wal.log`-Metadata).
- **Compaction-Parameter**: `l0_compact_threshold`, `segment_max_records`.

### 2.3 `compact` (manuelle, erzwungene Full-Compaction)
- **Semantik:** (1) zuerst `flush()`, damit die MemTable vollständig in
  SSTables liegt; (2) **vollständiger** Merge aller L0-Tabellen **und** aller
  L1-Segmente zu einer frischen L1-Segment-Menge (deterministischer Split nach
  `segment_max_records`); Tombstones werden physisch entfernt
  (`drop_tombstones = true`), da die komplette Historie jedes Keys im
  Merge-Set liegt (Invariante wie im bestehenden `compact()`).
- **Neu hinzuzufügende, öffentliche Funktion:** `Database::compact_full(&mut self)`
  (das bestehende private `compact()` bleibt als internem Auto-Trigger erhalten).
- **Durability-Vertrag:** Manifest-Commit via `save()` (fsync + atomarer
  rename) erfolgt **vor** dem Löschen der alten SSTables (Crash-Fenster §13
  der Design-Spez). Ein Crash während `compact_full` lässt die alten, vom alten
  Manifest referenzierten SSTables intakt → DB bleibt konsistent/öffbar.
- **Fehlervertrag:** Scheitert das Einlesen einer manifestierten Tabelle
  (`merge_ids` → `TableReader::open`/`iter` Fehler, z. B. korrupte Datei),
  bricht `compact_full` ab, es wird **nichts gelöscht**, das Manifest bleibt
  unverändert. Rückgabe: `Result<()>` (ggf. Anzahl geschriebener Segmente).
- Requirement: `&mut self` (In-Process exklusiv; kein Cross-Process-Lock — siehe §6).

### 2.4 `gc` (administrativ erreichbar machen)
- Macht das bestehende `Database::gc()` über den CLI-Befehl aufrufbar.
- Kontrakt (bereits implementiert, hier nur gebündelt): löscht ausschließlich
  `.sst`-Orphans, deren `file_id` **nicht** im Manifest steht; nie referenzierte
  Dateien, nie Manifest-Änderung; `.manifest.tmp` wird gefahrlos entfernt.
- Rückgabe: Anzahl gelöschter Orphans (auf `stdout`).
- Requirement: `&mut self`.

### 2.5 (Optional / bewusst zurückgestellt) `dump` / `export`
- Ein vollständiger Key/Value-Dump wäre primär für **Migration** oder
  tiefes Debugging nützlich. Da v0.9 bewusst **keine Migration** einführt und
  ein Dump erheblichen Mehraufwand (Decodierung aller Werte, Schema-Mapping)
  bedeutet, ist er **nicht Teil des ersten v1.0-Scopes**. `inspect` deckt die
  strukturelle Einsicht bereits ab. Empfehlung: nur aufnehmen, wenn ein
  konkreter Debug-/Migrationsbedarf auftritt (dann als separater Punkt).

### 2.6 (Bewusst zurückgestellt) `backup`
- **Nicht im ersten CLI-Scope.** Begründung: Snapshot-Konsistenz, WAL,
  laufende Writes und Crash-Verhalten sind semantisch deutlich
  anspruchsvoller als `inspect`/`gc`. Ein naiv kopiertes Verzeichnis kann
  mitten in einem Manifest/SSTable-Schreibvorgang enden → beim Restore
  referenziert das Manifest eine partial geschriebene Tabelle → `Corrupt`.
- **Design-Raum (für späteren eigenen Auftrag):** konsistentes Backup
  erfordert einen quieszierten Punkt — entweder (a) DB geschlossen, (b) nach
  `flush()` + `fsync` aller Dateien + atomarem Kopiervorgang des gesamten
  Satzes `{MANIFEST, SCHEMA, VERSION, wal.log, *.sst}` unter Stillstand, oder
  (c) filesystem-Ebene Snapshot. Cross-Process-Lock ist heute nicht
  implementiert. Dies wird als **eigener Produktauftrag** geführt, nicht als
  Teil von v1.0.

---

## §3 CLI-Gestaltung

- Binärdatei: `src/bin/lsm-admin.rs` (neu), registriert via `[[bin]]` in
  `Cargo.toml` (Name z. B. `lsm-admin`).
- Aufruf: `lsm-admin <command> [--dir <pfad>] [flags]`.
  - `lsm-admin inspect [--dir .] [--with-live-keys]`
  - `lsm-admin stats [--dir .]`
  - `lsm-admin compact [--dir .]`
  - `lsm-admin gc [--dir .]`
- Ausgabe: Standard = menschenlesbarer Text. JSON (`--json`) als später
  optionaler Zusatz (nicht im ersten Scope).
- Exit-Codes: `0` = ok, `1` = Laufzeitfehler (geöffnet/operiert), `2` =
  Aufruf-/Argumentfehler.

---

## §4 Nötige neue Zugriffsfunktionen (Getter auf `Database`)

Alle **lesend**, damit `inspect`/`stats` ohne Mutation auskommen:
- `manifest_summary() -> ManifestSummary` (Snapshot): `next_table_id`,
  `levels: Vec<Vec<u64>>` (Kopie), `segments: Vec<SegmentInfo>` (file_id,
  min_key, max_key, records als Kopie), `level_count`.
- `table_infos() -> Vec<TableInfo>`: je Tabelle `id`, `level`, Pfad,
  `num_records`, `key_bounds_hex`, Dateigröße. (Öffnet pro Tabelle einen
  `TableReader` — zuverlässig, aber I/O; für `inspect` akzeptabel.)
- `wal_size() -> u64` (Dateigröße `wal.log`, 0 falls nicht vorhanden).
- `db_size() -> u64` (Summe relevanter Dateien).
- `options() -> OptionsView` (Kopie von `l0_compact_threshold`,
  `segment_max_records`).
- `compact_full(&mut self)` (siehe §2.3).

Es werden **keine** bestehenden On-Disk-Formate verändert; ausschließlich
getter + ein neuer (privat aufbauender) Compaction-Einstiegspunkt.

---

## §5 Fehler-/Exit-Vertrag

- `open()` kann mit `UnsupportedFormatVersion` / `InvalidFormat` / `Corrupt`
  fehlschlagen (v0.9-Vertrag) → CLI bricht mit Exit `1` + Meldung ab.
- `compact`/`gc` Mutationen: Fehler werden propagiert; bei `compact`-Abbruch
  bleibt die DB konsistent (§2.3). `gc` löscht nie referenzierte Dateien.
- Ungültige Argumente → Exit `2`.
- Keine partiellen Schreibvorgänge bei `inspect`/`stats` (rein lesend).

---

## §6 Grenzen (bewusst)

- **Kein Cross-Process-Locking.** `&mut self` garantiert nur In-Process-
  Exklusivität. Ein zweiter Prozess kann dieselbe DB parallel öffnen. Für den
  Admin-CLI (Wartung im Stillstand) ist das akzeptabel; ein echtes
  Lock-Protokoll ist **kein** v1.0-Scope.
- Backup (§2.6) und Dump (§2.5) sind bewusst **nicht** in v1.0.

---

## §7 Scope-Gate (Empfehlung)

**v1.0 erster Scope:**
```
inspect  +  stats  +  compact (manuell, full)  +  gc
```
plus die nötigen Getter (§4) und die neue Binärdatei (§3).

**Bewusst zurückgestellt (eigene Aufträge, nicht v1.0):**
- `backup` — semantisch anspruchsvoller (Snapshot/WAL/Crash); eigener Punkt.
- `dump`/`export` — nur bei konkretem Debug-/Migrationsbedarf.

**Keine Nebenbaustellen:** keine MVCC-, Sharding-, Replikations- oder
Performance-Arbeit in v1.

---

## §8 Regression (bei Implementierungs-Go-Ahead)

Mindestens:
1. `inspect`/`stats` auf einer frisch erzeugten DB liefert konsistente Zahlen
   (table_count == stats SSTable-Anzahl; Summe Level-Counts == table_count).
2. `inspect`/`stats` sind rein lesend: danach keine neuen/veränderten Dateien
   außer ggf. `VERSION` bei Legacy-Öffnung (v0.9-Verhalten, unvermeidbar).
3. `compact` nach vielen Writes + deletes reduziert SSTable-Anzahl und entfernt
   Tombstones (Live-Key-Count sinkt bzw. Gesamt-Records sinken); DB danach
   weiter öffbar + inhaltsgleich (Oracle-Vergleich der Entities).
4. `compact` ist crash-sicher: simulierter Abbruch (oder manuell kopierte,
   partial geschriebene Tabelle + altes Manifest) lässt DB öffbar.
5. `gc` entfernt künstlich erzeugte Orphan-`.sst`, lässt referenzierte
   unangetastet; Anzahl in Ausgabe korrekt.
6. `gc`/`compact` auf beschädigter (aber versionierungskorrekter) DB →
   saubere Fehlermeldung, keine stillen Datenverluste.

---

## §9 Nächster Schritt

1. Diese Spec liegt als `design-v1.0-tooling.md` (untracked) vor.
2. Scope-Gate bestätigen (§7): inspect + stats + compact + gc; backup/dump
   zurückgestellt.
3. Bei Go-Ahead: Getter (§4) + `compact_full` + Binärdatei (§3) + Tests (§8) +
   Volltest (`cargo test --release`, warnungsfrei) + Checkpoint + Push.

Bis dahin: **kein Code**, nur Dokumentation.
