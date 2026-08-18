//! Auswertung eines [`Predicate`] gegen eine einzelne Entität.
//!
//! Diese Evaluierung ist die **einzige** Quelle für Prädikat-Wahrheitswerte im
//! System (Oracle UND Executor nutzen dieselbe Funktion — Konsistenz per
//! Konstruktion).
//!
//! ## Missing-Field-Semantik (festgeschrieben, v0.5)
//!
//! Ein Feld, das in der Entität fehlt (bzw. gelöscht ist), verhält sich wie
//! `NULL` in dreiwerter Logik, reduziert auf Bool:
//!
//! ```text
//! Eq            → false
//! Ne            → true
//! Lt/Lte/Gt/Gte → false
//! ```
//!
//! `Not` ist strukturell kompositional: `Eval(Not(p)) = !Eval(p)`. Damit gilt
//! zwingend `Ne(f) ≡ Not(Eq(f))` für alle Fälle (present + missing), denn:
//! `Ne(missing)=true` und `Not(Eq(missing))=Not(false)=true`. So gibt es keine
//! zweite, abweichende Semantik für `Ne` vs. `Not(Eq)`.
//!
//! Alle Wertvergleiche nutzen [`crate::ordering::value_cmp`] — dieselbe totale
//! Ordnung wie Index-Scan und Entity-Range-Scan.

use crate::codec::Value;
use crate::entity::Entity;
use crate::error::Result;
use crate::ordering;

use super::ast::{Cmp, Predicate};

/// Wertet `pred` gegen `entity` aus.
pub fn eval(entity: &Entity, pred: &Predicate) -> Result<bool> {
    Ok(eval_inner(entity, pred))
}

fn eval_inner(entity: &Entity, pred: &Predicate) -> bool {
    match pred {
        Predicate::And(a, b) => eval_inner(entity, a) && eval_inner(entity, b),
        Predicate::Or(a, b) => eval_inner(entity, a) || eval_inner(entity, b),
        Predicate::Not(p) => !eval_inner(entity, p),
        Predicate::Field(name, cmp, value) => {
            let Some(actual) = entity.field(name) else {
                return missing_field(*cmp);
            };
            compare(actual, *cmp, value)
        }
    }
}

/// Wahrheitswert eines Vergleichs gegen ein **fehlendes** Feld.
fn missing_field(cmp: Cmp) -> bool {
    matches!(cmp, Cmp::Ne)
}

/// Vergleicht den tatsächlichen Wert mit dem Literal.
fn compare(actual: &Value, cmp: Cmp, lit: &Value) -> bool {
    use std::cmp::Ordering;
    let ord = ordering::value_cmp(actual, lit);
    match cmp {
        Cmp::Eq => ord == Ordering::Equal,
        Cmp::Ne => ord != Ordering::Equal,
        Cmp::Lt => ord == Ordering::Less,
        Cmp::Lte => ord != Ordering::Greater,
        Cmp::Gt => ord == Ordering::Greater,
        Cmp::Gte => ord != Ordering::Less,
    }
}
