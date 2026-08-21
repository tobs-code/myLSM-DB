# Changelog

Alle erwähnenswerten Änderungen an `my-lsm-db` werden hier dokumentiert.
Format angelehnt an [Keep a Changelog](https://keepachangelog.com/).

## [1.3.0] – 2026-08-21

Erste als Release-Kandidat (`v1.3.0-rc`) und stabile v1.3-Baseline
gekennzeichnete Version. Fokus dieser Version ist **Release-Hardening**
(Packaging, Dokumentation, CI, Beispiele, Recovery-Test) — keine neuen
DB-Funktionen gegenüber dem v1.3-Entwicklungsstand.

### Added
- Typisierte Entity-Layer (`EntityStore`, `Entity`, `CollectionHandle`).
- Composite-Indexe (`create_composite_index` / `find_composite`).
- Optimistische Nebenläufigkeit: `cas_update` mit `Expected`/`Patch`.
- Transaktionen mit Read-your-own-writes (`Transaction`, atomarer WAL-Commit).
- Query-Builder mit Filter/Sort/Limit/Project und Aggregationen
  (`Count`/`Sum`/`Avg`/`Min`/`Max`).
- `explain_query`: liefert den Physical Plan als Text (keine Ausführung).
- Dateibasiertes Backup/Restore (`backup` / `EntityStore::restore`).
- Administratives CLI `lsm-admin` (inspect / stats / compact / gc).
- Format-Versionierung (`VERSION`-Datei, `FORMAT_VERSION = 1`).

### Stability
- Public API ab v1.3 als stabil gekennzeichnet. Die Low-Level-KV-Engine
  (`Database`) bleibt öffentlich, wird aber ausdrücklich als
  „nicht die empfohlene Anwendungs-API“ dokumentiert.

## Compatibility Policy

- **On-Disk-Format-Version:** `1` (`FORMAT_VERSION`).
- **v1.x lesbar:** Jeder v1.x-Binary liest Datenbanken, die von einem
  beliebigen v1.x- oder Legacy-v1-Binary geschrieben wurden. Beim Öffnen ohne
  `VERSION`-Datei wird die DB als Format `1` behandelt (Migration-by-read).
- **Format 2 (zukünftig):** Ein neuer Format-Zweig wird **nicht** lesend
  unterstützt. Beim Öffnen einer höheren/unpassenden Version liefert die
  Engine `UnsupportedFormatVersion`
  ([`error::Error::UnsupportedFormatVersion`](src/error.rs)) und die DB bleibt
  unverändert.
- **Migration:** Es gibt (noch) kein automatisches Upgrade. Ein künftiges
  Format 2 erfordert ein **explizites, separates Migrationswerkzeug**; die
  Engine selbst schreibt keine stillen Upgrades.
- **Backup/Restore:** `restore` prüft die Version des Backup-Roots und weist
  inkompatible Versionen ebenfalls mit `UnsupportedFormatVersion` ab.

[1.3.0]: https://github.com/tobs-code/myLSM-DB/releases/tag/v1.3.0
