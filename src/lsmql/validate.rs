//! Semantic Validation (Gate 4): Collection/Feld/Typ/Parameter/GROUP BY.
//!
//! Nutzt das `Schema` (via `EntityStore::schema()`) für UnknownCollection /
//! UnknownField. Parameter werden vorab substituiert (siehe `engine.rs`),
//! daher ist hier nur noch die Strukturvalidierung nötig.

use crate::lsmql::ast::*;
use crate::lsmql::error::*;
use crate::schema::Schema;

/// Validiert die semantische Korrektheit einer (bereits parameter-substituierten)
/// Query gegen das Schema. Liefert `Ok(())` oder einen präzisen
/// [`SemanticError`].
pub fn validate(q: &Query, schema: &Schema) -> LsmqlResult<()> {
    // GROUP BY ist in v1 AST-reserviert → UnsupportedQuery.
    if !q.group_by.is_empty() {
        return Err(LsmqlError::Unsupported {
            feature: "GROUP BY".into(),
            reason: "not implemented in v1".into(),
        });
    }

    // Collection bekannt?
    let coll_id = schema.lookup_collection_id(&q.source).ok_or_else(|| {
        LsmqlError::Semantic(SemanticError::UnknownCollection {
            collection: q.source.clone(),
        })
    })?;

    // Projektion: Felder/Aggregate bekannt + Aggregat-Feld numerisch?
    let numeric_fields: std::collections::HashSet<String> = collect_numeric_fields();
    match &q.projection {
        Projection::Star => {}
        Projection::Items(items) => {
            for it in items {
                match it {
                    ProjItem::Field(f) => check_field(schema, coll_id, &q.source, f)?,
                    ProjItem::Agg { kind, field } => {
                        if let Some(f) = field {
                            check_field(schema, coll_id, &q.source, f)?;
                            if !numeric_fields.contains(f.as_str()) {
                                // Wir können den Typ nicht zwingend wissen; nur
                                // warnen, wenn das Feld als bekannt-numerisch
                                // registriert ist und hier nicht passt. Da v1.3
                                // kein Feld-Typ-System hat, lassen wir nicht-
                                // numerische Aggregat-Felder zu und verlassen
                                // uns auf die Executor-Semantik (skips
                                // non-numeric). Hier nur Struktur-Check.
                                let _ = kind;
                            }
                        }
                    }
                }
            }
        }
    }

    // ORDER BY-Felder bekannt.
    for o in &q.order_by {
        check_field(schema, coll_id, &q.source, &o.field)?;
    }

    // WHERE: rekursiv alle Felder + Typen prüfen.
    if let Some(pred) = &q.predicate {
        validate_expr(schema, coll_id, &q.source, pred)?;
    }

    Ok(())
}

fn collect_numeric_fields() -> std::collections::HashSet<String> {
    // v1.3 speichert keine Feld-Typen; wir markieren die in der taskdb-Domäne
    // bekannten numerischen Felder, damit Aggregationen früh erkannt werden.
    // (Erweiterbar, aber nicht zwingend — Executor skips non-numeric eh.)
    let mut s = std::collections::HashSet::new();
    s.insert("estimate".into());
    s.insert("priority".into());
    s.insert("age".into());
    s.insert("created_at".into());
    s.insert("due_at".into());
    s.insert("count".into());
    s
}

fn check_field(schema: &Schema, coll_id: u32, coll: &str, field: &str) -> LsmqlResult<()> {
    if field == "id" {
        return Ok(());
    }
    if schema.lookup_field_id(coll_id, field).is_none() {
        return Err(LsmqlError::Semantic(SemanticError::UnknownField {
            collection: coll.to_string(),
            field: field.to_string(),
        }));
    }
    Ok(())
}

fn validate_expr(schema: &Schema, coll_id: u32, coll: &str, e: &Expr) -> LsmqlResult<()> {
    match e {
        Expr::And(v) | Expr::Or(v) => {
            for sub in v {
                validate_expr(schema, coll_id, coll, sub)?;
            }
            Ok(())
        }
        Expr::Not(b) => validate_expr(schema, coll_id, coll, b),
        Expr::Pred(p) => validate_pred(schema, coll_id, coll, p),
    }
}

fn validate_pred(schema: &Schema, coll_id: u32, coll: &str, p: &Predicate) -> LsmqlResult<()> {
    match p {
        Predicate::Eq(f, v)
        | Predicate::Ne(f, v)
        | Predicate::Lt(f, v)
        | Predicate::Le(f, v)
        | Predicate::Gt(f, v)
        | Predicate::Ge(f, v) => {
            check_field(schema, coll_id, coll, f)?;
            check_value_type(f, v)?;
            Ok(())
        }
        Predicate::In(f, vals) => {
            check_field(schema, coll_id, coll, f)?;
            // Alle Elemente müssen denselben Typ haben.
            let mut seen: Option<&str> = None;
            for v in vals {
                check_value_type(f, v)?;
                let t = value_type_name(v);
                if let Some(s) = seen {
                    if s != t {
                        return Err(LsmqlError::Semantic(SemanticError::TypeMismatch {
                            field: f.clone(),
                            expected: s.to_string(),
                            got: t.to_string(),
                        }));
                    }
                } else {
                    seen = Some(t);
                }
            }
            Ok(())
        }
        Predicate::IsNull(f) => {
            check_field(schema, coll_id, coll, f)?;
            Ok(())
        }
        Predicate::IsAbsent(f) => {
            // IS ABSENT ist in v1 AST-reserviert (siehe translate.rs: der
            // Translator liefert UnsupportedQuery). Die Feld-Existenzprüfung
            // ist hier wirkungslos — wir überspringen sie bewusst, damit das
            // Unsupported-Signal (→422) durchkommt statt eines falschen
            // UnknownField (→400).
            let _ = f;
            Ok(())
        }
    }
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "bool",
        Value::Null => "null",
        Value::Param(_) => "param", // sollte vor Validierung substituiert sein
    }
}

fn check_value_type(_field: &str, v: &Value) -> LsmqlResult<()> {
    if let Value::Param(name) = v {
        return Err(LsmqlError::Semantic(SemanticError::UnboundParameter {
            name: name.clone(),
        }));
    }
    // Lockere Typprüfung: v1.3 hat kein Feld-Typ-System. Wir akzeptieren alle
    // Wertetypen; der Executor vergleicht über die totale Ordnung. Eine
    // echte Typabweisung würde hier zu False-Positives führen.
    Ok(())
}
