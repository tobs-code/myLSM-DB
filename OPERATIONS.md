# Operations

Operative Regeln und bekannte Einsatzgrenzen für `my_lsm_db` (gültig ab Checkpoint `911c4c7` / v1.3).

Dieses Dokument beschreibt **Betrieb und Grenzen**, nicht die Architektur. Für
Architektur/Spezifikation siehe `design-v1.3-composite-index.md`.

## Single-Handle-Regel

Im Single-Process-Modell darf es **höchstens einen aktiven `EntityStore` pro
DB-Verzeichnis** geben. Ein zweiter `EntityStore::open` auf dasselbe Verzeichnis,
während der erste Store noch lebt, sieht dessen WAL-only-Daten (noch nicht ins
SSTable geflushte Schreiben) nicht.

Konsequenz: einen Store pro Prozess öffnen, sauber mit `close()` beenden, bevor
das Verzeichnis woanders neu geöffnet wird.

## Durability

- **Transaktionale Writes** (`store.transaction()` … `commit()`) sind nach dem
  Commit durable (WAL + fsync).
- **Direct Writes** (`CollectionHandle::put` / `cas_update` / `delete`) sind
  **deferred-durable**: sie sind erst nach `flush()`, `close()` oder `backup()`
  persistiert.

Persistenzgrenzen für Direct Writes sind also explizit `flush()` / `close()` /
`backup()` — nicht der einzelne `put`-Aufruf.

## Bulk-Load

Die Indexpflege (Secondary- und Composite-Indizes) erfolgt **synchron pro
Entity** während des Schreibens. Große Bulk-Loads mit vielen Indexstrukturen
können dadurch spürbar langsam werden (im Phase-K-Szenario ~49 s für 60k
Entities, >10 min für 600k). Bewusste, aktuell nicht optimierte Eigenschaft.

## Backup / Restore

Sichere Betriebsweise:

1. `store.backup(target_dir)` erstellt einen konsistenten Snapshot.
2. Weiterarbeiten auf der Originalsource verfälscht den Backup-Snapshot nicht.
3. `EntityStore::restore(backup_dir, restore_dir)` materialisiert den Snapshot in
   ein neues Verzeichnis; dieses anschließend mit `open` nutzen.

Backup ist lokal (Verzeichnis-Snapshot). Remote-Backup ist Host/Infra-Sache
(nicht Teil der Library).

## Concurrency-Grenze

`my_lsm_db` bietet **kein Cross-Process-Locking und kein Multi-Writer-Versprechen**.
Nebenläufigkeit ist auf einen aktiven Writer pro Verzeichnis (Single Process)
ausgelegt. Read-only-Konkurrenz oder Multi-Writer über Prozessgrenzen hinweg
werden nicht unterstützt.

## Backlog

- **`Bulk Index Build`** (batched / background Indexpflege für Bulk-Load):
  **C-Kandidat** (Performance/Produktoptimierung, keine Correctness-Regression).
  Ausdrücklich **keine zugesagte v1.4-Arbeit**. Trigger für eine Bearbeitung ist
  ein nachgewiesener realer Anwendungsbedarf, nicht der synthetische Phase-K-Lauf.
