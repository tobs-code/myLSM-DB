//! Regelbasierter Planner: `LogicalPlan` → `PhysicalPlan`.
//!
//! ## Regeln
//!
//! 1. Das Prädikat wird in **DNF** zerlegt (`OR` top-level, jede Klausel eine
//!    Konjunktion von Literalen).
//! 2. Pro AND-Klausel wird **ein** Index gewählt: ein Feld mit READY-Index, das
//!    ein indexierbares (nicht-negiertes, `!=`-freies) Prädikat trägt. Heuristik:
//!    `Eq`-Prädikat bevorzugt, sonst lexikographisch kleinstes Feldname
//!    (deterministisch). **Kein** Cost-Based, keine Cardinality.
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

/// Wählt aus den indexierbaren Feldern einer Klausel den zu verwendenden Index
/// (regelbasiert, deterministisch): `Eq` bevorzugt, sonst kleinstes Feldname.
fn pick_index_field<'a>(lits: &'a [Lit], schema: &Schema, collection_id: u32) -> Option<&'a str> {
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
    let mut eq_candidates: Vec<&Lit> = lits
        .iter()
        .filter(|l| l.indexable() && l.cmp == Cmp::Eq && has_index(l))
        .collect();
    eq_candidates.sort_by_key(|l| l.field.clone());
    if let Some(l) = eq_candidates.first() {
        return Some(&l.field);
    }
    let mut range_candidates: Vec<&Lit> = lits.iter().filter(|l| has_index(l)).collect();
    range_candidates.sort_by_key(|l| l.field.clone());
    range_candidates.first().map(|l| l.field.as_str())
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
    if let Some(field) = pick_index_field(clause, schema, collection_id) {
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
        if let Some(p) = combined {
            rows = PhysicalPlan::Filter {
                input: Box::new(rows),
                pred: p,
            };
        }
    }

    // 3) Sort / Limit.
    if let Some((field, dir)) = sort {
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
