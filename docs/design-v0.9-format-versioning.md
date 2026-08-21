# myLSM-DB — Design v0.9: Format-Versionierung

**Status:** Read-only Spezifikation + Forensik. **Kein Code** in diesem Schritt.
**Baseline:** `527757b` (v0.8, stabiler Produkt-Checkpoint, `origin/main`).
**Ziel:** Ein Versionierungsvertrag für die On-Disk-Formate, sodass eine
Datenbank, die von einer *neueren/älteren/unkompatiblen* Binärversion stammt,
**sauber erkannt** und mit einer *eigenen, unmissverständlichen Fehlermeldung*
abgewiesen wird — und **nicht** als gewöhnliche Korruption interpretiert wird.

**Gate-Entscheidung (siehe §7):** v0.9 implementiert **keine Migration**.
Ein Version-Marker + ein eigener `UnsupportedFormatVersion`-Fehler ist die
Produktions-Verbesserung. Migration wird erst konzipiert, wenn tatsächlich ein
inkompatibles v2-Format existiert.

---

## §1 Befund (Produktlücke)

Heute hat **kein einziges On-Disk-Format** einen Version-Marker. Die einzigen
Schutzmechanismen sind:

- SSTable: 4-Byte-`MAGIC` (`"SSTB"`) als *Format-Identität* (keine Version).
- WAL: CRC32 pro Record als *Integritätsschutz* (keine Version).
- MANIFEST / SCHEMA: strikte Text-Zeilen-Parser (kein Versionsfeld).
- Value-/Key-Codec: Tag-Byte pro Wert (Typkennung, keine globale Version).

Daraus folgt das Risiko:

1. **Stille Fehlinterpretation.** Eine neuere Binärversion, die ein geändertes
   SSTable-/WAL-Layout schreibt, emittiert weiterhin `MAGIC="SSTB"`. Eine ältere
   Binärversion, die diese Dateien liest, parst sie **still** falsch (falsche
   Daten, Panic bei Range-Decode) — es gibt keinen Schutz.
2. **Keine Unterscheidung "neu" vs. "kaputt".** Ein von einer *neueren* Binary
   stammendes Format und eine *korrupte* Datei sehen aktuell gleich aus
   (`InvalidFormat`/`Corrupt`). Ein Betreiber kann nicht erkennen: *"Du brauchst
   eine neuere Binary"* vs. *"Deine Platte ist kaputt"*.
3. **Kein Mindest-Format.** Es gibt keine Deklaration, welche älteste
   Format-Version eine Binary noch lesen kann.

v0.9 schließt Lücke 1–3 mit einem **Versionierungsvertrag**, nicht mit einem
Migrations-Feature.

---

## §2 Format-Inventar (Ist-Stand, ohne Version-Marker)

Alle Pfade relativ zum DB-Verzeichnis.

### 2.1 `MANIFEST` (Text, atomic via tmp+rename)
```
L <level> <count> <id> <id> ...          # Level -> SSTable-file_ids
M <file_id> <min_key_hex> <max_key_hex> <records>
N <next_table_id>
```
- Hex-Kodierung der Segment-Ranges (`to_hex`).
- Parser strikt: `L`-Zeile mit ungültiger `level`-Zahl → `InvalidFormat`
  (`manifest.rs` F.3–F.5).
- **Kein Versionsfeld.** Erkennung von "anderes Format" nur über
  Zeilen-Struktur.

### 2.2 `SCHEMA` (Text, atomic via tmp+rename)
```
NC <next_collection_id>
NF <next_field_id>
C  <id> <name_escaped>                   # %20 / %25 escaping
F  <collection_id> <field_id> <name_escaped>
NI <next_index_id>
IX <id> <collection_id> <field_id> <BUILDING|READY>
```
- Namens-Escaping (Leerzeichen sicher).
- Parser: unbekannter Status-String → `InvalidFormat`; fehlende Token →
  `InvalidFormat` (`schema.rs`).
- **Kein Versionsfeld.**

### 2.3 `wal.log` (Binär, Records)
```
für jeden Record: [ crc: u32 LE ][ type: u8 ][ payload... ]
```
- `type` ∈ {0 PUT,1 DELETE,2 BEGIN,3 TX_PUT,4 TX_DELETE,5 COMMIT,6 ABORT}.
- CRC over `[type][payload]` (Schutz vor Bitrot/Teil-Schreiben).
- Unbekannter `type` → Replay stoppt (behandelt als "inkompatibel",
  nicht als Korruption). **Kein Versionsfeld** im Header.

### 2.4 `*.sst` (Binär, SSTable)
```
[ records ][ sparse index ][ bloom ][ footer ]
record := [ flags: u8 ][ key ][ value ]      # flags: 0=put, 1=tombstone
sparse index entry := [ key ][ offset: u32 LE ]   # nur an Block-Grenzen (spacing=16)
bloom := 1024bit / 4 hashes
footer := [ index_off: u32 LE ][ bloom_off: u32 LE ][ MAGIC: u32 LE = 0x53535442 ]
```
- `MAGIC="SSTB"` nur im Footer, prüft Format-Identität.
- **Kein Versionsfeld** (auch nicht im Footer neben MAGIC).
- Records haben **keine CRC** → Korruption im Record-Body wird *nicht*
  erkannt (stille Falschdaten / Panic bei `decode_ordered`).

### 2.5 Key-Encoding (`keycodec.rs`) — keine Version
```
Entity-Key: 'E' | collection_id: u32 LE | entity_id_len: u32 LE | entity_id | field_id: u32 LE
Index-Key:  'I' | collection_id: u32 LE | field_id: u32 LE | encoded_value | entity_id_len: u32 LE | entity_id
```
- `encoded_value` via `ordering.rs` (`encode_ordered`): Tag-Byte + Payload,
  selbst-delimitierend, ordnungserhaltend.

### 2.6 Value-Codec (`codec.rs`) — keine Version
- Tag ∈ {NULL,BOOL,INT,FLOAT,STRING,BYTES} + Payload.

**Fazit des Inventars:** Einzige Format-Identität = SSTable-`MAGIC` und WAL-CRC.
Es gibt weder DB- noch Datei-Version. Das ist der Kern von §1.

---

## §3 Versionierungsstrategie

### 3.1 Wo sitzt die Version? → **DB-Ebene + Defense-in-Depth pro Datei**
- **Primär:** Eine neue Datei **`VERSION`** im DB-Verzeichnis (Text, eine Zeile
  `V <u32>`), atomar geschrieben (tmp+rename+fsync) bei DB-Erstellung.
  Sie ist die *Single Source of Truth* für die komplette Format-Familie, weil
  MANIFEST/SCHEMA/SSTables/WAL immer gemeinsam (atomar pro Operation) geschrieben
  werden.
- **Defense-in-Depth (empfohlen, kann in v0.9 oder Folge-Schritt):**
  - SSTable-Footer um 1 Byte `format_version` erweitern (vor/nach MAGIC).
  - WAL-Header um 1 Byte `format_version` ergänzen.
  Das schützt den *Einzel-Datei*-Lese-Pfad (z. B. Recovery-Scan über `*.sst`),
  falls `MANIFEST`/`VERSION` fehlen.

### 3.2 Versionstyp → **`u32`, monoton steigend (KEIN SemVer)**
- SemVer suggeriert Kompatibilitätsversprechen, die wir nicht erzwingen können.
  Eine einfache Ganzzahl ist ausreichend und unambiguous.
- Binary deklariert zwei Konstanten:
  - `FORMAT_VERSION: u32` — die Version, die diese Binary *schreibt* (aktuell `1`).
  - `MIN_SUPPORTED_VERSION: u32` — die älteste Version, die diese Binary noch
    *lesen* kann (initial = `1`).
- **Legacy-Kompromiss (wichtig):** Existiert **keine `VERSION`-Datei**, wird
  sie als `v1` interpretiert. Damit bleiben bereits existierende Produktions-DBs
  (vor v0.9 erzeugt, z. B. `527757b`) **weiterhin lesbar** — kein Breaking Change.

### 3.3 Readable vs. Writable
- *Writable* = `FORMAT_VERSION` (was wir schreiben).
- *Readable* = `[MIN_SUPPORTED_VERSION ..= FORMAT_VERSION]`.
- Alles außerhalb dieses Intervalls → explizite Ablehnung (§5).

---

## §4 Kompatibilitätsmatrix (DB-Version × Binary-Version)

Sei `db` = Version in `VERSION` (oder `1` bei Fehlen), `cur` = `FORMAT_VERSION`,
`min` = `MIN_SUPPORTED_VERSION` der Binary.

| Fall | Bedingung | Verhalten | Fehler |
|------|-----------|-----------|--------|
| **Exakt kompatibel** | `min ≤ db ≤ cur` | normales Öffnen | — |
| **Ältere, aber lesbare DB** | `min ≤ db < cur` | öffnen (gleiches Format, keine Migration nötig) | — |
| **Ältere, nicht mehr lesbare DB** | `db < min` | ablehnen, kein implizites Migrieren | `UnsupportedFormatVersion` (zu alt) |
| **Neuere DB (Zukunft)** | `db > cur` | ablehnen, Hinweis "neuere Binary nötig" | `UnsupportedFormatVersion` (zu neu) |
| **Unlesbare Versionsangabe** | `VERSION` vorhanden, aber nicht parsebar | ablehnen, Struktur unklar | `InvalidFormat` |
| **Bekannte Version, korrupte Daten** | `min ≤ db ≤ cur`, aber CRC/Parse-Intern bricht | ablehnen | `Corrupt` |

**Kernregel:** `db > cur` (neuere DB) wird **nie** als `Corrupt` behandelt.
Das wäre die gefährlichste Verwechslung (Betreiber würde "Platte kaputt"
denken, statt "Binary zu alt").

---

## §5 Fehlervertrag (muss konzeptionell getrennt sein)

Heute (`error.rs`): `Io`, `Corrupt(&'static str)`, `NotFound`,
`InvalidFormat(String)`, `InvalidArgument(String)`, `Unsupported(String)`,
`Internal(String)`.

Neu: **eigener Variant** `UnsupportedFormatVersion`, der sauber von
`InvalidFormat` und `Corrupt` getrennt ist.

```
InvalidFormat(msg)            // Struktur/Identität unlesbar:
                              //   falsches MAGIC, unparsebares Text-Format,
                              //   unbekannter Dateityp, unparsebare VERSION
Corrupt(what)                 // erkanntes Format + bekannte Version, aber
                              //   intern inkonsistent:
                              //   CRC-Fail, Index zeigt außerhalb der Datei,
                              //   Key-Kollation gebrochen
UnsupportedFormatVersion {    // erkanntes Format, aber Version außerhalb
  found: u32,                 //   [MIN_SUPPORTED, FORMAT_VERSION]
  min_supported: u32,
  max_supported: u32,
}
```

Unterscheidung in einem Satz:
- `InvalidFormat` → *"Ich verstehe diese Datei überhaupt nicht."*
- `UnsupportedFormatVersion` → *"Ich erkenne das Format, aber die Version passt nicht (zu alt/zu neu)."*
- `Corrupt` → *"Format + Version ok, aber der Inhalt ist intern kaputt."*

Diese drei müssen in Code **und** in Nutzer-Meldung unterscheidbar bleiben
(kein `InvalidFormat` als Generic-Catch-All für die anderen beiden).

---

## §6 Migration (nur Konzept — in v0.9 NICHT umgesetzt)

Da v0.9 **keine** inkompatible Formatänderung einführt, ist Migration nicht
erforderlich. Für die Zukunft (wenn ein echtes v2 entsteht) gilt folgender
Vertrag, der hier nur festgehalten wird:

1. **Niemals implizit beim Öffnen migrieren.** Migration ist ein *expliziter*
   Befehl (`db migrate`), nie Teil von `open()`. `open()` bei `db > cur` oder
   `db < min` schlägt sauber fehl (§4/§5).
2. **In-Place mit Backup.** Migration läuft im selben Verzeichnis, aber erst
   nach einem vollständigen Backup (Snapshot/Kopie des DB-Verzeichnisses).
3. **Crash-Safety.** Jeder Schritt atomar (tmp+rename+fsync), damit ein
   Abbruch zwischen zwei Schritten die DB nicht halb-migriert zurücklässt.
4. **Rollback.** Bei Fehler während Migration: Wiederherstellung aus dem Backup;
   die alte `VERSION` bleibt bis zum erfolgreichen Abschluss erhalten.
5. **Idempotenz.** Migration mehrfach aufrufbar, ohne Doppeleffekte
   (Re-Check der Ziel-Version).
6. **Lesbarkeits-Garantie.** Eine Binary muss `MIN_SUPPORTED_VERSION`
   garantieren; eine Migration erhöht `FORMAT_VERSION`, aber senkt nie
   `MIN_SUPPORTED_VERSION` unter das, was schon ausgeliefert wurde.

---

## §7 Gate-Entscheidung (v0.9 braucht KEINE Migration)

**Frage:** Muss v0.9 Migration enthalten, oder reicht ein versionierter
Marker + expliziter "unsupported version"-Fehler?

**Antwort:** Ein **versionierter Marker + `UnsupportedFormatVersion`-Fehler**
reicht. Begründung:
- v0.9 ändert das On-Disk-Layout **nicht** (kein Breaking Change). Die
  einzige neue Datei ist `VERSION`, und deren *Fehlen* wird als `v1` behandelt
  (§3.2) → bestehende DBs bleiben lesbar.
- Der reale Nutzen ist sofort: ein Betreiber mit einer *neueren* DB erhält
  eine klare Meldung statt stillem Datenmüll.
- Migration wäre *Premature Complexity*: Es existiert noch gar kein
  inkompatibles Zielformat. Die Migration wird genau dann konzipiert, wenn
  v1→v2 tatsächlich definiert wird.

**Empfehlung für v0.9-Umfang:** `VERSION`-Datei + `FORMAT_VERSION`/
`MIN_SUPPORTED_VERSION`-Konstanten + `UnsupportedFormatVersion`-Fehler +
Lesbarkeits-Check in `open()`. (Optional, aber empfohlen: pro-Datei
Versions-Byte in SSTable-Footer/WAL-Header als Defense-in-Depth — kann als
Teil von v0.9 oder als kleiner Folgeschritt erfolgen; Entscheidung bei
Implementierungs-Go-Ahead.)

---

## §8 Regression / Fixtures

Folgende Fixtures müssen als Regressionstests entstehen (erst bei
Implementierungs-Go-Ahead, Code-Schritt):

1. **Normale aktuelle DB** — `VERSION` = `FORMAT_VERSION` → `open()` ok.
2. **Zukünftige/ungewöhnliche Version** — `VERSION` = `cur + 1` → erwartet
   `UnsupportedFormatVersion` (NICHT `Corrupt`, NICHT Panic).
3. **Korrupte Versionsangabe** — `VERSION` = `"xyz\n"` → erwartet
   `InvalidFormat`.
4. **Bekannte Version, korrupter Body** — gültige `VERSION`, aber SSTable mit
   manipuliertem Record (CRC-Fail im WAL bzw. gebrochene Kollation) → erwartet
   `Corrupt`.
5. **Legacy-DB ohne `VERSION`** — alte Produktions-DB (wie `527757b`) → erwartet
   sauberes Öffnen als `v1` (kein Breaking Change).
6. **Zu alte DB (nur relevant sobald `MIN_SUPPORTED_VERSION > 1`)** — `VERSION`
   unter `min` → erwartet `UnsupportedFormatVersion` (zu alt).
7. **Crash während Migration** — *nur* falls Migration jemals in Scope kommt
   (hier nicht). Dann: Backup-Wiederherstellung, alte `VERSION` intakt.

Jeder Fixture ist eine kleine, eingecheckte Verzeichnis-Struktur unter
`tests/fixtures/v0.9/...`, die `open()` mit der jeweils erwarteten
`Result`-Variante prüft.

---

## §9 Kurz-Skizze der Implementierung (KEIN Code in diesem Schritt)

*Nur zur Veranschaulichung, wenn der Go-Ahead erfolgt:*

- `src/lib.rs`: Konstanten `FORMAT_VERSION = 1`, `MIN_SUPPORTED_VERSION = 1`;
  `open()` liest `VERSION` (Fehlen ⇒ `1`), prüft Intervall, sonst
  `UnsupportedFormatVersion`.
- `src/error.rs`: neuer Variant `UnsupportedFormatVersion { found, min_supported,
  max_supported }` + `Display`.
- `src/version.rs` (neu, klein): `read_version(path)`, `write_version(path)`
  (atomic), `check_compatible(found) -> Result<()>`.
- Optional `src/sstable.rs` / `src/wal.rs`: 1 Byte Versionsfeld im
  Footer/Header + Check beim Laden.
- Tests in `tests/version.rs` + Fixtures (§8).

---

## §10 Nächster Schritt

1. Diese Spec liegt als `design-v0.9-format-versioning.md` (untracked) vor.
2. Gate bestätigen: **v0.9 = Version-Marker + Fehlervertrag, keine Migration.**
3. Bei Go-Ahead: Implementierung (§9) + Fixtures/Regression (§8) + Volltest
   (`cargo test --release`) + Checkpoint (Commit auf `527757b` + Push).

Bis dahin: **kein Code**, nur Dokumentation.
