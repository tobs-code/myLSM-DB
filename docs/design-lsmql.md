# LSMQL — Read-Only Query Language (Design-Spec)

> **Phase: Spezifikation (kein Code).** Basis: `911c4c7` (v1.3, composite
> indexes + `explain_query()`). Diese Spec ist die verbindliche Semantik für
> einen späteren Implementierungszyklus. **my-lsm-db v1.3 bleibt unangetastet** —
> LSMQL ist eine reine Frontend-Schicht über den existierenden Planner.

## 0. Scope

**IN v1 (read-only):**
- `SELECT` / `FROM` / `WHERE` / `ORDER BY` / `LIMIT` / `OFFSET`
- Bool-Expr: `AND` / `OR` / `NOT` (echter verschachtelter AST, keine flache Liste)
- Operatoren: `=  !=  <  <=  >  >=  IN  IS NULL  IS NOT NULL  IS ABSENT`
- Aggregationen: `COUNT(*)` `SUM` `AVG` `MIN` `MAX`
- `GROUP BY` — **AST/Parser kennen es, Executor liefert `UnsupportedQuery` in v1**
- Benannte Parameter: `$name` + `params` Map (keine String-Interpolation)
- `EXPLAIN` — mappt 1:1 auf `EntityStore::explain_query()` (Public API aus v1.3)

**OUT of scope (explizit, nie in v1):**
- Mutationen (`INSERT`/`UPDATE`/`DELETE`) — bleiben API-/Domain-Sache
  (`put`, `cas_update`, `Transaction`). LSMQL erfindet keine zweite
  Mutationssemantik.
- `JOIN` `UNION` `CTE` `WINDOW` `SUBQUERY` `TRIGGER` `FUNCTION`
- Full-Text / `LIKE` / `CONTAINS` — erst wenn die Engine eine definierte
  Semantik + sinnvolle Ausführung dafür besitzt.
- Composite-Index-Syntax (`USE INDEX ...`) — der **Planner** entscheidet.

---

## 1. Zielbild

```lsmql
SELECT id, title, status, priority
FROM tasks
WHERE project_id = $project
  AND status = "todo"
ORDER BY priority DESC
LIMIT 20
```

Intern:

```
LSMQL text
   ↓ Lexer
   ↓ Parser
   ↓ Query AST
   ↓ Semantic validation
   ↓ existing QueryBuilder / Planner   ← bleibt die Wahrheit
   ↓ Executor
   ↓ my-lsm-db (v1.3)
```

**Entscheidender Punkt:** kein zweiter Query-Planner. Der bestehende
Composite-Index-Planner aus v1.3 ist das alleinige Ausführungsmodell. LSMQL
beschreibt *was* gesucht wird, nicht *wie*.

### 1.1 Beispiele

```lsmql
-- einfacher Filter
FROM tasks
WHERE project_id = "p1" AND status = "todo"

-- OR / Verschachtelung
FROM tasks
WHERE (status = "todo" AND priority >= 2)
   OR (status = "doing" AND assignee = "tobias")

-- Aggregation
SELECT COUNT(*), SUM(estimate), AVG(estimate)
FROM tasks
WHERE project_id = "p1"

-- GROUP BY (AST vorbereitet, v1: UnsupportedQuery)
SELECT status, COUNT(*)
FROM tasks
WHERE project_id = "p1"
GROUP BY status

-- EXPLAIN
EXPLAIN
SELECT id, title
FROM tasks
WHERE project_id = "p1" AND status = "todo"
LIMIT 20
```

---

## 2. Grammatik (EBNF)

```ebnf
query        := explain? select from where? order? limit? offset? ;

explain      := "EXPLAIN" ;
select       := "SELECT" projection ;
projection   := "*"
              | aggOrField ("," aggOrField)* ;
aggOrField   := aggregate | field ;
aggregate    := "COUNT" "(" "*" ")"
              | ("SUM" | "AVG" | "MIN" | "MAX") "(" field ")" ;
field        := IDENT ;

from         := "FROM" collection ;
collection   := IDENT ;            -- "tasks", "events", "users", ...

where        := "WHERE" expr ;
expr         := orExpr ;
orExpr       := andExpr ("OR" andExpr)* ;
andExpr      := notExpr ("AND" notExpr)* ;
notExpr      := "NOT" notExpr | pred ;
pred         := parenExpr
              | field op value
              | field "IN" "(" value ("," value)* ")"
              | field "IS" ("NULL" | "NOT NULL" | "ABSENT") ;
parenExpr    := "(" expr ")" ;

op           := "=" | "!=" | "<" | "<=" | ">" | ">=" ;

order        := "ORDER" "BY" orderItem ("," orderItem)* ;
orderItem    := field ("ASC" | "DESC")? ;
limit        := "LIMIT" INT ;
offset       := "OFFSET" INT ;

value        := string | number | bool | param ;
param        := "$" IDENT ;
string       := "\"" [^"]* "\"" ;
number       := "-"? [0-9]+ ("." [0-9]+)? ;
bool         := "true" | "false" ;
```

### 2.1 SQL-Reihenfolge

`SELECT → FROM → WHERE → ORDER BY → LIMIT → OFFSET` (klassisch, sofort
verständlich). `EXPLAIN` ist Präfix. `FROM`-vor-`SELECT`-Stil (wie in
manchen Beispielen oben) ist **nicht** Teil der Grammatik — nur die
SQL-Reihenfolge wird geparst. (Die informellen Beispiele mit `FROM` zuerst
waren nur zur Lesbarkeit; die Spec bindet die SQL-Reihenfolge.)

---

## 3. AST

```text
Query
 ├── explain:     bool
 ├── projection:  Vec<ProjItem>        // *  |  field / aggregate
 ├── source:      Ident                // collection name
 ├── predicate:   Option<Expr>
 ├── group_by:    Vec<Ident>           // v1: AST only → UnsupportedQuery
 ├── order_by:    Vec<(Ident, Dir)>
 ├── limit:       Option<usize>
 └── offset:      usize

ProjItem
 ├── Star
 ├── Field(Ident)
 └── Agg { kind: AggKind, field: Option<Ident> }   // COUNT(*) → field=None

AggKind = Count | Sum | Avg | Min | Max

Expr
 ├── And(Vec<Expr>)
 ├── Or(Vec<Expr>)
 ├── Not(Box<Expr>)
 └── Pred(Predicate)

Predicate
 ├── Eq(Field, Value)
 ├── Ne(Field, Value)
 ├── Lt(Field, Value)
 ├── Le(Field, Value)
 ├── Gt(Field, Value)
 ├── Ge(Field, Value)
 ├── In(Field, Vec<Value>)
 ├── IsNull(Field)        // explizit gespeicherter NULL
 └── IsAbsent(Field)      // Feld fehlt komplett (absent)

Value = String | Number | Bool | Param(Ident)
```

### 3.1 Wichtige Designentscheidung: echter Bool-AST

Keine flache Filterliste. `And`/`Or`/`Not` sind echte Knoten, damit
verschachtelte Ausdrücke wie

```lsmql
WHERE (status = "todo" AND priority >= 2)
   OR (status = "doing" AND assignee = "tobias")
```

verlustfrei abgebildet werden. Das bestehende Query-Modell (v1.3) unterstützt
bereits OR/Union-Verhalten auf Candidate-ID-Ebene — der AST ist die direkte
Quelle dafür.

---

## 4. NULL vs. ABSENT (v1.2/v1.3 Semantik)

Die Sprache ehrt die bestehende NULL/absent-Unterscheidung **exakt**:

| LSMQL                 | Bedeutung                              | Entity-Zustand            |
|-----------------------|---------------------------------------|---------------------------|
| `assignee IS NULL`    | explizit gespeicherter Nullwert       | Feld = `Value::Null`      |
| `assignee IS ABSENT`  | Feld fehlt komplett                  | Feld nicht in Entity      |
| `assignee = "x"`      | Feld = `"x"`                          | Feld = `Value::String`    |

**Kein** implizites Mapping. `IS NULL` ≠ `IS ABSENT`. Wer nach "kein Wert"
suchen will, muss den Operator bewusst wählen. Das erhält die Semantik aus
v1.2/v1.3 unverändert.

---

## 5. Semantic Validation

Nach dem Parsen, vor dem Planner-Aufruf:

1. **Collection existiert** im Store-Schema? → sonst `UnknownCollection`.
2. **Felder bekannt?** Projektion/Aggregat-Feld/WHERE-Feld müssen auf der
   Collection definiert sein → sonst `UnknownField`.
3. **Typkompatibilität:** Operator vs. Wertetyp.
   - `=, !=, <, <=, >, >=` erfordern vergleichbare Typen
     (Int↔Int, Float↔Float, String↔String, Bool↔Bool).
   - `IN` erfordert, dass alle Listenelemente denselben (vergleichbaren) Typ
     wie das Feld haben.
   - `IS NULL / IS NOT NULL / IS ABSENT` sind typfrei (gelten für jedes Feld).
4. **Parameter aufgelöst:** jeder `$name` muss in der `params`-Map stehen →
   sonst `UnboundParameter`.
5. **GROUP BY:**
   - Parser/AST akzeptieren es.
   - In **v1** liefert der Executor bei `group_by.non_empty()`
     `UnsupportedQuery("GROUP BY not implemented in v1")`.
   - Später: Validierung, dass GROUP-BY-Felder in Projektion/Quelle vorkommen.
6. **Aggregation ↔ Projection:** `COUNT(*)`/`SUM`/... dürfen nur in
   `SELECT` stehen, nicht in `WHERE`/`ORDER BY`.

---

## 6. Parameter (kein String-Building)

API-Vertrag (taskdb später):

```json
POST /query
{
  "query": "SELECT id, title FROM tasks WHERE project_id = $project",
  "params": { "project": "p1" }
}
```

- `$name` wird **vor** dem Planner durch den Parameterwert ersetzt
  (Parameter-Substitution im AST, nicht im Text).
- Dadit keine SQL-Injection via String-Interpolation.
- Der Substitutions-Schritt ist Teil der Semantic Validation (Punkt 5.4).

---

## 7. EXPLAIN

`EXPLAIN <query>` liefert den **bestehenden** Plan aus
`EntityStore::explain_query()` (Public API seit v1.3) — LSMQL füttert den
Planner mit dem gleichen `Query`-Builder-Aufruf und gibt dessen Plan-Struct
zurück. Die Sprache erfindet keine eigene Plan-Darstellung.

Beispielausgaben (semantisch, nicht im Format gebunden):

```lsmql
EXPLAIN
SELECT id, title
FROM tasks
WHERE project_id = "p1" AND status = "todo"
LIMIT 20
```
```
CompositeIndexScan
  index: tasks_project_status
  prefix:
    project_id = "p1"
    status = "todo"
  residual: none
  limit: 20
```

```lsmql
EXPLAIN
SELECT id, title
FROM tasks
WHERE project_id = "p1"
  AND priority >= 2
```
```
CompositeIndexScan
  index: tasks_project_status
  prefix:
    project_id = "p1"
  residual:
    priority >= 2
```

```lsmql
EXPLAIN
SELECT id, title
FROM tasks
WHERE title = "foo"
```
```
FullScan
  collection: tasks
  residual:
    title = "foo"
```

Das ist die saubere Antwort auf den v1.3-Benchmark-Befund: statt zu raten,
macht LSMQL den tatsächlichen Plan sichtbar. Die Sprache beschreibt *was*,
der Planner entscheidet *wie* — und `EXPLAIN` macht beides transparent.

---

## 8. Fehlersemantik

| Fehler                  | Trigger                                | HTTP (taskdb) |
|-------------------------|---------------------------------------|---------------|
| `ParseError`            | ungültige Syntax                      | 400           |
| `UnknownCollection`     | `FROM <x>` nicht im Schema            | 404           |
| `UnknownField`          | Feld nicht auf Collection definiert   | 400           |
| `TypeMismatch`          | Operator vs. Wertetyp                 | 422           |
| `UnboundParameter`      | `$x` nicht in `params`                | 400           |
| `UnsupportedQuery`      | `GROUP BY` in v1, etc.                | 422           |

Fehler sind strukturiert: `{ "error": "<kind>", "message": "...", "span"?:
[line,col] }`. Kein Panic, keine DB-weite Exception.

---

## 9. Oracle-Matrix (20–30 Queries)

Jede Zeile: LSMQL → erwarteter Plan-Typ → erwartetes Ergebnis-Semantik.
Dient als Akzeptanzgitter für den späteren Implementierungszyklus.

| # | LSMQL                                                         | Plan             | Ergebnis-Semantik                         |
|---|--------------------------------------------------------------|------------------|-------------------------------------------|
| 1 | `SELECT * FROM tasks WHERE project_id = "p1"`               | CompositeScan    | alle Tasks von p1                         |
| 2 | `SELECT * FROM tasks WHERE project_id = "p1" AND status = "todo"` | CompositeScan | p1 + todo (Index-Prefix)               |
| 3 | `SELECT * FROM tasks WHERE status = "todo"`                 | FullScan/Index?  | alle todo (nur status-Teilindex?)        |
| 4 | `SELECT id, title FROM tasks WHERE project_id = "p1"`       | CompositeScan    | nur id,title projiziert                   |
| 5 | `SELECT * FROM tasks WHERE priority >= 2`                   | CompositeScan resid | priority-Filter als Residual            |
| 6 | `SELECT * FROM tasks WHERE status IN ("todo","doing")`      | FullScan/Union   | todo ∪ doing (OR auf Candidate-Ebene)    |
| 7 | `SELECT * FROM tasks WHERE assignee IS NULL`                | (residual)       | nur explizit-NULL assignee                |
| 8 | `SELECT * FROM tasks WHERE assignee IS ABSENT`              | (residual)       | nur absent assignee                       |
| 9 | `SELECT * FROM tasks WHERE assignee = "tobias"`             | (residual)       | assignee = tobias                         |
|10 | `SELECT * FROM tasks WHERE NOT(status = "done")`            | (residual)       | alles außer done                          |
|11 | `SELECT * FROM tasks WHERE (status="todo" AND priority>=2) OR (status="doing" AND assignee="t")` | (residual) | Verschachtelung, UND/ODER          |
|12 | `SELECT * FROM tasks WHERE project_id="p1" ORDER BY priority DESC` | CompositeScan + Sort | nach priority absteigend            |
|13 | `SELECT * FROM tasks WHERE project_id="p1" LIMIT 20`        | CompositeScan    | max 20 Zeilen                             |
|14 | `SELECT * FROM tasks WHERE project_id="p1" LIMIT 20 OFFSET 40` | CompositeScan | Zeilen 41–60                              |
|15 | `SELECT COUNT(*) FROM tasks WHERE project_id="p1"`          | CompositeScan    | eine Zeile: count                         |
|16 | `SELECT COUNT(*), SUM(estimate), AVG(estimate) FROM tasks WHERE project_id="p1"` | CompositeScan | 1 Zeile, 3 Aggregate               |
|17 | `SELECT MIN(estimate), MAX(estimate) FROM tasks`            | FullScan         | globale Min/Max                            |
|18 | `SELECT status, COUNT(*) FROM tasks WHERE project_id="p1" GROUP BY status` | — | v1: UnsupportedQuery                  |
|19 | `EXPLAIN SELECT * FROM tasks WHERE project_id="p1" AND status="todo"` | Plan-Struct | CompositeScan (Prefix p1,todo)        |
|20 | `EXPLAIN SELECT * FROM tasks WHERE title="foo"`             | Plan-Struct      | FullScan (residual title)                 |
|21 | `SELECT * FROM tasks WHERE project_id = $p AND status = $s` (+params) | CompositeScan | param-substituiert                     |
|22 | `SELECT * FROM tasks WHERE priority < 5 AND priority > 1`  | (residual)       | 1 < priority < 5                          |
|23 | `SELECT * FROM tasks WHERE assignee != "x"`                | (residual)       | alle außer x                              |
|24 | `SELECT * FROM events WHERE task_id = "t1" ORDER BY created_at DESC LIMIT 50` | CompositeScan | Activity-Feed t1                     |
|25 | `SELECT * FROM tasks WHERE project_id="p1" AND NOT(assignee IS ABSENT)` | (residual) | p1 mit gesetztem assignee              |
|26 | `SELECT title FROM tasks WHERE project_id="p1" AND status="todo" LIMIT 5` | CompositeScan | nur title, top 5                      |
|27 | `SELECT * FROM tasks WHERE status = "done" AND estimate >= 8` | (residual)    | done + groß                               |
|28 | `SELECT * FROM tasks WHERE id = "w1"`                       | PK/FullScan      | eine Task                                  |
|29 | `EXPLAIN SELECT COUNT(*) FROM tasks WHERE project_id="p1"`  | Plan-Struct      | CompositeScan (count)                      |
|30 | `SELECT * FROM nonexistent WHERE x = 1`                     | —                | Parse/Validation: UnknownCollection       |

---

## 10. Warum das v1.3 nicht verbiegt

- **Kein zweiter Planner.** LSMQL übersetzt 1:1 in den bestehenden
  `QueryBuilder` + `execute_query` + `explain_query`.
- **Composite Indexes bleiben unsichtbar** für die Sprache. Der User schreibt
  `WHERE project_id=... AND status=...`, der Planner wählt den Index.
- **NULL/absent** wird 1:1 durchgereicht (eigene Operatoren).
- **Aggregationen** nutzen die v0.8-Semantik (count/sum/avg/min/max als
  Resultat-Form).
- **GROUP BY** ist AST-reserviert, Executor sagt in v1 `UnsupportedQuery` —
  die Sprache wird nicht später gebrochen, wenn es implementiert wird.
- **Mutationen** bleiben außerhalb. Kein Konflikt mit `put`/`cas_update`/
  `Transaction`.

---

## 11. Nächster Schritt (nicht in diesem Zyklus)

Wenn die Oracle-Matrix + diese Spec als "semantisch sauber" akzeptiert ist,
kann ein Implementierungszyklus beginnen:

1. Lexer + Parser (Pest oder hand-rolled RecDescent)
2. AST → QueryBuilder-Mapping
3. `explain_query()`-Brücke für `EXPLAIN`
4. Parameter-Substitution
5. Oracle-Test-Harness (die 30 Queries aus §9)

**my-lsm-db v1.3 bleibt währenddessen eingefroren.** LSMQL ist additive
Frontend-Schicht, kein Eingriff in Storage/Planner.
