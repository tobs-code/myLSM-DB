# v1.4 — LSMQL Read-Only Query Interface (Release-Spec / Inventur)

> **Phase: Release-Spec + Inventur. Kein neuer Code in diesem Zyklus.**
> Ziel: den **bestehenden** LSMQL-Vertrag einfrieren und als v1.4-Release-Hardening
> vorbereiten. LSMQL ist bereits implementiert (Commit `0de0a90` + Hardening
> `5ea3886`, `22f031e`, `c3c8431`) und durch einen echten Consumer
> (taskdb `7b4c47d` → `60f8d9d`) abgenommen.

## 0. Warum v1.4 jetzt (und nicht früher)

Frühere v1.4-Kandidaten waren hypothetisch. v1.4 ist der erste Kandidat, der
durch einen **realen Consumer** und einen **vollständigen Vertrag** begründet
ist:

- **Consumer:** taskdb nutzt LSMQL über HTTP (`POST /api/query`,
  `POST /api/query/explain`) als echte Query-Schnittstelle.
- **Vertrag:** 9/9 HTTP-Vertragstests + 10/10 Consumer-Oracle-Tests grün.
- **Storage:** my-lsm-db v1.3.0 (`911c4c7`) bleibt unangetastet — LSMQL ist
  additive Frontend-Schicht.

## 1. Scope (v1.4)

**IN v1.4 (read-only, bereits implementiert + getestet):**

- `SELECT ... FROM`
- `WHERE`
- `AND` / `OR` / `NOT` (echter verschachtelter Bool-AST)
- `IN`
- Benannte Parameter `$name` (Substitution im AST, nicht im Text)
- `IS NULL` (explizit gespeicherter `Value::Null`)
- reserviertes `IS ABSENT` → `UnsupportedQuery` (wie `GROUP BY`)
- `ORDER BY` (`ASC` / `DESC`)
- `LIMIT` / `OFFSET`
- Aggregationen: `COUNT(*)`, `SUM`, `AVG`, `MIN`, `MAX`
- `EXPLAIN` (1:1 auf `EntityStore::explain_query()` aus v1.3)

**Architektur-Invariante (hart):**

```
LSMQL text
   ↓ Lexer → Parser → Query AST → Semantic Validation
   ↓ existing QueryBuilder        ← LSMQL erfindet keinen eigenen Planner
   ↓ existing Planner (v1.3)
   ↓ EntityStore::execute_query / execute_aggregate / explain_query
   ↓ my-lsm-db v1.3.0
```

> **LSMQL entwickelt keine eigene Optimierungslogik.** Die einzige
> Ausführungssemantik ist `AST → QueryBuilder → bestehender Planner`.
> Composite-Index-Auswahl bleibt ausschließlich Planner-Sache.

## 2. Explizite Gates (NICHT in v1.4)

- `INSERT` / `UPDATE` / `DELETE` — bleiben `put` / `cas_update` / `Transaction`
- `GROUP BY` — AST-reserviert, bleibt `UnsupportedQuery` in v1.4
- MVCC / Snapshots
- neue Index-Typen
- Index-Intersection (jenseits des bestehenden Composite-Planners)
- Remote Backup
- Storage- / WAL-Änderungen
- **eigener Planner** (zweite Query-Semantik neben `QueryBuilder`)
- zweite Query-Semantik neben `QueryBuilder`

## 3. NULL vs. ABSENT (eingefroren, v1.2/v1.3-Semantik)

| LSMQL                | Bedeutung                    | Entity-Zustand       |
|----------------------|------------------------------|----------------------|
| `assignee IS NULL`   | explizit gespeicherter Null  | Feld = `Value::Null` |
| `assignee IS ABSENT` | Feld fehlt komplett          | Feld nicht in Entity |
| `assignee = "x"`     | Feld = `"x"`                 | Feld = `Value::String` |

`IS NULL` ≠ `IS ABSENT`. Kein implizites Mapping.

## 4. Fehlervertrag (eingefroren)

| Fehler                  | Trigger                                  | HTTP (taskdb) |
|-------------------------|-------------------------------------------|---------------|
| `Parse`                 | ungültige Syntax                         | 400           |
| `UnknownCollection`     | `FROM <x>` nicht im Schema               | 400¹          |
| `UnknownField`          | Feld nicht auf Collection definiert      | 400           |
| `TypeMismatch`          | Operator vs. Wertetyp                    | 400¹          |
| `UnboundParameter`      | `$x` nicht in `params`                   | 400           |
| `Unsupported`           | `GROUP BY`, `IS ABSENT` in v1.4          | 422           |

¹ Abweichung von `design-lsmql.md §8` (dort 404/422): der taskdb-HTTP-Vertrag
mappt alle `Semantic`-Fehler auf `400` und `Unsupported` auf `422` (siehe
`taskdb/src/server.rs::map_err`). **Der taskdb-Vertrag ist die binding authority**
für HTTP-Status; `design-lsmql.md §8` wird in einem Folge-Edit angepasst.

## 5. Abnahme (Regression-Gate, keine Feature-Entwicklung)

Die folgenden Gates müssen alle grün sein, bevor der Version-Bump auf
`1.4.0` erfolgt:

1. **80/80 lsm-db Tests** bleiben grün (inkl. 11 LSMQL-Oracle + 12 LSMQL-Unit).
2. **9 HTTP-Vertragstests** in taskdb (`tests/http_api.rs`) bleiben grün.
3. **30er-Oracle-Matrix** aus `design-lsmql.md §9` wird **vollständig** als
   Regression/Abnahme geprüft — jede Zeile als dedizierter Test.
4. **LSMQL-Ergebnis ≡ QueryBuilder-Ergebnis** (exakte ID-Menge; bei
   `ORDER BY` zusätzlich exakte Reihenfolge).
5. **`EXPLAIN`** kommt ausschließlich aus dem bestehenden Planner
   (`explain_query()`).
6. **Keine Regression** bei `NULL` / `ABSENT`.
7. **Parameterwerte** werden niemals als Query-Syntax interpretiert
   (Injection-Sicherheit: `' OR 1=1 --` bleibt reiner Wert).

> **Regel bei der Matrix-Abnahme:**
> - Test prüft nur bereits spezifizierte Semantik → Test hinzufügen.
> - Test verlangt neue Semantik oder neue Engine-Fähigkeit → **STOP**,
>   Befund/Produktentscheidung, kein stilles Scope-Wachstum.

## 6. Release-Hardening (nächster Zyklus, nach Inventur)

Wenn die Inventur **keinen neuen A/B-Befund** findet, ist die Implementierung
im nächsten Zyklus faktisch ein **Release-Hardening des existierenden Features**
— kein neues Engine-Feature:

- Version-Bump `1.3.0` → `1.4.0` in `Cargo.toml` (nur nach bestandener Abnahme §5).
- Vollständige 30er-Oracle-Matrix als permanente Regressionstests.
- `design-lsmql.md §8` Fehler-/HTTP-Tabelle an den taskdb-Vertrag anpassen.
- Changelog-Eintrag: "LSMQL read-only query interface (stable)".
- my-lsm-db v1.3.0 (`911c4c7`) bleibt die Storage-Baseline.

## 7. Referenzen

- Spec: `docs/design-lsmql.md` (Ursprungs-Spec, Phase Spezifikation)
- Oracle-Matrix: `docs/design-lsmql.md §9` (20–30 Queries)
- Implementierung: `src/lsmql/` (Commits `0de0a90`, `5ea3886`, `22f031e`, `c3c8431`)
- Consumer: taskdb `POST /api/query`, `POST /api/query/explain`
  (Commits `7b4c47d`, `e729dad`, `60f8d9d`)
- Storage-Baseline: my-lsm-db v1.3.0 `911c4c7` (eingefroren)
