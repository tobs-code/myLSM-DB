//! Executor: führt einen [`PhysicalPlan`] gegen die Entity-/Index-Layer aus.
//!
//! ## Datenfluss (Pull-Modell)
//!
//! ```text
//! IndexScan → Ids → Fetch → (id, Entity)
//! FullScan  → (id, Entity)  [streamt]
//! UnionIds  → merge (id, Entity) + Dedup per ID
//! Filter    → Zeilen streamen + filtern
//! Sort      → blockiert (sammelt alle Zeilen, sortiert)
//! Limit     → `take(n)`, hört früh auf zu ziehen
//! ```
//!
//! `IndexScan` erzeugt nur IDs; `Fetch` materialisiert sie (und filtert nie
//! implizit). `Filter` ist die einzige Stelle mit Filter-Semantik. Operatoren
//! sind als `Iterator`s verkettet, sodass `Limit` nach `n` Zeilen nicht weiter
//! aus den Quellen zieht; nur `Sort` muss wegen der Gesamtsortierung
//! blockieren.

use std::collections::HashSet;

use crate::codec;
use crate::entity::{Entity, core_get_entity};
use crate::error::{Error, Result};
use crate::index;
use crate::keycodec;
use crate::ordering;
use crate::schema::Schema;
use crate::{Database, DirectMutator, ScanStream};

use super::logical::SortDir;
use super::physical::PhysicalPlan;

/// Stream über `(id, Entity)`-Zeilen. Bietet die Operator-Verkettung.
type RowStream<'db> = Box<dyn Iterator<Item = Result<(String, Entity)>> + 'db>;

/// Führt den Plan gegen `db`/`schema` aus und sammelt alle Zeilen ein.
/// Collection-Namen werden über das Schema aufgelöst; eine nicht existierende
/// Collection liefert leer.
pub fn run(
    db: &mut Database,
    schema: &Schema,
    plan: &PhysicalPlan,
) -> Result<Vec<(String, Entity)>> {
    exec_rows(db, schema, plan)?.collect()
}

/// Baut den Iterator für einen Plan-Knoten (Pull-Modell).
fn exec_rows<'db>(
    db: &'db mut Database,
    schema: &'db Schema,
    plan: &PhysicalPlan,
) -> Result<RowStream<'db>> {
    match plan {
        PhysicalPlan::FullScan { collection } => scan_collection_stream(db, schema, collection),
        PhysicalPlan::IndexScan { .. } => {
            let ids = candidate_ids(db, schema, plan)?;
            let collection = plan.collection().unwrap_or("");
            Ok(fetch_stream(db, schema, ids, collection))
        }
        PhysicalPlan::Fetch { input, collection } => {
            let ids = candidate_ids(db, schema, input)?;
            Ok(fetch_stream(db, schema, ids, collection))
        }
        PhysicalPlan::UnionIds { branches } => {
            // Jeder Zweig ist eine Entity-Zeilenquelle (Fetch{IndexScan} oder
            // FullScan). Da nur EIN `&mut db` gleichzeitig existieren kann,
            // werden die Zweige eager eingesammelt und per Entity-ID dedupliziert.
            let mut out: Vec<(String, Entity)> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for branch in branches {
                for (id, e) in exec_rows(db, schema, branch)?.collect::<Result<Vec<_>>>()? {
                    if seen.insert(id.clone()) {
                        out.push((id, e));
                    }
                }
            }
            Ok(Box::new(out.into_iter().map(Ok)))
        }
        PhysicalPlan::Filter { input, pred } => {
            let inner = exec_rows(db, schema, input)?;
            let pred = pred.clone();
            Ok(Box::new(inner.filter(move |r| {
                r.as_ref()
                    .is_ok_and(|(_, e)| super::expression::eval(e, &pred).unwrap_or(false))
            })))
        }
        PhysicalPlan::Sort { input, field, dir } => {
            let inner = exec_rows(db, schema, input)?;
            let mut rows: Vec<(String, Entity)> = inner.collect::<Result<Vec<_>>>()?;
            sort_rows(&mut rows, field, *dir);
            Ok(Box::new(rows.into_iter().map(Ok)))
        }
        PhysicalPlan::Limit { input, n } => {
            let inner = exec_rows(db, schema, input)?;
            Ok(Box::new(inner.take(*n)))
        }
    }
}

/// Erzeugt die Kandidaten-IDs eines id-produzierenden Plans (IndexScan /
/// UnionIds). Eager, weil `Fetch` die IDs besitzt und über sie die Entities
/// einzeln (Punkt-Lookup) nachlädt.
fn candidate_ids(db: &mut Database, schema: &Schema, plan: &PhysicalPlan) -> Result<Vec<String>> {
    match plan {
        PhysicalPlan::IndexScan {
            collection,
            field,
            lower,
            upper,
        } => index_scan(db, schema, collection, field, lower, upper),
        PhysicalPlan::UnionIds { branches } => {
            let mut out: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for branch in branches {
                for id in candidate_ids(db, schema, branch)? {
                    if seen.insert(id.clone()) {
                        out.push(id);
                    }
                }
            }
            Ok(out)
        }
        other => Err(Error::InvalidFormat(format!(
            "plan is not id-producing: {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

/// Streamt die Entities einer Collection (FullScan) direkt aus dem Lazy-Scan
/// der Storage-Schicht — **ohne** die Datenmenge zu materialisieren.
fn scan_collection_stream<'db>(
    db: &'db mut Database,
    schema: &'db Schema,
    collection: &str,
) -> Result<RowStream<'db>> {
    let Some(cid) = schema.lookup_collection_id(collection) else {
        return Ok(Box::new(std::iter::empty()));
    };
    let pstart = keycodec::collection_prefix(cid);
    let pend = keycodec::successor(&pstart);
    let stream = db.scan_stream(Some(&pstart), pend.as_deref())?;
    Ok(Box::new(ScanAssembler {
        stream: Box::new(stream),
        schema,
        cid,
        current: None,
    }))
}

/// Streamt Kandidaten-IDs zu Entities (je Punkt-Lookup). Eine gelöschte/nicht
/// gefundene Entität entfällt; gefiltert wird hier nie implizit.
fn fetch_stream<'db>(
    db: &'db mut Database,
    schema: &'db Schema,
    ids: Vec<String>,
    collection: &str,
) -> RowStream<'db> {
    Box::new(FetchIter {
        db,
        schema,
        cid: schema.lookup_collection_id(collection),
        ids: ids.into_iter(),
    })
}

/// Streamt die (sortierten) Feld-Rows einer Collection zu Entities: gruppiert
/// die zusammenliegenden Feld-Keys einer Entität, emittiert beim Wechsel der
/// Entity-ID. Komplett gelöschte (nur Tombstone-)Entitäten entfallen.
struct ScanAssembler<'db> {
    stream: ScanStream<'db>,
    schema: &'db Schema,
    cid: u32,
    current: Option<(String, Entity)>,
}

impl<'db> Iterator for ScanAssembler<'db> {
    type Item = Result<(String, Entity)>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.stream.next() {
                None => {
                    let cur = self.current.take();
                    return cur.filter(|(_, e)| !e.fields.is_empty()).map(Ok);
                }
                Some(Err(e)) => return Some(Err(e)),
                Some(Ok((key, value))) => {
                    let Some((_, ee, ef)) = keycodec::decode_entity_key(&key) else {
                        continue;
                    };
                    let Some(bytes) = value else { continue }; // Tombstone-Feld.
                    let Ok(val) = codec::decode(&bytes) else {
                        return Some(Err(Error::InvalidFormat("bad field value".into())));
                    };
                    let Some(name) = self.schema.field_name(self.cid, ef) else {
                        return Some(Err(Error::InvalidFormat(format!("unknown field id {ef}"))));
                    };
                    let eid = String::from_utf8_lossy(ee).into_owned();
                    let new_entity = {
                        let mut ent = Entity::new();
                        ent.fields.push((name.to_string(), val.clone()));
                        ent
                    };
                    match &mut self.current {
                        Some((cur_id, ent)) if *cur_id == eid => {
                            ent.fields.push((name.to_string(), val));
                        }
                        _ => {
                            let prev = self.current.replace((eid, new_entity));
                            if let Some(prev) = prev {
                                if !prev.1.fields.is_empty() {
                                    return Some(Ok(prev));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Streamt IDs → Entities (Punkt-Lookup).
struct FetchIter<'db> {
    db: &'db mut Database,
    schema: &'db Schema,
    cid: Option<u32>,
    ids: std::vec::IntoIter<String>,
}

impl<'db> Iterator for FetchIter<'db> {
    type Item = Result<(String, Entity)>;
    fn next(&mut self) -> Option<Self::Item> {
        let cid = self.cid?;
        for id in self.ids.by_ref() {
            let mut m = DirectMutator { db: &mut *self.db };
            match core_get_entity(self.schema, &mut m, cid, id.as_bytes()) {
                Ok(Some(e)) => return Some(Ok((id, e))),
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
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
