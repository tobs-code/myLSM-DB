# design-v0.7-write-cache.md — Old-Value-Cache für den Warm-Put

Status: **Entwurf / Prototyp-Skizze. Kein Code.**

## 1. Ausgangslage (kausal belegt)

A/B-Zerlegung des Warm-Puts (100k, 300k Puts):

| Messung | A (alles in MemTable) | B (Flush+Compaction) |
|---|---|---|
| Gesamt | 13.6 µs/put | 139.2 µs/put |
| `get_us` (Point-Lookups) | **0.04 s** | **30.6 s (~102 µs/put)** |
| `flush_us` + `compact_us` | 0 | 10.4 s |
| `wal_us` / `memtable_us` / `hint` / `fieldenc` | klein | klein |

Fazit: Der Engpass sind die **Point-Lookup-Disk-Reads** (`get_us`), die beim
Warm-Put die **alten Indexfeld-Werte** für den Index-Diff aus SSTables lesen.
Nicht Tabellenanzahl, nicht Flush/Compaction. `field_hint` (5a) beseitigt bereits
den Range-Scan (Feld-Satz), aber nicht die Wert-Lookups.

## 2. Ziel

`get_us` im Warm-Pfad von **30.6 s → ~0**. Damit fällt der 100k-Warm-Workload
von **41.8 s** auf den **Storage-Floor ≈ 11 s** (Flush + Compaction bleiben als
separat adressierbare Restkosten).

## 3. Variante A — Old-Value-Cache / Write-Set (bevorzugt)

### 3.1 Was wird gecacht?

- **Nur die Werte indexierter Felder** pro Entity, die für den Index-Diff
  gebraucht werden (in `core_put_entity`: die `m.get`-Loops 307/322).
- Pro `(entity)` eine Map `field_id -> Value`.
- Der vorhandene **Feld-Satz** (`field_hint`) bleibt unverändert (beseitigt den
  Range-Scan); der Wert-Cache ergänzt ihn um die **Werte** (beseitigt die
  Point-Lookups).

### 3.2 Write-Through, nie Persistenz-Wahrheit

Nach erfolgreichem `put_entity` werden die geschriebenen indexierten Feldwerte
**write-through** in den Cache geschrieben (analog zum bestehenden
`field_hint.insert(..., new_ids)` bei entity.rs:428). Damit sind sie beim
nächsten Update die "alten Werte".

Der Cache ist **Performance-Hint, nie Persistenz-Wahrheit**: Nach Reopen ist er
leer, die Semantik bleibt (erster Zugriff = Storage-Lookup/Cold-Path, danach
wieder warm).

### 3.3 Lebensdauer

- Nur während einer offenen `EntityStore`-Instanz.
- **Flush-/Compaction-unabhängig**: Der Cache liest nie vom Dateisystem, er wird
  nur von Puts befüllt. Flush/Compaction ändern die logischen Werte nicht, also
  bleibt der Cache gültig.
- Reopen -> leer (korrekter Fallback).

### 3.4 Invalidierung

| Ereignis | Aktion |
|---|---|
| `delete_entity` | Entity aus Cache entfernen |
| Transaktions-Commit | betroffene Entities neu warm (Werte neu schreiben) |
| Transaktions-Rollback/Abort | betroffene Entities invalidieren (Cache darf sonst nie-committete Werte halten) |
| `create_index` auf neuem Feld | bestehende Entities haben dort evtl. keinen gecachten Wert -> **Cache-Miss-Fallback** (Point-Lookup), dann befüllen |
| `drop_index` | Werte des Feldes fallen aus der Nutzung; Einträge optional aufräumen |
| Eviction (Budget) | ganze Entity verwerfen -> nächster Put cold/partial (korrekt) |

### 3.5 Memory-Budget (wichtig bei 10M Entities)

Die Werte **aller** Entities dauerhaft zu halten wäre nicht tragbar
(≈ 10M × ~2 indexierte Felder × ~16–24 Byte ≈ 400–500 MB). Deshalb:

- **Begrenzter Cache** mit konfigurierbarer Kapazität (Anzahl Entities oder
  Byte-Budget), **LRU-Eviction pro Entity**.
- Bei Eviction wird die gesamte Entity-Hint verworfen -> nächster Put fällt auf
  den (korrekten) Cold-/Partial-Pfad zurück.
- Hot/Warm-Workloads (genau die, die von Updates profitieren) treffen den Cache;
  Kaltdaten werden evictet. Das ist ein kontrollierter Kompromiss zwischen
  Speicher und Warm-Trefferquote.
- Option: Werte NUR für Felder cachen, die aktuell indexiert sind (imputations­
  genau); nicht-indexierte Werte nie halten.

### 3.6 Fallback (Cache-Miss)

```text
cache miss -> normaler Point-Lookup (bestehender Pfad)
            -> optional den Wert danach cachen
```

Der Warm-Pfad bleibt also korrekt, wenn der Cache leer/kalt/evictet ist.

### 3.7 Umsetzung in `core_put_entity`

- Ersetze die beiden `m.get`-Loops (307–322) durch: erst Cache prüfen; bei
  Miss den bestehenden Point-Lookup, Ergebnis danach in den Cache.
- `stale_keys`-Bestimmung bleibt (braucht nur den Feld-Satz, keinen Wert).
- Nach dem Schreiben: neue indexierte Werte write-through in den Cache.
- Signatur: `hint` erweitern zu `Option<&mut HashMap<(u32, Vec<u8>), EntityHint>>`
  mit `EntityHint { fields: HashSet<u32>, values: HashMap<u32, Value> }`.

## 4. Variante B — Index-Diff ohne Old-Value-Read (verworfen)

Statt "alten Wert lesen -> alten Index-Key -> neuen Index-Key -> Diff" den
**Indexzustand selbst** als Quelle für das Delta verwenden.

**Abgelehnt**, weil:
- Der Index ist ausdrücklich **keine Source of Truth** (Konvention der Engine).
- Inkonsistenzen im Index (Crash, partieller Rebuild) würden in den Write-Pfad
  einfließen und Korrektheit gefährden.
- Der Index wird beim `drop_index` entfernt; seine Nutzung für Puts wäre dann
  unmöglich, ohne die Feld-Werte trotzdem zu lesen.

**B ist semantisch deutlich riskanter bei geringem Vorteil gegenüber A.**

## 5. Korrektheits-Gates (vorgeschlagene Tests)

1. **Warm == Cold**: für beliebige Sequenz von Puts/Deletes/Updates liefern
   kalter und warmer Pfad identische Resultate (Oracle).
2. **Reopen leert den Cache, nicht die Semantik**: nach Reopen sind alle Werte
   korrekt (erster Zugriff cold, danach warm).
3. **Update -> Update**
4. **Update -> Delete -> Reinsert**
5. **Index hinzufügen / entfernen** mitten in der Sequenz.
6. **Flush zwischen Updates**.
7. **Compaction zwischen Updates**.
8. **Transaktions-Rollback** darf keine gecachten Werte hinterlassen, die einen
   nie-committeten Zustand widerspiegeln.
9. **Crash / Recovery**.

## 6. Messplan (nach Implementierung, gleiche Matrix)

- Warm 100k: `get_us` **30.6 s → ~0** (primäres Gate).
- Warm 100k wall-clock: **41.8 s → ~11 s** (Storage-Floor; Flush+Compaction
  bleiben, separat messbar via `flush_us`/`compact_us`).
- Kein Speedup am Warm-Pfad, wenn `get_us` nicht sinkt -> Ansatz verworfen.
- Memory: Cache-Größe reporten; Budget-Trefferquote messen.

### 6.1 Gemessene Ergebnisse (Prototyp, 100k warm, Case B Flush+Compaction)

| Metrik | ohne Cache | mit Cache | Δ |
|---|---|---|---|
| `get_us` | 30.6 s | **6.85 s** | −77 % |
| Warm-Put wall | 41.77 s (139 µs/put) | **16.24 s (54.1 µs/put)** | **2.6×** |
| `cache_hits` / `cache_misses` | – | **600 000 / 0** | – |

**Befund:** Der Cache für **indexierte Old Values** beseitigt den Wert-Lookup
praktisch vollständig (600k Hits, 0 Misses). Der verbleibende Read-Anteil
(6.85 s `get_us`) entsteht durch **konservative Existenzprüfungen für stale,
nicht-indexierte Felder** (bei jedem Feld-Removal: `active`/`seen` in Runde
1/2), die bewusst nicht gecacht werden, damit `field_hint` nie zur
Wahrheitsquelle wird.

**Bewusste Nicht-Erweiterung:** Der Cache wird **nicht** auf alle Feldwerte
erweitert. Das würde einen zweiten, materialisierten Entity-State aufbauen
(mit Konsistenz-, Eviction-, Delete/Tx-, Crash-, Reopen-, Partial-Update- und
10M-Entity-Speicherfragen) — der Einstieg in die verworfenen
persistente/materialisierte Entity-State-Architektur. Der Cache hat einen eng
umrissenen Zweck: **den Index-Diff beschleunigen.**

**Nächster Hebel:** verbleibende ~9.3 s (Flush ~5.05 s + Compaction ~4.23 s)
als separates Diagnose-/Optimierungsthema; keine weitere Entity-Cache-Komplexität.

## 7. Scope / Abgrenzung

- **Kein** neues Manifestformat, kein Leveled-LSM, keine Async-Compaction.
- Flush/Compaction-Kosten (die restlichen ~11 s) sind **nicht** Gegenstand
  dieses Entwurfs; sie bleiben separat adressierbar.
- 1d-Compaction bleibt funktional (bounded L0), wird aber für den Warm-Put als
  nicht ursächlich betrachtet.