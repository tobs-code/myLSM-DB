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

use crate::codec::{self, Value};
use crate::entity::{Entity, core_get_entity};
use crate::error::{Error, Result};
use crate::index;
use crate::keycodec;
use crate::ordering;
use crate::schema::Schema;
use crate::{Database, DirectMutator, Mutator, ScanStream};

use super::logical::{Aggregate, SortDir};
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
    let mut m = DirectMutator { db };
    run_m(&mut m, schema, plan)
}

/// Führt den Plan gegen eine beliebige `Mutator`-Sicht aus (committed via
/// [`DirectMutator`], transaktional via `TxMutator` mit Pending-Overlay).
/// Sammelet alle Zeilen ein; eine nicht existierende Collection liefert leer.
pub fn run_m<M: Mutator>(
    m: &mut M,
    schema: &Schema,
    plan: &PhysicalPlan,
) -> Result<Vec<(String, Entity)>> {
    exec_rows(m, schema, plan)?.collect()
}

/// Baut den Iterator für einen Plan-Knoten (Pull-Modell).
fn exec_rows<'m, M: Mutator>(
    m: &'m mut M,
    schema: &'m Schema,
    plan: &PhysicalPlan,
) -> Result<RowStream<'m>> {
    match plan {
        PhysicalPlan::FullScan { collection } => scan_collection_stream(m, schema, collection),
        PhysicalPlan::IndexOrderScan {
            collection,
            field,
            lower,
            upper,
            dir,
        } => index_order_stream(m, schema, collection, field, lower, upper, *dir),
        PhysicalPlan::IndexScan { .. } => {
            let ids = candidate_ids(m, schema, plan)?;
            let collection = plan.collection().unwrap_or("");
            Ok(fetch_stream(m, schema, ids, collection))
        }
        PhysicalPlan::CompositeIndexScan { .. } => {
            let ids = candidate_ids(m, schema, plan)?;
            let collection = plan.collection().unwrap_or("");
            Ok(fetch_stream(m, schema, ids, collection))
        }
        PhysicalPlan::Fetch { input, collection } => {
            let ids = candidate_ids(m, schema, input)?;
            Ok(fetch_stream(m, schema, ids, collection))
        }
        PhysicalPlan::UnionIds { branches } => {
            // Jeder Zweig ist eine Entity-Zeilenquelle (Fetch{IndexScan} oder
            // FullScan). Da nur EIN `&mut M` gleichzeitig existieren kann,
            // werden die Zweige eager eingesammelt und per Entity-ID dedupliziert.
            let mut out: Vec<(String, Entity)> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for branch in branches {
                for (id, e) in exec_rows(m, schema, branch)?.collect::<Result<Vec<_>>>()? {
                    if seen.insert(id.clone()) {
                        out.push((id, e));
                    }
                }
            }
            Ok(Box::new(out.into_iter().map(Ok)))
        }
        PhysicalPlan::Filter { input, pred } => {
            let inner = exec_rows(m, schema, input)?;
            let pred = pred.clone();
            Ok(Box::new(inner.filter(move |r| {
                r.as_ref()
                    .is_ok_and(|(_, e)| super::expression::eval(e, &pred).unwrap_or(false))
            })))
        }
        PhysicalPlan::Sort { input, field, dir } => {
            let inner = exec_rows(m, schema, input)?;
            let mut rows: Vec<(String, Entity)> = inner.collect::<Result<Vec<_>>>()?;
            sort_rows(&mut rows, field, *dir);
            Ok(Box::new(rows.into_iter().map(Ok)))
        }
        PhysicalPlan::Limit { input, n } => {
            let inner = exec_rows(m, schema, input)?;
            Ok(Box::new(inner.take(*n)))
        }
    }
}

/// Erzeugt die Kandidaten-IDs eines id-produzierenden Plans (IndexScan /
/// UnionIds). Eager, weil `Fetch` die IDs besitzt und über sie die Entities
/// einzeln (Punkt-Lookup) nachlädt.
fn candidate_ids<M: Mutator>(
    m: &mut M,
    schema: &Schema,
    plan: &PhysicalPlan,
) -> Result<Vec<String>> {
    match plan {
        PhysicalPlan::IndexScan {
            collection,
            field,
            lower,
            upper,
        } => index_scan(m, schema, collection, field, lower, upper),
        PhysicalPlan::CompositeIndexScan {
            collection,
            index_id,
            field_ids,
            leading,
        } => index_composite_scan(m, schema, collection, *index_id, field_ids, leading),
        PhysicalPlan::UnionIds { branches } => {
            let mut out: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for branch in branches {
                for id in candidate_ids(m, schema, branch)? {
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
/// der gewählten Mutator-Sicht — **ohne** die Datenmenge zu materialisieren.
fn scan_collection_stream<'m, M: Mutator>(
    m: &'m mut M,
    schema: &'m Schema,
    collection: &str,
) -> Result<RowStream<'m>> {
    let Some(cid) = schema.lookup_collection_id(collection) else {
        return Ok(Box::new(std::iter::empty()));
    };
    let pstart = keycodec::collection_prefix(cid);
    let pend = keycodec::successor(&pstart);
    let stream = m.scan(Some(&pstart), pend.as_deref())?;
    Ok(Box::new(ScanAssembler {
        stream: Box::new(stream),
        schema,
        cid,
        current: None,
    }))
}

/// Streamt Kandidaten-IDs zu Entities (je Punkt-Lookup). Eine gelöschte/nicht
/// gefundene Entität entfällt; gefiltert wird hier nie implizit.
fn fetch_stream<'m, M: Mutator>(
    m: &'m mut M,
    schema: &'m Schema,
    ids: Vec<String>,
    collection: &str,
) -> RowStream<'m> {
    Box::new(FetchIter {
        m,
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
                    let eid = match crate::keycodec::decode_entity_id(ee) {
                        Ok(s) => s.to_string(),
                        Err(e) => return Some(Err(e)),
                    };
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
struct FetchIter<'m, M: Mutator> {
    m: &'m mut M,
    schema: &'m Schema,
    cid: Option<u32>,
    ids: std::vec::IntoIter<String>,
}

impl<'m, M: Mutator> Iterator for FetchIter<'m, M> {
    type Item = Result<(String, Entity)>;
    fn next(&mut self) -> Option<Self::Item> {
        let cid = self.cid?;
        for id in self.ids.by_ref() {
            match core_get_entity(self.schema, &mut *self.m, cid, id.as_bytes()) {
                Ok(Some(e)) => return Some(Ok((id, e))),
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
}

fn index_scan<M: Mutator>(
    m: &mut M,
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
        return Err(Error::InvalidArgument(format!("unknown field {field}")));
    };
    if schema.find_index(cid, fid).is_none() {
        return Err(Error::InvalidArgument(format!("no index on field {field}")));
    }
    index::find_m(m, schema, cid, fid, lower, upper)
}

/// Führt einen Composite-Index-Scan aus und liefert die verifizierten
/// Entity-IDs. Die Feldnamen dienen nur dem Mapping; die Komponentenanzahl
/// (`field_ids.len()`) bestimmt die Dekodierung.
fn index_composite_scan<M: Mutator>(
    m: &mut M,
    schema: &Schema,
    collection: &str,
    index_id: u32,
    field_ids: &[String],
    leading: &[(usize, index::Bound, index::Bound)],
) -> Result<Vec<String>> {
    let Some(cid) = schema.lookup_collection_id(collection) else {
        return Ok(Vec::new());
    };
    index::find_composite_m(m, schema, cid, index_id, field_ids.len(), leading)
}

/// Baut den (bounded) Index-Order-Scan: sammelt die Kandidaten-IDs des
/// Index-Ranges `lower..upper` ein und emittiert sie **lazy** in exakt der
/// `sort_rows`-Ordnung (`dir`), wobei jede Kandidaten-Entity gegen ihren
/// echten Wert verifiziert wird (Index ist nie die Wahrheit). Ein
/// vorgeschaltetes `Limit` hört früh auf zu ziehen.
fn index_order_stream<'m, M: Mutator>(
    m: &'m mut M,
    schema: &'m Schema,
    collection: &str,
    field: &str,
    lower: &index::Bound,
    upper: &index::Bound,
    dir: SortDir,
) -> Result<RowStream<'m>> {
    let Some(cid) = schema.lookup_collection_id(collection) else {
        return Ok(Box::new(std::iter::empty()));
    };
    let Some(fid) = schema.lookup_field_id(cid, field) else {
        return Err(Error::InvalidArgument(format!("unknown field {field}")));
    };
    if schema.find_index(cid, fid).is_none() {
        return Err(Error::InvalidArgument(format!("no index on field {field}")));
    }
    let (start, end) = index::index_range(cid, fid, lower, upper);
    let stream = m.scan(Some(&start), end.as_deref())?;
    let mut cands: Vec<(Value, String)> = Vec::new();
    for row in stream {
        let (key, _) = row?;
        if let Some((value, eid)) = index::decode_index_key_value(&key)? {
            cands.push((value, eid));
        }
    }
    // Deterministische Ordnung identisch zu `sort_rows`: Wert nach `dir`,
    // bei Gleichstand Entity-ID (immer aufsteigend).
    let asc = dir == SortDir::Asc;
    cands.sort_by(|a, b| {
        let mut o = ordering::value_cmp(&a.0, &b.0);
        if !asc {
            o = o.reverse();
        }
        if o == std::cmp::Ordering::Equal {
            a.1.cmp(&b.1)
        } else {
            o
        }
    });
    Ok(Box::new(IndexOrderIter {
        m,
        schema,
        cid,
        fid,
        lower: lower.clone(),
        upper: upper.clone(),
        cands: cands.into_iter(),
    }))
}

/// Streamt Kandidaten → verifizierte, geordnete `(id, Entity)`-Zeilen.
struct IndexOrderIter<'m, M: Mutator> {
    m: &'m mut M,
    schema: &'m Schema,
    cid: u32,
    fid: u32,
    lower: index::Bound,
    upper: index::Bound,
    cands: std::vec::IntoIter<(Value, String)>,
}

impl<'m, M: Mutator> Iterator for IndexOrderIter<'m, M> {
    type Item = Result<(String, Entity)>;
    fn next(&mut self) -> Option<Self::Item> {
        for (_value, id) in self.cands.by_ref() {
            // Verifikation gegen die Entity (Index ist nie die Wahrheit):
            // fehlendes/gelöschtes Feld oder Wert außerhalb des Bereichs ⇒ skip.
            let ekey = keycodec::encode_entity_key(self.cid, id.as_bytes(), self.fid);
            let actual = match self.m.get(&ekey) {
                Ok(Some(bytes)) => match codec::decode(&bytes) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                },
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            };
            if !index::within(&actual, &self.lower, &self.upper) {
                continue;
            }
            match core_get_entity(self.schema, &mut *self.m, self.cid, id.as_bytes()) {
                Ok(Some(e)) => return Some(Ok((id, e))),
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
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

/// Projiziert jede Zeile auf die angeforderten Felder (Result-Form).
///
/// Die Reihenfolge der Ergebnisfelder folgt `fields`. Ein angefordertes Feld,
/// das auf einer Entity **fehlt** (`field()` = `None`), wird weggelassen; ein
/// vorhandenes `Value::Null` wird dagegen eingeschlossen. Dies ist bewusst
/// KEINE Decode-/Storage-Optimierung — die Entities werden vollständig
/// materialisiert und danach auf die angeforderten Felder reduziert.
pub fn project_rows(
    rows: Vec<(String, Entity)>,
    fields: &[String],
) -> Vec<(String, Entity)> {
    rows.into_iter()
        .map(|(id, e)| {
            let mut pe = Entity::new();
            for f in fields {
                if let Some(v) = e.field(f) {
                    pe.fields.push((f.clone(), v.clone()));
                }
            }
            (id, pe)
        })
        .collect()
}

/// Aggregiert über die finalen (gefilterten, sortierten, limitierten) Zeilen.
///
/// Semantik exakt nach `design-v0.8-query.md` §3: NULL/absent/non-numeric
/// werden übersprungen; `Sum` akkumuliert in `i128` und sättigt auf `i64`
/// (bzw. `Float64`, sobald ein `Float` auftritt); `Avg` liefert immer
/// `Float64`; `Min`/`Max` bleiben `Int64` bzw. werden bei Typmischung zu
/// `Float64` promoviert; nicht-endliche Floats werden übersprungen. Eine
/// leere/Null-wertige Menge liefert `None` — außer `Count`, das immer
/// `Some(Int)` liefert.
pub fn aggregate_rows(rows: &[(String, Entity)], agg: &Aggregate) -> Result<Option<Value>> {
    match agg {
        Aggregate::Count => Ok(Some(Value::Int(rows.len() as i64))),
        Aggregate::Sum(field)
        | Aggregate::Avg(field)
        | Aggregate::Min(field)
        | Aggregate::Max(field) => {
            let is_min = matches!(agg, Aggregate::Min(_));
            let mut has_float = false;
            let mut i_sum: i128 = 0;
            let mut f_sum: f64 = 0.0;
            let mut count: u64 = 0;
            let mut i_extreme: Option<i64> = None;
            let mut f_extreme: Option<f64> = None;
            for (_, e) in rows {
                let Some(v) = e.field(field) else {
                    continue;
                };
                match v {
                    Value::Null => continue,
                    Value::Int(i) => {
                        i_sum = i_sum.saturating_add(*i as i128);
                        f_sum += *i as f64;
                        count += 1;
                        i_extreme = Some(match i_extreme {
                            None => *i,
                            Some(c) => {
                                if is_min {
                                    c.min(*i)
                                } else {
                                    c.max(*i)
                                }
                            }
                        });
                        let f = *i as f64;
                        f_extreme = Some(match f_extreme {
                            None => f,
                            Some(c) => {
                                if is_min {
                                    c.min(f)
                                } else {
                                    c.max(f)
                                }
                            }
                        });
                    }
                    Value::Float(x) => {
                        if !x.is_finite() {
                            continue; // NaN/Inf überspringen
                        }
                        has_float = true;
                        f_sum += *x;
                        count += 1;
                        let f = *x;
                        f_extreme = Some(match f_extreme {
                            None => f,
                            Some(c) => {
                                if is_min {
                                    c.min(f)
                                } else {
                                    c.max(f)
                                }
                            }
                        });
                    }
                    // String / Bytes / Bool: nicht-numerisch → überspringen.
                    _ => continue,
                }
            }
            if count == 0 {
                return Ok(None);
            }
            match agg {
                Aggregate::Sum(_) => {
                    if has_float {
                        Ok(Some(Value::Float(f_sum)))
                    } else {
                        let s = if i_sum > i64::MAX as i128 {
                            i64::MAX
                        } else if i_sum < i64::MIN as i128 {
                            i64::MIN
                        } else {
                            i_sum as i64
                        };
                        Ok(Some(Value::Int(s)))
                    }
                }
                Aggregate::Avg(_) => Ok(Some(Value::Float(f_sum / count as f64))),
                Aggregate::Min(_) | Aggregate::Max(_) => {
                    if has_float {
                        Ok(f_extreme.map(Value::Float))
                    } else {
                        Ok(i_extreme.map(Value::Int))
                    }
                }
                Aggregate::Count => unreachable!(),
            }
        }
    }
}
