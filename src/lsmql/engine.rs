//! LSMQL Engine — Public API (Gate 3: kein eigener Planner).
//!
//! Die Engine übersetzt LSMQL-Text in den bestehenden `QueryBuilder` und nutzt
//! ausschließlich `EntityStore::execute_query` / `execute_aggregate` /
//! `explain_query`. Sie erfindet keine zweite Ausführungssemantik.

use std::collections::HashMap;

use crate::entity::EntityStore;
use crate::lsmql::ast::*;
use crate::lsmql::error::*;
use crate::lsmql::lexer::tokenize;
use crate::lsmql::parser::parse;
use crate::lsmql::translate::to_builder;
use crate::lsmql::validate::validate;

/// Ergebnis einer LSMQL-Abfrage.
#[derive(Debug, Clone)]
pub enum QueryResult {
    /// Zeilen-Ergebnis (`SELECT ...`, keine Aggregation).
    Rows(Vec<(String, crate::entity::Entity)>),
    /// Skalar-Ergebnis (`SELECT COUNT(*)` etc.).
    Scalar(Option<crate::codec::Value>),
}

/// Führt eine LSMQL-Abfrage aus.
///
/// Pipeline: tokenize → parse → Parameter-Substitution → Semantic Validation
/// → QueryBuilder → `EntityStore::execute_query`/`execute_aggregate`.
pub fn run(
    store: &mut EntityStore,
    query: &str,
    params: &HashMap<String, crate::codec::Value>,
) -> LsmqlResult<QueryResult> {
    let tokens = tokenize(query)?;
    let mut q = parse(&tokens)?;
    substitute_params(&mut q, params)?;
    validate(&q, store.schema())?;

    let builder = to_builder(&q)?;

    if q.explain {
        // EXPLAIN sollte nicht ausführen; aber `run` ohne Explain-Flag ist
        // hier nicht gemeint. Wir behandeln Explain separat (siehe `explain`).
        return Err(LsmqlError::Unsupported {
            feature: "EXPLAIN in run()".into(),
            reason: "use lsmql::explain() instead".into(),
        });
    }

    if builder.aggregation.is_some() {
        let res = store
            .execute_aggregate(builder)
            .map_err(|e| LsmqlError::Execution {
                message: e.to_string(),
            })?;
        Ok(QueryResult::Scalar(res))
    } else {
        let rows = store
            .execute_query(builder)
            .map_err(|e| LsmqlError::Execution {
                message: e.to_string(),
            })?;
        // OFFSET anwenden (QueryBuilder kennt kein Offset; wir skippen hier).
        let rows = if q.offset > 0 {
            rows.into_iter().skip(q.offset).collect()
        } else {
            rows
        };
        Ok(QueryResult::Rows(rows))
    }
}

/// Liefert den Physical-Plan einer LSMQL-Abfrage als Text (via
/// `EntityStore::explain_query`).
pub fn explain(
    store: &EntityStore,
    query: &str,
    params: &HashMap<String, crate::codec::Value>,
) -> LsmqlResult<String> {
    let tokens = tokenize(query)?;
    let mut q = parse(&tokens)?;
    substitute_params(&mut q, params)?;
    validate(&q, store.schema())?;
    let builder = to_builder(&q)?;
    store
        .explain_query(&builder)
        .map_err(|e| LsmqlError::Execution {
            message: e.to_string(),
        })
}

/// Ersetzt alle `$name`-Parameter im AST durch die Werte aus `params`.
/// Ein nicht gebundener Parameter ist ein Fehler (Gate 4:
/// `UnboundParameter`).
fn substitute_params(
    q: &mut Query,
    params: &HashMap<String, crate::codec::Value>,
) -> LsmqlResult<()> {
    if let Some(e) = &mut q.predicate {
        subst_expr(e, params)?;
    }
    Ok(())
}

fn subst_expr(e: &mut Expr, params: &HashMap<String, crate::codec::Value>) -> LsmqlResult<()> {
    match e {
        Expr::And(v) | Expr::Or(v) => {
            for s in v {
                subst_expr(s, params)?;
            }
            Ok(())
        }
        Expr::Not(b) => subst_expr(b, params),
        Expr::Pred(p) => subst_pred(p, params),
    }
}

fn subst_pred(p: &mut Predicate, params: &HashMap<String, crate::codec::Value>) -> LsmqlResult<()> {
    match p {
        Predicate::Eq(_, v)
        | Predicate::Ne(_, v)
        | Predicate::Lt(_, v)
        | Predicate::Le(_, v)
        | Predicate::Gt(_, v)
        | Predicate::Ge(_, v) => subst_value(v, params),
        Predicate::In(_, vals) => {
            for v in vals {
                subst_value(v, params)?;
            }
            Ok(())
        }
        Predicate::IsNull(_) | Predicate::IsAbsent(_) => Ok(()),
    }
}

fn subst_value(v: &mut Value, params: &HashMap<String, crate::codec::Value>) -> LsmqlResult<()> {
    if let Value::Param(name) = v {
        let val = params.get(name).cloned().ok_or_else(|| {
            LsmqlError::Semantic(SemanticError::UnboundParameter { name: name.clone() })
        })?;
        *v = db_value_to_lsmql(val);
    }
    Ok(())
}

fn db_value_to_lsmql(v: crate::codec::Value) -> Value {
    match v {
        crate::codec::Value::String(s) => Value::String(s),
        crate::codec::Value::Int(i) => Value::Number(i as f64),
        crate::codec::Value::Float(f) => Value::Number(f),
        crate::codec::Value::Bool(b) => Value::Bool(b),
        crate::codec::Value::Null => Value::Null,
        crate::codec::Value::Bytes(_) => Value::Null,
    }
}
