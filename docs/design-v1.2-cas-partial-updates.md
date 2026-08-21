# myLSM-DB — Design v1.2: CAS + Partial Entity Updates (read-only Spec)

**Status:** Read-only Spezifikation (Produkt-Spec-Zyklus). **Kein Code-Wandel.**
`74f8a2f` (v1.1) bleibt eingefrorene Produktbaseline.
**Nächster Schritt danach:** bei Freigabe separater Implementierungs-Zyklus.

---

## §0 Warum v1.2 zuerst (Produktentscheidung)

Von den vier Kandidaten (Snapshots/MVCC, Composite Indexes, Remote Backup,
CAS+Partial Updates) ist CAS+Partial der einzige, der die Entity-API
**unmittelbar** erweitert, ohne Storage-/Transaktions-Semantik aufzureißen:
er nutzt die vorhandene Single-Writer-`Transaction` und den `Mutator`-Vertrag
und fügt nur eine *Conditional-Mutation-Semantik* hinzu.

---

## §1 Befund: bestehender Entity-/Transaction-Vertrag (Forensik)

- **`Entity`** = `fields: Vec<(String, Value)>` (`src/entity.rs:30-48`).
  **Kein Version-/ETag-Feld.** Ein „Version Counter" existiert heute nicht.
- **Mutation** läuft über `core_put_entity` (`src/entity.rs:268`), das bereits
  die Index-Schreibreihenfolge (`PUT neuer Index → PUT Entity → DELETE alter
  Index`, siehe `src/index.rs:16-19`) und `field_hint`-Invalidierung pflegt.
- **Einheitliche Mutations-Sicht** = Trait `Mutator { get, scan, put, delete }`
  (`src/lib.rs:1119`), implementiert von `DirectMutator` (nicht-transaktional)
  und `TxMutator` (transaktional, Pending-Overlay mit Read-your-own-writes).
- **Atomarität** = `Transaction::commit` (`src/entity.rs:1047`): schreibt
  `wal_begin → wal_tx_put/wal_tx_delete → wal_commit → wal_sync` (COMMIT
  POINT) und wendet danach auf die MemTable an. Crash vor dem Point = nichts
  dauerhaft; danach = durable.
- **Single-Writer**: `Transaction` lehnt sich an `&mut EntityStore`
  (`src/entity.rs:120-122`) — nur eine Transaktion gleichzeitig, keine
  Concurrency. Das ist der Rahmen, in dem CAS operiert.

**Konsequenz für den Spec:** CAS+Partial muss **nicht** als neuer Mechanismus
erfunden werden. Er lässt sich als Operation über den `Mutator` ausdrücken:
`read(current) → verify(expected) → apply(patch) → core_put_entity(new)`.
Dadurch erben Direkt-CAS und CAS-in-Transaction automatisch dieselbe
Index-/Atomar-/Crash-Semantik.

---

## §2 Entwurfsentscheidungen (die 7 Klärpunkte)

### 1. Vergleichsbasis (`expected`)

Vorhandene Architektur liefert **keinen** Version-Counter. Empfehlung:
CAS vergleicht gegen den **Wertzustand**, nicht gegen eine künstliche
Version. `Expected` wird ein Enum:

```text
Expected =
  | Entity(Entity)        // voller Entity-Wert muss exakt (feldsatz + Werte) matchen
  | Field(&str, Value)    // einzelnes Feld muss den Wert haben
  | Absent                // nur wenn die Entity (noch) nicht existiert
  | Any                   // unconditional (dann ist es rein ein Partial-Update)
```

- **Empfehlung:** Primär `Entity` + `Absent` (`insert-if-absent`). `Field`
  als Komfort-Variante. `Any` deckt „reines Partial Update ohne Bedingung".
- **Version-Counter ist bewusst NICHT der Default.** Er würde ein neues
  `Entity`-Version-Feld + Schema-Bump + Migrationspfad erzwingen (siehe §10).
  Wird hier als mögliche *spätere* Erweiterung geführt, nicht als v1.2-Basis.

### 2. Bedeutung von Partial Update (`patch`)

`Patch` als Feld-Operationen (kein Schema-Umbau):

```text
Patch  = Set(&str, Value)        // Feld setzen/überschreiben
       | Remove(&str)            // Feld entfernen (→ absent)
       | Increment(&str, Value)  // numerisch: Int/Float um Delta erhöhen
```

- `Value::Null` (present, aber null) ist **verschieden** von `Remove`
  (absent) — beide müssen sauber getrennt bleiben (Oracle-Edge).
- Schema-**strukturelle** Änderungen (Collections/Felder anlegen/löschen)
  bleiben außerhalb von `Patch`; sie laufen über die bestehende
  Schema-Registry. `Patch` ändert nur Feldwerte existierender Entities.

### 3. CAS-Mismatch → eigener Fehler

Neuer Variant: `Error::Conflict { collection_id, entity_id, reason }`
(statt `InvalidState`/`InvalidArgument`). `reason` unterscheidet
`ExpectedAbsentButExists` / `ExpectedValueMismatch` / `ExpectedFieldMismatch`.
Der Fehler trägt **keinen** Kopie des aktuellen Werts (kein Leak; ein
optionales `current_hash` nur zur Diagnose).

### 4. Verhalten mit Transactions

- **Direct CAS** (`EntityStore::cas_update`): intern ein **einzelner
  WAL-Transaction-Block** (Begin→TxPut→Commit→fsync) — atomar wie ein
  normaler `commit`.
- **CAS innerhalb einer bestehenden `Transaction`**
  (`Transaction::cas_update`): selbe Logik über `TxMutator`; der Vergleich
  sieht committete **und** eigene uncommittete Writes
  (Read-your-own-writes), das Ergebnis landet im Pending-Puffer und wird mit
  dem Transaktions-`commit` festgeschrieben.
- **Beide** rufen dieselbe interne `core_cas_entity(mutator, expected, patch)`
  auf → **identische Semantik**, ein einziger Implementierungsort.

### 5. Index-Konsistenz

`core_cas_entity` baut die neue Entity durch `apply(patch, current)` und
ruft dann das **bestehende** `core_put_entity(new)` auf demselben `Mutator`
(src/entity.rs:268). Damit gilt automatisch die etablierte
Index-Schreibreihenfolge (§1) — auch für `Increment`/`Set` auf indexierten
Feldern. Kein neuer Index-Pfad.

### 6. Crash-/WAL-Semantik

Da CAS ein WAL-Transaction-Block ist (§4), entsteht **kein Zwischenzustand**:
- Crash vor COMMIT POINT → nichts angewandt, Entity unverändert.
- Crash nach COMMIT POINT → Mutation durable; MemTable-Apply ist best-effort
  (Durability bleibt über WAL erhalten, wie bei `Transaction::commit`).

### 7. Oracle / Testplan (Referenzimplementierung)

`read → verify → apply` als Oracle, analog zum v0.8-Query-Oracle:

- zufällige `Patch`-Folgen auf existierende Entities,
- CAS-Hits (expected korrekt) und CAS-Misses (expected falsch → `Conflict`),
- indexierte **und** nicht-indexierte Felder (jeweils `Set`/`Increment`/
  `Remove`),
- `Delete`/Reinsert (Entity verschwindet, taucht mit anderem Wert wieder auf),
- `Null` vs. `absent` (Removal liefert `absent`; `Set(Null)` liefert
  present-null),
- Direct CAS vs. CAS-in-Transaction liefern nach Commit **identisches**
  Ergebnis (dieselbe `core_cas_entity`).

---

## §3 Architektur-Integrationsskizze (kein Code in diesem Schritt)

```
EntityStore::cas_update(coll, id, expected, patch)
   │
   ├─ DirectMutator m = DirectMutator { db }
   └─ core_cas_entity(&mut m, expected, patch)
          │
          ├─ current = get_entity(m, coll, id)      // Mutator.get
          ├─ if !matches(current, expected) → Err(Conflict)
          ├─ new = apply(patch, current)            //纯内存
          └─ core_put_entity(m, coll, id, &new)     // bestehender Pfad
                                                    // → Index-Order, Hint
   │
   └─ (Direct) WAL-Block: begin → tx_put → commit → fsync
```

`Transaction::cas_update` nutzt `TxMutator` statt `DirectMutator`; der Rest
ist identisch und wird erst beim Transaktions-`commit` durable.

---

## §4 Scope-Gate (ausdrücklich NICHT in v1.2)

- MVCC / Snapshots / Point-in-Time-Reads (eigener Architekturblock).
- Multi-Column / Composite Secondary Indexes (Query-/Index-Ausbau).
- Remote/Object-Storage-Backup (baut auf v1.1 Backup-Root auf).
- **Version-Counter als Pflichtfeld** (würde Schema-Bump + Migration nötig
  machen; nur als optionale spätere Erweiterung geführt).
- Schema-Migration zwischen Formatversionen.

---

## §5 Offene Frage + Empfehlung

**Ist die Vergleichsbasis Wert (`Expected::Entity`) oder Version?**

- **Empfehlung:** **Wertbasiert** (`Expected::Entity`/`Absent`/`Field`).
  Passt zur heutigen Architektur (kein neues Feld, keine Migration), deckt
  die üblichen CAS-Muster („ nur überschreiben wenn unverändert",
  „insert-if-absent") und ist über den `Mutator` sauber formuliert.
- Version-Counter nur aufnehmen, wenn ein konkreter Nutzerfall
  „optimistische Konkurrenz mit häufigen, kleinen Mutationen bei großen
  Entities" auftaucht — dann als **zusätzliches** `Entity`-Metafeld +
  `Expected::Version`, nicht als Ersatz der Wertbasis.

---

## §6 Nächster Schritt

Bei Freigabe: eigenständiger Implementierungs-Zyklus
(`core_cas_entity` + `Error::Conflict` + `EntityStore::cas_update` +
`Transaction::cas_update` + Oracle-Fixtures). `74f8a2f` bleibt bis dahin
unangetastet.
