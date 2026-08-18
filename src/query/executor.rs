//! Executor: führt einen [`PhysicalPlan`] gegen die Entity-/Index-Layer aus.
//!
//! ## Datenfluss
//!
//! ```text
//! IndexScan → Ids → Fetch → (id, Entity)
//! FullScan  → (id, Entity)
//! UnionIds  → merge (id, Entity) + Dedup per ID
//! Filter / Sort / Limit → Zeilen weiterverarbeiten
//! ```
//!
//! `IndexScan` erzeugt nur IDs; `Fetch` materialisiert sie (und filtert nie
//! implizit). `Filter` ist die einzige Stelle mit Filter-Semantik. Falls der
//! Plan an der Wurzel ein nackter `IndexScan` wäre, werden die IDs sofort
//! gefetcht (Robustheit, kommt in geplanten Plänen nicht vor).

use std::collections::HashSet;

use crate::Database;
use crate::entity::Entity;
use crate::error::{Error, Result};
use crate::index;
use crate::ordering;
use crate::schema::Schema;

use super::logical::SortDir;
use super::physical::PhysicalPlan;

/// Zwischenergebnis eines Operators.
enum Rows {
    /// Nur Entity-IDs (Kandidaten eines IndexScan).
    Ids(Vec<String>),
    /// Materialisierte `(id, Entity)`-Zeilen.
    Entities(Vec<(String, Entity)>),
}

/// Führt den Plan gegen `db`/`schema` aus. Die Collection-Namen werden über
/// das Schema aufgelöst; eine nicht existierende Collection liefert leer.
pub fn run(
    db: &mut Database,
    schema: &Schema,
    plan: &PhysicalPlan,
) -> Result<Vec<(String, Entity)>> {
    let rows = exec(db, schema, plan)?;
    Ok(match rows {
        Rows::Entities(v) => v,
        // Nur IDs an der Wurzel → materialisieren.
        Rows::Ids(ids) => fetch(db, schema, ids, plan.collection().unwrap_or(""))?,
    })
}

fn exec(db: &mut Database, schema: &Schema, plan: &PhysicalPlan) -> Result<Rows> {
    match plan {
        PhysicalPlan::FullScan { collection } => {
            let entities = scan_collection(db, schema, collection)?;
            Ok(Rows::Entities(entities))
        }
        PhysicalPlan::IndexScan {
            collection,
            field,
            lower,
            upper,
        } => {
            let ids = index_scan(db, schema, collection, field, lower, upper)?;
            Ok(Rows::Ids(ids))
        }
        PhysicalPlan::Fetch { input, collection } => {
            let inner = exec(db, schema, input)?;
            let ids = match inner {
                Rows::Ids(ids) => ids,
                Rows::Entities(rows) => {
                    // Fetch über bereits materialisierten Zeilen → nur IDs extrahieren.
                    rows.into_iter().map(|(id, _)| id).collect()
                }
            };
            let entities = fetch(db, schema, ids, collection)?;
            Ok(Rows::Entities(entities))
        }
        PhysicalPlan::UnionIds { branches } => {
            let mut out: Vec<(String, Entity)> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for branch in branches {
                match exec(db, schema, branch)? {
                    Rows::Entities(rows) => {
                        for (id, e) in rows {
                            if seen.insert(id.clone()) {
                                out.push((id, e));
                            }
                        }
                    }
                    Rows::Ids(ids) => {
                        for id in ids {
                            if seen.insert(id.clone())
                                && let Some(e) = core_get(db, schema, collection_of(branch), &id)?
                            {
                                out.push((id, e));
                            }
                        }
                    }
                }
            }
            Ok(Rows::Entities(out))
        }
        PhysicalPlan::Filter { input, pred } => {
            let inner = exec(db, schema, input)?;
            let rows = match inner {
                Rows::Entities(rows) => rows,
                Rows::Ids(ids) => {
                    let coll = plan.input().and_then(|i| i.collection()).unwrap_or("");
                    fetch(db, schema, ids, coll)?
                }
            };
            let filtered = rows
                .into_iter()
                .filter(|(_, e)| super::expression::eval(e, pred).unwrap_or(false))
                .collect();
            Ok(Rows::Entities(filtered))
        }
        PhysicalPlan::Sort { input, field, dir } => {
            let inner = exec(db, schema, input)?;
            let mut rows = match inner {
                Rows::Entities(rows) => rows,
                Rows::Ids(ids) => {
                    let coll = plan.input().and_then(|i| i.collection()).unwrap_or("");
                    fetch(db, schema, ids, coll)?
                }
            };
            sort_rows(&mut rows, field, *dir);
            Ok(Rows::Entities(rows))
        }
        PhysicalPlan::Limit { input, n } => {
            let inner = exec(db, schema, input)?;
            let rows = match inner {
                Rows::Entities(rows) => rows,
                Rows::Ids(ids) => {
                    let coll = plan.input().and_then(|i| i.collection()).unwrap_or("");
                    fetch(db, schema, ids, coll)?
                }
            };
            let mut rows = rows;
            rows.truncate(*n);
            Ok(Rows::Entities(rows))
        }
    }
}

fn collection_of(plan: &PhysicalPlan) -> &str {
    plan.collection().unwrap_or("")
}

/// Materialisiert IDs zu Entitäten (immer über die Entity — nie implizit
/// gefiltert, aber eine gelöschte/nicht gefundene Entity entfällt).
fn fetch(
    db: &mut Database,
    schema: &Schema,
    ids: Vec<String>,
    collection: &str,
) -> Result<Vec<(String, Entity)>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(e) = core_get(db, schema, collection, &id)? {
            out.push((id, e));
        }
    }
    Ok(out)
}

fn core_get(
    db: &mut Database,
    schema: &Schema,
    collection: &str,
    id: &str,
) -> Result<Option<Entity>> {
    let Some(cid) = schema.lookup_collection_id(collection) else {
        return Ok(None);
    };
    let mut m = crate::DirectMutator { db };
    crate::entity::core_get_entity(schema, &mut m, cid, id.as_bytes())
}

fn scan_collection(
    db: &mut Database,
    schema: &Schema,
    collection: &str,
) -> Result<Vec<(String, Entity)>> {
    let Some(cid) = schema.lookup_collection_id(collection) else {
        return Ok(Vec::new());
    };
    let mut m = crate::DirectMutator { db };
    crate::entity::core_scan_collection(schema, &mut m, cid)
}

fn index_scan(
    db: &mut Database,
    schema: &Schema,
    collection: &str,
    field: &str,
    lower: &index::Bound,
    upper: &index::Bound,
) -> Result<Vec<String>> {
    let Some(cid) = schema.lookup_collection_id(collection) else {
        return Ok(Vec::new());
    };
    let Some(fid) = schema.lookup_field_id(cid, field) else {
        return Err(Error::InvalidFormat(format!("unknown field {field}")));
    };
    if schema.find_index(cid, fid).is_none() {
        return Err(Error::InvalidFormat(format!("no index on field {field}")));
    }
    index::find(db, schema, cid, fid, lower, upper)
}

/// Deterministisches Sortieren nach `field`; bei Gleichstand über die
/// Entity-ID (immer aufsteigend). Ein fehlendes Feld sortiert als kleinstes
/// (Asc) bzw. größtes (Desc).
fn sort_rows(rows: &mut [(String, Entity)], field: &str, dir: SortDir) {
    use std::cmp::Ordering;
    let asc = dir == SortDir::Asc;
    rows.sort_by(|a, b| {
        let va = a.1.field(field);
        let vb = b.1.field(field);
        let ord = match (va, vb) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => {
                if asc {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (Some(_), None) => {
                if asc {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (Some(x), Some(y)) => {
                let o = ordering::value_cmp(x, y);
                if asc { o } else { o.reverse() }
            }
        };
        if ord != Ordering::Equal {
            ord
        } else {
            a.0.cmp(&b.0)
        }
    });
}
