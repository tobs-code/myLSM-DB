# myLSM-DB — Design v1.1: Backup / Restore (Phase H, Zweig A)

**Status:** Spezifikation + Regression-Fixtures (read-only Artefakt).
**Baseline:** `5daa675` (v1.0, stabil). Erst nach grünem Zyklus: Checkpoint + Push.
**Ziel:** Eine konsistente, eigenständige Datenbankkopie, die mit `open()`
wiederhergestellt werden kann und denselben committed Zustand wie die Quelle
zum Backup-Commit-Punkt repräsentiert.

---

## §1 Vertrag (vom Auftraggeber präzisiert)

1. **Backup startet exklusiv über `&mut self`.**
2. Pending Direct-Writes werden zunächst durch `flush()` persistent gemacht.
3. Danach werden **nur die zum committed Manifest gehörenden Dateien** plus
   notwendige Metadaten kopiert.
4. Ein Backup darf niemals einen halbfertigen Zustand als erfolgreich melden.
5. Die Zielkopie muss unabhängig von der Quelldatenbank geöffnet werden können.
6. **Keine Cross-Process-Synchronisation** (bewusst außerhalb des Vertrags).
7. Restore arbeitet auf einer **geschlossenen Ziel-DB bzw. einem leeren
   Zielpfad**.
8. Versionsprüfung erfolgt über den bestehenden v0.9-Mechanismus.
9. Kein automatisches Überschreiben einer bestehenden Datenbank ohne
   explizite Semantik.

---

## §2 Backup-Root (expliziter Dateisatz)

```
Backup-Root
├── VERSION        (nur wenn in der Quelle vorhanden; Legacy → entfällt)
├── MANIFEST       (committed)
├── SCHEMA         (nur wenn vorhanden)
└── committed *.sst (genau die vom Manifest referenzierten Tabellen)
```

**Bewusst NICHT kopiert:** `wal.log`, `*.tmp` (insb. `.manifest.tmp`),
Orphan-`.sst`. Begründung: Nach einem erfolgreichen `flush()` ist der
committed Zustand vollständig in SSTables + Manifest; der WAL ist kein
Bestandteil des logischen Zustands. **Orphan-Garbage ist kein Teil des
logischen DB-Zustands** (daher darf `gc` vorher/innerhalb keinen Einfluss auf
den Backup-Inhalt haben — der Backup-Root enthält per Definition nur referenzierte
`.sst`).

---

## §3 Implementierungsvertrag (Skizze, kein Code in diesem Schritt)

- `Database::backup(&mut self, dest: &Path) -> Result<usize>`:
  `flush()` → Ziel vorbereiten (nicht existent oder leer, sonst
  `InvalidArgument`) → kopiere den Backup-Root (nur referenzierte `.sst` via
  `manifest.all_ids()`) → bei Copy-Fehler: kopierte Dateien + Zielverzeichnis
  best-effort entfernen, dann `Err` (kein halbfertiger Erfolg). Rückgabe:
  Anzahl kopierter Dateien.
- `Database::restore(src: &Path, dest: &Path) -> Result<usize>` (assoziiert):
  Versionsprüfung via `version::read_version` + `check_compatible` (Punkt 8)
  → MANIFEST im `src` erforderlich (`InvalidFormat` sonst) → Ziel vorbereiten
  (leer/nicht existent; vorhandene DB-Dateien → `InvalidArgument`, keine
  stille Überschreibung, Punkt 7/9) → kopiere Backup-Root aus `src` (VERSION,
  MANIFEST, SCHEMA, referenzierte `.sst`) → Cleanup bei Fehler. Rückgabe:
  Anzahl kopierter Dateien.
- `EntityStore` erhält delegierende `backup`/`restore` für ergonomische
  Anwendungsnutzung.
- CLI: `lsm-admin backup [--dir <db>] <dest>` und
  `lsm-admin restore <src> <dest>`.

---

## §4 Regression-Fixtures (erst definiert, dann Implementierung)

| # | Test                                             | Erwartung                                        |
|---| ------------------------------------------------ | ------------------------------------------------ |
| 1 | Backup einer leeren DB                           | Restore öffnet                                   |
| 2 | Backup mit KV-/Entity-Daten                      | alle Daten identisch                             |
| 3 | Backup mit Entities + Index                      | Index-Queries identisch                          |
| 4 | Backup nach Compaction                           | identisch                                        |
| 5 | Backup mit Pending Direct-Writes                 | `backup()` macht sie gemäß Vertrag persistent    |
| 6 | Backup enthält keine Orphans                     | Restore bleibt sauber (Orphan nicht kopiert)     |
| 7 | Restore in existierenden Zielpfad                | definierter Fehler, kein stilles Überschreiben  |
| 8 | beschädigtes/unvollständiges Backup              | `open()` scheitert, kein „halb gültiger" Restore |
| 9 | inkompatible `VERSION` im Backup                 | `UnsupportedFormatVersion`                       |
|10 | Restore → neue DB → Write/Flush/Compact          | vollständig funktionsfähig                       |

---

## §5 Scope-Gate (ausdrücklich NICHT in v1)

Online-Backup während paralleler Prozesse · Snapshot/MVCC · inkrementelle
Backups · Point-in-Time-Recovery · Remote/Object-Storage · Kompression ·
Verschlüsselung · automatische Backup-Rotation · Migration zwischen
Formatversionen.

`5daa675` bleibt Baseline bis der gesamte Zyklus (Spec → Implementierung →
Regression → Volltest warnungsfrei → Checkpoint) grün ist.
