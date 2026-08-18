//! Regelbasierter Planner: `LogicalPlan` → `PhysicalPlan`.
//!
//! ## Regeln
//!
//! 1. Das Prädikat wird in **DNF** zerlegt (`OR` top-level, jede Klausel eine
//!    Konjunktion von Literalen).
//! 2. Pro AND-Klausel wird **ein** Index gewählt: ein Feld mit READY-Index, das
//!    ein indexierbares (nicht-negiertes, `!=`-freies) Prädikat trägt. Cost-basiert:
//!    `cost = BASE_CARDINALITY * selectivity(shape)` mit `Eq < Between < OneSided`;
//!    bei gleichem Shape gewinnt das enger gebundene Literal, sonst lex kleinstes
//!    Feldname (deterministisch). Keine Statistik-/Cardinality-Infrastruktur.
//! 3. Die Bounds des gewählten Feldes werden zum Index-Range gemerged; das Feld
//!    fällt damit aus dem Residual-Filter heraus (`IndexScan` liefert bereits
//!    verifizierte IDs).
//! 4. `OR` → `UnionIds` aus den Klausel-Zweigen (jeder Zweig `Fetch{IndexScan}`
//!    oder `FullScan`), Dedup per Entity-ID.
//! 5. `Ne`, negierte Literale und nicht-indexierte Felder → `Filter`
//!    (Residual). Die Evaluierung nutzt dieselbe `eval` wie das Oracle.
//!
//! Wichtig für die **Missing-Field-Semantik**: Ein negiertes Literal wird NICHT
//! als umgekehrter Range-Operator dargestellt (also `Not(age>30)` wird nicht zu
//! `age<=30`), weil das bei fehlenden Feldern die Semantik verändern würde
//! (`Not(Gt(missing)) = true`, aber `Lte(missing) = false`). Deshalb bleibt
//! `Not` im Literal und wird nur im Residual-Filter über `Predicate::Not`
//! rekonstruiert und via `eval` (kompositional) ausgewertet.

use crate::codec::Value;
use crate::index::Bound;
use crate::ordering;
use crate::schema::{IndexStatus, Schema};

use super::ast::{Cmp, Predicate};
use super::logical::{LogicalPlan, SortDir};
use super::physical::PhysicalPlan;

/// Ein Literal einer Konjunktion: `[not] field cmp value`.
#[derive(Debug, Clone)]
struct Lit {
    field: String,
    cmp: Cmp,
    value: Value,
    negated: bool,
}

impl Lit {
    /// Indexierbar = positives (nicht-negiertes) Literal mit nicht-`Ne`-Vergleich.
    fn indexable(&self) -> bool {
        !self.negated && self.cmp != Cmp::Ne
    }

    fn to_predicate(&self) -> Predicate {
        let p = Predicate::Field(self.field.clone(), self.cmp, self.value.clone());
        if self.negated { p.negate() } else { p }
    }
}

/// DNF eines Prädikats: Liste von Klauseln, jede eine Konjunktion von Literalen.
///
/// Diese Transformation nutzt NUR die Booleschen Identitäten (Distribution,
/// De Morgan, Doppelnegation) — sie setzt nie `Not(cmp)` als invertierten
/// `cmp` ein. Damit bleibt sie exakt äquivalent zur kompositionalen `eval`.
fn dnf(pred: &Predicate) -> Vec<Vec<Lit>> {
    match pred {
        Predicate::Field(f, c, v) => vec![vec![Lit {
            field: f.clone(),
            cmp: *c,
            value: v.clone(),
            negated: false,
        }]],
        Predicate::And(a, b) => {
            let da = dnf(a);
            let db = dnf(b);
            let mut out = Vec::new();
            for ca in &da {
                for cb in &db {
                    let mut clause = ca.clone();
                    clause.extend(cb.clone());
                    out.push(clause);
                }
            }
            out
        }
        Predicate::Or(a, b) => {
            let mut out = dnf(a);
            out.extend(dnf(b));
            out
        }
        Predicate::Not(inner) => dnf_not(inner),
    }
}

/// DNF von `Not(inner)`.
fn dnf_not(pred: &Predicate) -> Vec<Vec<Lit>> {
    match pred {
        Predicate::Field(f, c, v) => vec![vec![Lit {
            field: f.clone(),
            cmp: *c,
            value: v.clone(),
            negated: true,
        }]],
        Predicate::Not(inner) => dnf(inner),
        Predicate::And(a, b) => {
            // Not(A ∧ B) = Not(A) ∨ Not(B)
            let mut out = dnf_not(a);
            out.extend(dnf_not(b));
            out
        }
        Predicate::Or(a, b) => {
            // Not(A ∨ B) = Not(A) ∧ Not(B)
            let da = dnf_not(a);
            let db = dnf_not(b);
            let mut out = Vec::new();
            for ca in &da {
                for cb in &db {
                    let mut clause = ca.clone();
                    clause.extend(cb.clone());
                    out.push(clause);
                }
            }
            out
        }
    }
}

/// Konstanter Cardinality-Platzhalter des Kostenmodells (v0.6). Explizit ein
/// Heuristik-Wert, später durch echte Statistiken ersetzbar — aber **keine**
/// Statistik-Infrastruktur in v0.6.
const BASE_CARDINALITY: f64 = 1.0;

/// Selektivitäts-Shape eines indexierbaren Literals: `Eq < Between < OneSided`.
#[derive(Debug, Clone, Copy)]
enum Shape {
    Eq,
    Between,
    OneSided,
}

impl Shape {
    /// Shape-Selektivität (kleiner = selektiver = günstiger).
    fn selectivity(self) -> f64 {
        match self {
            Shape::Eq => 0.1,
            Shape::Between => 0.25,
            Shape::OneSided => 0.5,
        }
    }
}

/// Bestimmt den Selektivitäts-Shape eines Feldes. Ein `Eq`-Literal dominiert
/// (Selektivität 0.1); sonst gilt ein Feld mit beiden Bounds als `Between`,
/// mit nur einem Bound als einseitige Range.
fn field_shape(lits: &[Lit], field: &str, lower: &Bound, upper: &Bound) -> Shape {
    use Bound::*;
    let has_eq = lits
        .iter()
        .any(|l| l.indexable() && l.field == field && l.cmp == Cmp::Eq);
    if has_eq {
        return Shape::Eq;
    }
    match (lower, upper) {
        (Unbounded, _) | (_, Unbounded) => Shape::OneSided,
        _ => Shape::Between,
    }
}

/// Ist `(la, ua)` ein strikt engerer gebundener Bereich als `(lb, ub)`?
/// Erst das untere Bound (höher = enger), dann das obere Bound (niedriger = enger).
fn tighter(la: &Bound, ua: &Bound, lb: &Bound, ub: &Bound) -> bool {
    if lower_stronger(la, lb) {
        return true;
    }
    if lower_stronger(lb, la) {
        return false;
    }
    upper_stronger(ua, ub)
}

/// Wählt aus den indexierbaren Feldern einer Klausel den günstigsten Index
/// (cost-basiert, deterministisch).
///
/// Kostenmodell: `cost = BASE_CARDINALITY * selectivity(shape)` mit
/// `Eq < Between < OneSided`. Bei gleicher Selektivität gewinnt das enger
/// gebundene Literal; letzter Tie-Break ist das lex kleinstes Feldname.
fn pick_index_field_cost<'a>(
    lits: &'a [Lit],
    schema: &Schema,
    collection_id: u32,
) -> Option<&'a str> {
    let has_index = |l: &'a Lit| -> bool {
        if !l.indexable() {
            return false;
        }
        schema
            .lookup_field_id(collection_id, &l.field)
            .and_then(|fid| schema.find_index(collection_id, fid))
            .map(|idx| idx.status == IndexStatus::Ready)
            .unwrap_or(false)
    };

    let mut fields: Vec<&str> = lits
        .iter()
        .filter(|l| has_index(l))
        .map(|l| l.field.as_str())
        .collect();
    fields.sort_unstable();
    fields.dedup();

    let mut best: Option<(&str, Shape, Bound, Bound)> = None;
    for field in fields {
        let (lower, upper) = merge_bounds(lits, field);
        let shape = field_shape(lits, field, &lower, &upper);
        let cand = (field, shape, lower, upper);
        best = Some(match best {
            None => cand,
            Some(b) => {
                if cheaper(&cand, &b) {
                    cand
                } else {
                    b
                }
            }
        });
    }
    best.map(|(field, _, _, _)| field)
}

/// Kosten-Vergleich zweier Kandidaten (deterministische Total-Ordnung).
///
/// Zuerst zählt die Shape-Selektivität (kleiner = besser); bei gleicher
/// Selektivität die engere Bounds; zuletzt das lex kleinstes Feldname.
fn cheaper(a: &(&str, Shape, Bound, Bound), b: &(&str, Shape, Bound, Bound)) -> bool {
    let cost_a = BASE_CARDINALITY * a.1.selectivity();
    let cost_b = BASE_CARDINALITY * b.1.selectivity();
    if cost_a < cost_b {
        return true;
    }
    if cost_a > cost_b {
        return false;
    }
    if tighter(&a.2, &a.3, &b.2, &b.3) {
        return true;
    }
    if tighter(&b.2, &b.3, &a.2, &a.3) {
        return false;
    }
    a.0 < b.0
}

/// Vergleich zweier Bound-Werte für das "engere" untere Bound (größer ist enger).
fn lower_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    ordering::value_cmp(a, b)
}

/// Ist `a` ein strikt engeres **unteres** Bound als `b`? Größerer Wert ist
/// enger; bei Gleichstand ist `Inclusive` enger als `Exclusive`.
fn lower_stronger(a: &Bound, b: &Bound) -> bool {
    use Bound::*;
    use std::cmp::Ordering;
    match (a, b) {
        (Unbounded, _) => false,
        (_, Unbounded) => true,
        (Inclusive(x), Inclusive(y)) | (Exclusive(x), Exclusive(y)) => {
            lower_cmp(x, y) == Ordering::Greater
        }
        (Inclusive(x), Exclusive(y)) | (Exclusive(x), Inclusive(y)) => match lower_cmp(x, y) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => matches!(a, Inclusive(_)),
        },
    }
}

/// Ist `a` ein strikt engeres **oberes** Bound als `b`? Kleinerer Wert ist
/// enger; bei Gleichstand ist `Exclusive` enger als `Inclusive`.
fn upper_stronger(a: &Bound, b: &Bound) -> bool {
    use Bound::*;
    use std::cmp::Ordering;
    match (a, b) {
        (Unbounded, _) => false,
        (_, Unbounded) => true,
        (Inclusive(x), Inclusive(y)) | (Exclusive(x), Exclusive(y)) => {
            lower_cmp(x, y) == Ordering::Less
        }
        (Inclusive(x), Exclusive(y)) | (Exclusive(x), Inclusive(y)) => match lower_cmp(x, y) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => matches!(a, Exclusive(_)),
        },
    }
}

/// Engeres unteres Bound behalten.
fn tighten_lower(cur: &Bound, next: &Bound) -> Bound {
    if lower_stronger(next, cur) {
        next.clone()
    } else {
        cur.clone()
    }
}

/// Engeres oberes Bound behalten.
fn tighten_upper(cur: &Bound, next: &Bound) -> Bound {
    if upper_stronger(next, cur) {
        next.clone()
    } else {
        cur.clone()
    }
}

/// Merged Bounds aus den indexierbaren Literalen eines Feldes.
fn merge_bounds(lits: &[Lit], field: &str) -> (Bound, Bound) {
    use Bound::*;
    let mut lower: Bound = Unbounded;
    let mut upper: Bound = Unbounded;
    for l in lits {
        if !l.indexable() || l.field != field {
            continue;
        }
        match l.cmp {
            Cmp::Eq => {
                lower = tighten_lower(&lower, &Inclusive(l.value.clone()));
                upper = tighten_upper(&upper, &Inclusive(l.value.clone()));
            }
            Cmp::Gt => lower = tighten_lower(&lower, &Exclusive(l.value.clone())),
            Cmp::Gte => lower = tighten_lower(&lower, &Inclusive(l.value.clone())),
            Cmp::Lt => upper = tighten_upper(&upper, &Exclusive(l.value.clone())),
            Cmp::Lte => upper = tighten_upper(&upper, &Inclusive(l.value.clone())),
            Cmp::Ne => {}
        }
    }
    (lower, upper)
}

/// Baut aus den (nicht abgedeckten) Literalen einer Klausel ein Prädikat.
fn clause_residual(lits: &[Lit]) -> Predicate {
    let mut iter = lits.iter();
    let first = iter.next().expect("clause has literals");
    let mut p = first.to_predicate();
    for l in iter {
        p = p.and(l.to_predicate());
    }
    p
}

/// Erzeugt den Zeilen-/Zweig-Plan (ID-Source) für eine einzelne AND-Klausel.
///
/// Gibt `(row_source, residual_lits)` zurück, wobei `row_source` eine
/// Entity-Zeilenquelle ist: `Fetch{IndexScan}` oder `FullScan`.
fn plan_clause(
    schema: &Schema,
    collection: &str,
    collection_id: u32,
    clause: &[Lit],
) -> (PhysicalPlan, Vec<Lit>) {
    if let Some(field) = pick_index_field_cost(clause, schema, collection_id) {
        let (lower, upper) = merge_bounds(clause, field);
        let index = PhysicalPlan::IndexScan {
            collection: collection.to_string(),
            field: field.to_string(),
            lower,
            upper,
        };
        let fetch = PhysicalPlan::Fetch {
            input: Box::new(index),
            collection: collection.to_string(),
        };
        let residual: Vec<Lit> = clause
            .iter()
            .filter(|l| !(l.field == field && l.indexable()))
            .cloned()
            .collect();
        (fetch, residual)
    } else {
        let scan = PhysicalPlan::FullScan {
            collection: collection.to_string(),
        };
        (scan, clause.to_vec())
    }
}

/// Merge für den OR-Fall: vereinigt die Bounds eines Feldes über alle Klauseln
/// (Union = der **weiteste** Bereich, der jede Klausel-Range überdeckt).
fn union_bounds(clauses: &[Vec<Lit>], field: &str) -> (Bound, Bound) {
    use Bound::*;
    let mut lower: Option<Bound> = None;
    let mut upper: Option<Bound> = None;
    for clause in clauses {
        let (l, u) = merge_bounds(clause, field);
        lower = Some(match lower {
            None => l,
            Some(cur) => {
                if lower_stronger(&l, &cur) {
                    cur
                } else {
                    l
                }
            }
        });
        upper = Some(match upper {
            None => u,
            Some(cur) => {
                if upper_stronger(&u, &cur) {
                    cur
                } else {
                    u
                }
            }
        });
    }
    (lower.unwrap_or(Unbounded), upper.unwrap_or(Unbounded))
}

/// Enablement-Regel für `IndexOrderScan` (v0.6, Teil 3): Das Sortierfeld muss
/// in **jeder** DNF-Klausel ein positives, indexierbares Literal tragen und
/// einen READY-Index haben. Nur dann enthalten alle möglicherweise treffenden
/// Zeilen das Feld, und der geordnete Index-Scan ist exakt äquivalent zum
/// `Sort`-Fallback (keine Missing-Field-Verschiebung).
fn index_order_enabled(
    schema: &Schema,
    collection_id: Option<u32>,
    field: &str,
    clauses: &[Vec<Lit>],
) -> bool {
    let Some(cid) = collection_id else {
        return false;
    };
    let has_ready_index = schema
        .lookup_field_id(cid, field)
        .and_then(|fid| schema.find_index(cid, fid))
        .map(|idx| idx.status == IndexStatus::Ready)
        .unwrap_or(false);
    if !has_ready_index {
        return false;
    }
    clauses
        .iter()
        .all(|clause| clause.iter().any(|l| l.field == field && l.indexable()))
}

/// Plante einen Logical Plan zu einem Physical Plan (read-only, mutiert nichts).
pub fn plan(schema: &Schema, logical: LogicalPlan) -> PhysicalPlan {
    // 1) Logical-Plan in (Collection, Prädikat, Sort, Limit) zerlegen.
    let mut collection = String::new();
    let mut predicates: Vec<Predicate> = Vec::new();
    let mut sort: Option<(String, SortDir)> = None;
    let mut limit: Option<usize> = None;

    {
        let mut stack: Vec<LogicalPlan> = vec![logical];
        while let Some(node) = stack.pop() {
            match node {
                LogicalPlan::Collection { name } => collection = name,
                LogicalPlan::Filter { input, pred } => {
                    predicates.push(pred);
                    stack.push(*input);
                }
                LogicalPlan::Sort { input, field, dir } => {
                    sort = Some((field, dir));
                    stack.push(*input);
                }
                LogicalPlan::Limit { input, n } => {
                    limit = Some(n);
                    stack.push(*input);
                }
            }
        }
    }

    let combined = predicates.into_iter().reduce(Predicate::and);
    let clauses: Vec<Vec<Lit>> = match &combined {
        Some(p) => dnf(p),
        None => vec![vec![]], // keine Filter → eine leere Klausel (immer true)
    };

    let collection_id = schema.lookup_collection_id(&collection);

    // 2) Jede Klausel planen.
    let mut rows: PhysicalPlan;

    if clauses.len() == 1 {
        let clause = &clauses[0];
        let (source, residual) = match collection_id {
            Some(cid) => plan_clause(schema, &collection, cid, clause),
            // Collection existiert nicht → leerer FullScan, keine Indizes.
            None => (
                PhysicalPlan::FullScan {
                    collection: collection.clone(),
                },
                clause.clone(),
            ),
        };
        rows = source;
        if !residual.is_empty() {
            rows = PhysicalPlan::Filter {
                input: Box::new(rows),
                pred: clause_residual(&residual),
            };
        }
    } else {
        // OR: Union der Klausel-Zweige + Dedup.
        let mut branches: Vec<PhysicalPlan> = Vec::new();
        for clause in &clauses {
            let (source, _residual) = match collection_id {
                Some(cid) => plan_clause(schema, &collection, cid, clause),
                None => (
                    PhysicalPlan::FullScan {
                        collection: collection.clone(),
                    },
                    clause.clone(),
                ),
            };
            branches.push(source);
        }
        rows = PhysicalPlan::UnionIds { branches };
        // Residual = das VOLLE Prädikat. Das ist immer korrekt: Eine OR-Klausel,
        // die komplett über einen Index abgedeckt ist, hätte ein leeres
        // Klausel-Residual; würden wir dieses weglassen, fielen ihre Treffer
        // fälschlich heraus (Or(x, true) ≠ Or(x)). Der Index dient hier nur zur
        // Kandidatenreduktion, der Filter re-prüft exakt die Query-Semantik.
        if let Some(p) = combined.clone() {
            rows = PhysicalPlan::Filter {
                input: Box::new(rows),
                pred: p,
            };
        }
    }

    // 3) Sort / Limit. Bei erfüllter Enablement-Regel (`ORDER BY indexed_field
    //    LIMIT n` mit Presence-Garantie) wird Sort entfernt und ein
    //    `IndexOrderScan` eingesetzt — bounded, statt `Sort` über allen N.
    if let Some((field, dir)) = sort {
        if let (Some(n), true) = (
            limit,
            index_order_enabled(schema, collection_id, &field, &clauses),
        ) {
            let (lower, upper) = union_bounds(&clauses, &field);
            let mut inner = PhysicalPlan::IndexOrderScan {
                collection: collection.clone(),
                field: field.clone(),
                lower,
                upper,
                dir,
            };
            // Residual-Filter: bei Ein-Klausel-DNF die übrigen Literale;
            // bei OR das volle Prädikat (exakt, wie im UnionIds-Pfad).
            let filter_pred = if clauses.len() == 1 {
                let residual: Vec<Lit> = clauses[0]
                    .iter()
                    .filter(|l| !(l.field == field && l.indexable()))
                    .cloned()
                    .collect();
                if residual.is_empty() {
                    None
                } else {
                    Some(clause_residual(&residual))
                }
            } else {
                combined.clone()
            };
            if let Some(p) = filter_pred {
                inner = PhysicalPlan::Filter {
                    input: Box::new(inner),
                    pred: p,
                };
            }
            rows = PhysicalPlan::Limit {
                input: Box::new(inner),
                n,
            };
            return rows;
        }
        rows = PhysicalPlan::Sort {
            input: Box::new(rows),
            field,
            dir,
        };
    }
    if let Some(n) = limit {
        rows = PhysicalPlan::Limit {
            input: Box::new(rows),
            n,
        };
    }

    rows
}
