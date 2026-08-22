//! AST → `my-lsm-db` QueryBuilder (Gate 3: kein eigener Planner).
//!
//! LSMQL erzeugt ausschließlich den bestehenden `QueryBuilder` aus
//! `my-lsm-db`. Der Planner (Composite-Index-Wahl etc.) bleibt die alleinige
//! Wahrheit. Das Mapping verliert keine Information (Gate 1): `And`/`Or`/`Not`
//! werden 1:1 auf die `Predicate`-Baumebene abgebildet.

use crate::codec::Value as DbValue;
use crate::lsmql::ast::*;
use crate::lsmql::error::*;
use crate::query::QueryBuilder;
use crate::query::ast::{Cmp, Predicate as DbPred};
use crate::query::logical::Aggregate;

/// Übersetzt eine (validierte) Query in einen `QueryBuilder`.
///
/// Liefert `Err(Unsupported)` für `IS ABSENT` (in v1 AST-reserviert, wie
/// `GROUP BY`) — der Executor/Planner von v1.3 hat keinen "absent"-Operator.
pub fn to_builder(q: &Query) -> LsmqlResult<QueryBuilder> {
    let mut b = QueryBuilder::new(&q.source);

    // WHERE → Predicate-Baum.
    if let Some(expr) = &q.predicate {
        let pred = to_pred(expr)?;
        b = b.filter(pred);
    }

    // ORDER BY.
    for o in &q.order_by {
        b = b.sort(&o.field, o.dir.as_query_dir());
    }

    // LIMIT / OFFSET (offset übernimmt der Executor nicht direkt; wir
    // simulieren es durch vorgeschaltetes Limit + Skip in `engine.rs`).
    if let Some(n) = q.limit {
        b = b.limit(n + q.offset);
    }

    // Projektion / Aggregation (wechselseitig exklusiv, wie in v0.8).
    match &q.projection {
        Projection::Star => {}
        Projection::Items(items) => {
            let fields: Vec<&str> = items
                .iter()
                .filter_map(|it| match it {
                    ProjItem::Field(f) => Some(f.as_str()),
                    ProjItem::Agg { .. } => None,
                })
                .collect();
            let aggs: Vec<&ProjItem> = items
                .iter()
                .filter(|it| matches!(it, ProjItem::Agg { .. }))
                .collect();
            if !aggs.is_empty() {
                // Aggregation ist der Terminal-Schritt.
                if aggs.len() != 1 || !fields.is_empty() {
                    // v0.8: Aggregation exklusiv zu Projektion.
                    // Wir erlauben nur rein-aggregierende Projektionen.
                    if !fields.is_empty() {
                        return Err(LsmqlError::Unsupported {
                            feature: "mixed projection + aggregation".into(),
                            reason: "use either SELECT fields OR SELECT agg (not both)".into(),
                        });
                    }
                }
                if let ProjItem::Agg { kind, field } = &aggs[0] {
                    b = b.aggregate(to_aggregate(*kind, field.clone()));
                }
            } else if !fields.is_empty() {
                b = b.project(&fields);
            }
        }
    }

    Ok(b)
}

/// Übersetzt den LSMQL-`Expr`-Baum in einen `my-lsm-db`-`Predicate`-Baum.
/// Verschachtelung (And/Or/Not) wird verlustfrei erhalten.
fn to_pred(e: &Expr) -> LsmqlResult<DbPred> {
    match e {
        Expr::And(subs) => {
            let mut iter = subs.iter();
            let first = to_pred(iter.next().ok_or_else(|| LsmqlError::Unsupported {
                feature: "empty AND".into(),
                reason: "parser should not produce this".into(),
            })?)?;
            // Unter-Prädikate dürfen `Unsupported` liefern (z.B. IS ABSENT
            // verschachtelt) — das muss sauber durchgereicht werden.
            let mut acc = first;
            for s in iter {
                acc = acc.and(to_pred(s)?);
            }
            Ok(acc)
        }
        Expr::Or(subs) => {
            let mut iter = subs.iter();
            let first = to_pred(iter.next().ok_or_else(|| LsmqlError::Unsupported {
                feature: "empty OR".into(),
                reason: "parser should not produce this".into(),
            })?)?;
            let mut acc = first;
            for s in iter {
                acc = acc.or(to_pred(s)?);
            }
            Ok(acc)
        }
        Expr::Not(b) => Ok(to_pred(b)?.negate()),
        Expr::Pred(p) => to_atom(p),
    }
}

/// Übersetzt ein atomares `Predicate`.
fn to_atom(p: &Predicate) -> LsmqlResult<DbPred> {
    match p {
        Predicate::Eq(f, v) => Ok(DbPred::field(f, Cmp::Eq, to_db_value(v)?)),
        Predicate::Ne(f, v) => Ok(DbPred::field(f, Cmp::Ne, to_db_value(v)?)),
        Predicate::Lt(f, v) => Ok(DbPred::field(f, Cmp::Lt, to_db_value(v)?)),
        Predicate::Le(f, v) => Ok(DbPred::field(f, Cmp::Lte, to_db_value(v)?)),
        Predicate::Gt(f, v) => Ok(DbPred::field(f, Cmp::Gt, to_db_value(v)?)),
        Predicate::Ge(f, v) => Ok(DbPred::field(f, Cmp::Gte, to_db_value(v)?)),
        Predicate::In(f, vals) => {
            // IN → OR-Kette von Eq (Candidate-Union, wie in Spec §6/§9).
            let mut iter = vals.iter();
            let first = DbPred::field(f, Cmp::Eq, to_db_value(iter.next().unwrap())?);
            Ok(iter.fold(first, |acc, v| {
                acc.or(DbPred::field(f, Cmp::Eq, to_db_value(v).unwrap()))
            }))
        }
        // IS NULL → explizit gespeicherter Nullwert (v1.2/v1.3-Semantik:
        // Value::Null ist ein echter, gespeicherter Wert).
        Predicate::IsNull(f) => Ok(DbPred::field(f, Cmp::Eq, DbValue::Null)),
        // IS ABSENT → in v1 nicht ausführbar (kein "absent"-Operator im
        // v1.3-Predicate). AST-reserviert wie GROUP BY.
        Predicate::IsAbsent(_f) => Err(LsmqlError::Unsupported {
            feature: "IS ABSENT".into(),
            reason: "v1.3 Predicate has no 'absent' operator; reserved for future".into(),
        }),
    }
}

fn to_aggregate(kind: AggKind, field: Option<String>) -> Aggregate {
    match kind {
        AggKind::Count => Aggregate::Count,
        AggKind::Sum => Aggregate::Sum(field.unwrap_or_default()),
        AggKind::Avg => Aggregate::Avg(field.unwrap_or_default()),
        AggKind::Min => Aggregate::Min(field.unwrap_or_default()),
        AggKind::Max => Aggregate::Max(field.unwrap_or_default()),
    }
}

/// LSMQL-`Value` → `my-lsm-db`-`Value`.
fn to_db_value(v: &Value) -> LsmqlResult<DbValue> {
    match v {
        Value::String(s) => Ok(DbValue::String(s.clone())),
        Value::Number(n) => {
            if n.fract() == 0.0 && n.abs() < (i64::MAX as f64) {
                Ok(DbValue::Int(*n as i64))
            } else {
                Ok(DbValue::Float(*n))
            }
        }
        Value::Bool(b) => Ok(DbValue::Bool(*b)),
        Value::Null => Ok(DbValue::Null),
        Value::Param(name) => Err(LsmqlError::Semantic(SemanticError::UnboundParameter {
            name: name.clone(),
        })),
    }
}
