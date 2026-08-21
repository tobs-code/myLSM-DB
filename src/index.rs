//! Secondary Indexes über der KV-Engine.
//!
//! ## Architektur-Invariante (NICHT versehentlich kaputtmachen!)
//!
//! 1. **Die Entity ist immer Source of Truth.** Ein Index wird niemals zur
//!    Rekonstruktion eines Entity-Zustands verwendet. Er liefert nur
//!    Kandidaten.
//! 2. **Ein Index darf False Positives enthalten, aber NIEMALS False
//!    Negatives.** Ein veralteter Index darf höchstens dazu führen, dass
//!    `find()` eine Entity nach der Verifikation herausfiltert — aber nie,
//!    dass eine passende Entity übersehen wird.
//! 3. Daraus folgt: Während einer Änderung ist der Index **temporär ein
//!    Superset** der korrekten Einträge, nie ein Subset. `find()` verifiziert
//!    deshalb jede Kandidaten-Entity gegen ihren echten Wert.
//!
//! Write-Reihenfolge (siehe `entity.rs`):
//! ```text
//! PUT neuer Index-Eintrag  →  PUT Entity  →  DELETE alter Index-Eintrag
//! ```

use std::collections::HashSet;

use crate::codec::{self, Value};
use crate::error::{Error, Result};
use crate::keycodec;
use crate::ordering;
use crate::schema::{IndexStatus, Schema};
use crate::{Database, DirectMutator, Mutator};

/// Unter-/Obergrenze eines Index-Bereichs.
#[derive(Debug, Clone)]
pub enum Bound {
    Unbounded,
    Inclusive(Value),
    Exclusive(Value),
}

/// Abfrage-Operation auf einem (geordneten) Index.
#[derive(Debug, Clone)]
pub enum FindOp {
    Eq(Value),
    Gt(Value),
    Gte(Value),
    Lt(Value),
    Lte(Value),
    Between(Value, Value),
}

impl FindOp {
    pub(crate) fn to_bounds(&self) -> (Bound, Bound) {
        use Bound::*;
        match self {
            FindOp::Eq(v) => (Inclusive(v.clone()), Inclusive(v.clone())),
            FindOp::Gt(v) => (Exclusive(v.clone()), Unbounded),
            FindOp::Gte(v) => (Inclusive(v.clone()), Unbounded),
            FindOp::Lt(v) => (Unbounded, Exclusive(v.clone())),
            FindOp::Lte(v) => (Unbounded, Inclusive(v.clone())),
            FindOp::Between(l, h) => (Inclusive(l.clone()), Inclusive(h.clone())),
        }
    }
}

/// Der Scan-Bereich im Index-Key-Raum für einen Bereich.
pub(crate) fn index_range(
    collection_id: u32,
    field_id: u32,
    lower: &Bound,
    upper: &Bound,
) -> (Vec<u8>, Option<Vec<u8>>) {
    let fp = |f: u32| keycodec::index_field_prefix(collection_id, f);
    let vp = |f: u32, v: &Value| {
        keycodec::index_value_prefix(collection_id, f, &ordering::encode_ordered(v))
    };
    let start = match lower {
        Bound::Unbounded => fp(field_id),
        Bound::Inclusive(v) => vp(field_id, v),
        Bound::Exclusive(v) => {
            keycodec::successor(&vp(field_id, v)).unwrap_or_else(|| fp(field_id))
        }
    };
    let end = match upper {
        Bound::Unbounded => keycodec::successor(&fp(field_id)),
        Bound::Exclusive(v) => Some(vp(field_id, v)),
        Bound::Inclusive(v) => keycodec::successor(&vp(field_id, v)),
    };
    (start, end)
}

/// Liefert den tatsächlichen Wert eines Feldes einer Entity (oder `None`,
/// wenn das Feld fehlt/gelöscht ist). Verifikationspfad.
fn field_value_m<M: Mutator>(
    m: &mut M,
    collection_id: u32,
    entity_id: &[u8],
    field_id: u32,
) -> Result<Option<Value>> {
    let key = keycodec::encode_entity_key(collection_id, entity_id, field_id);
    match m.get(&key)? {
        Some(bytes) => codec::decode(&bytes).map(Some),
        None => Ok(None),
    }
}

/// Prüft, ob ein Wert in den Bereich fällt.
pub(crate) fn within(value: &Value, lower: &Bound, upper: &Bound) -> bool {
    use std::cmp::Ordering;
    let lo = match lower {
        Bound::Unbounded => true,
        Bound::Inclusive(v) => ordering::value_cmp(value, v) != Ordering::Less,
        Bound::Exclusive(v) => ordering::value_cmp(value, v) == Ordering::Greater,
    };
    let hi = match upper {
        Bound::Unbounded => true,
        Bound::Exclusive(v) => ordering::value_cmp(value, v) == Ordering::Less,
        Bound::Inclusive(v) => ordering::value_cmp(value, v) != Ordering::Greater,
    };
    lo && hi
}

/// Dekodiert einen Index-Key in `(Wert, Entity-ID)`. `Ok(None)`, wenn der Key
/// kein Index-Key ist; Fehler bei unlesbarem Wert oder nicht-UTF-8-ID.
pub(crate) fn decode_index_key_value(key: &[u8]) -> Result<Option<(Value, String)>> {
    let Some((_c, _f, entity)) = keycodec::decode_index_key(key) else {
        return Ok(None);
    };
    // Layout: [I][cid u32][fid u32][enc_value][len u32][entity_id]
    let value_len = ordering::ordered_value_len(&key[9..])?;
    let value = ordering::decode_ordered(&key[9..9 + value_len])?;
    let eid = crate::keycodec::decode_entity_id(entity)?.to_string();
    Ok(Some((value, eid)))
}

/// Führt eine Index-Abfrage über eine beliebige Mutator-Sicht aus. Liefert die
/// **verifizierten** Entity-IDs. Ein fehlender Index auf dem Feld ist ein Fehler.
///
/// Der Mutator-Pfad erlaubt es, innerhalb einer Transaktion über das
/// Pending-Overlay zu suchen (Read-your-own-writes).
pub(crate) fn find_m<M: Mutator>(
    m: &mut M,
    schema: &Schema,
    collection_id: u32,
    field_id: u32,
    lower: &Bound,
    upper: &Bound,
) -> Result<Vec<String>> {
    if schema.find_index(collection_id, field_id).is_none() {
        return Err(Error::InvalidArgument("no index on field".into()));
    }
    let (start, end) = index_range(collection_id, field_id, lower, upper);
    let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = m
        .scan(Some(&start), end.as_deref())?
        .collect::<std::result::Result<_, _>>()?;
    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (key, _) in rows {
        let (_c, _f, entity) = keycodec::decode_index_key(&key)
            .ok_or_else(|| Error::InvalidFormat("bad index key".into()))?;
        let eid = crate::keycodec::decode_entity_id(entity)?.to_string();
        if !seen.insert(eid.clone()) {
            continue;
        }
        // Verifikation gegen die Entity (Index ist nie die Wahrheit).
        if let Some(actual) = field_value_m(m, collection_id, entity, field_id)? {
            if within(&actual, lower, upper) {
                ids.push(eid);
            }
        }
    }
    Ok(ids)
}

/// Führt eine Index-Abfrage auf der committeten Engine aus.
pub fn find(
    db: &mut Database,
    schema: &Schema,
    collection_id: u32,
    field_id: u32,
    lower: &Bound,
    upper: &Bound,
) -> Result<Vec<String>> {
    let mut m = DirectMutator { db };
    find_m(&mut m, schema, collection_id, field_id, lower, upper)
}

/// Scan-Bereich im Composite-Index-Key-Raum.
///
/// `leading` sind die gebundenen Komponenten in aufsteigender Positionsreihenfolge
/// (Index in `field_ids`): Komponente `0..L-1` müssen Equality sein; Komponente
/// `L-1` darf ein beliebiger Bereich sein. Die Berechnung hängt die encodierten
/// Werte (bzw. deren Successor) in der korrekten Reihenfolge an das
/// Index-Präfix — dank selbst-delimitierender `encode_ordered`-Werte bleibt das
/// präfix-sortierbar.
fn composite_index_range(
    collection_id: u32,
    index_id: u32,
    leading: &[(usize, Bound, Bound)],
) -> (Vec<u8>, Option<Vec<u8>>) {
    use Bound::*;
    let prefix = keycodec::composite_prefix(collection_id, index_id);
    let n = leading.len();
    // Equality-Präfix aus allen bis auf die letzte Komponente.
    let mut eq_prefix = prefix.clone();
    for i in 0..n.saturating_sub(1) {
        let (_, lo, _hi) = &leading[i];
        match lo {
            Inclusive(v) => eq_prefix.extend_from_slice(&ordering::encode_ordered(v)),
            // Ungültig (nicht-Equality in Zwischenposition): ganzer Präfix-Scan.
            _ => return (prefix.clone(), keycodec::successor(&prefix)),
        }
    }
    let mut start = eq_prefix.clone();
    let end = if n > 0 {
        let (_, lo, hi) = &leading[n - 1];
        match lo {
            Unbounded => {}
            Inclusive(v) => start.extend_from_slice(&ordering::encode_ordered(v)),
            Exclusive(v) => {
                if let Some(s) = keycodec::successor(&ordering::encode_ordered(v)) {
                    start.extend_from_slice(&s);
                }
            }
        }
        match hi {
            Unbounded => keycodec::successor(&eq_prefix).unwrap_or_else(|| eq_prefix.clone()),
            Inclusive(v) => {
                let suffix = keycodec::successor(&ordering::encode_ordered(v)).unwrap_or_default();
                let mut e = eq_prefix.clone();
                e.extend_from_slice(&suffix);
                e
            }
            Exclusive(v) => {
                let mut e = eq_prefix.clone();
                e.extend_from_slice(&ordering::encode_ordered(v));
                e
            }
        }
    } else {
        keycodec::successor(&prefix).unwrap_or_else(|| prefix.clone())
    };
    (start, Some(end))
}

/// Dekodiert einen Composite-Index-Key in `(Komponenten-Werte, Entity-ID)`.
/// `Ok(None)`, wenn der Key kein Composite-Index-Key ist; Fehler bei
/// unlesbarem Wert oder nicht-UTF-8-ID.
pub(crate) fn decode_composite_key_value(
    key: &[u8],
    n_components: usize,
) -> Result<Option<(Vec<Value>, String)>> {
    if key.len() < 1 + 4 + 4 + 4 + 4 || key[0] != keycodec::INDEX_TAG {
        return Ok(None);
    }
    let mut off = 1;
    let _cid = u32::from_le_bytes(key[off..off + 4].try_into().unwrap());
    off += 4;
    let marker = u32::from_le_bytes(key[off..off + 4].try_into().unwrap());
    off += 4;
    if marker != keycodec::COMPOSITE_FIELD_MARKER {
        return Ok(None);
    }
    let _index_id = u32::from_le_bytes(key[off..off + 4].try_into().unwrap());
    off += 4;
    let mut comps = Vec::with_capacity(n_components);
    for _ in 0..n_components {
        let len = ordering::ordered_value_len(&key[off..])?;
        let v = ordering::decode_ordered(&key[off..off + len])?;
        comps.push(v);
        off += len;
    }
    if off + 4 > key.len() {
        return Ok(None);
    }
    let eid_len = u32::from_le_bytes(key[off..off + 4].try_into().unwrap()) as usize;
    off += 4;
    if off + eid_len > key.len() {
        return Ok(None);
    }
    let eid = crate::keycodec::decode_entity_id(&key[off..off + eid_len])?.to_string();
    Ok(Some((comps, eid)))
}

/// Führt eine Composite-Index-Abfrage über eine beliebige Mutator-Sicht aus.
/// Liefert die **verifizierten** Entity-IDs (gegen die echten Komponenten-
/// Werte der Entity — der Index ist nie die Wahrheit).
pub(crate) fn find_composite_m<M: Mutator>(
    m: &mut M,
    schema: &Schema,
    collection_id: u32,
    index_id: u32,
    n_components: usize,
    leading: &[(usize, Bound, Bound)],
) -> Result<Vec<String>> {
    let Some(def) = schema.index_by_id(index_id) else {
        return Err(Error::InvalidArgument("composite index not found".into()));
    };
    if def.status != IndexStatus::Ready {
        return Err(Error::InvalidArgument("composite index not ready".into()));
    }
    let fids = def.field_ids.clone();
    let (start, end) = composite_index_range(collection_id, index_id, leading);
    let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = m
        .scan(Some(&start), end.as_deref())?
        .collect::<std::result::Result<_, _>>()?;
    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (key, _) in rows {
        let Some((_comps, eid)) = decode_composite_key_value(&key, n_components)? else {
            continue;
        };
        // Verifikation gegen die echten Entity-Werte (Index ist nie die
        // Wahrheit): ein veralteter Composite-Eintrag darf keine False
        // Positive erzeugen. Ein fehlendes Feld (`absent`) erfüllt keinen
        // konkreten Bereich → Kandidat fällt weg.
        let mut ok = true;
        for (idx, lo, hi) in leading {
            let actual = field_value_m(m, collection_id, eid.as_bytes(), fids[*idx])?;
            match actual {
                None => {
                    ok = false;
                    break;
                }
                Some(v) => {
                    if !within(&v, lo, hi) {
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok {
            continue;
        }
        if !seen.insert(eid.clone()) {
            continue;
        }
        ids.push(eid);
    }
    Ok(ids)
}

/// Löscht alle Composite-Index-Keys eines (collection, index).
pub fn clear_composite(db: &mut Database, collection_id: u32, index_id: u32) -> Result<()> {
    let start = keycodec::composite_prefix(collection_id, index_id);
    let end = keycodec::successor(&start).ok_or_else(|| {
        Error::InvalidFormat("composite prefix has no successor".into())
    })?;
    let rows = db.scan(Some(&start), Some(&end))?;
    for (key, _) in rows {
        db.delete(&key)?;
    }
    Ok(())
}

/// Baut einen Composite-Index (vollständig) neu auf: erst alle alten
/// Composite-Index-Keys löschen, dann pro Entity das Tupel der `field_ids`
/// rekonstruieren und — falls **alle** Komponenten vorhanden sind — den
/// Composite-Key schreiben. Idempotent (ideal für Recovery nach Crash).
pub fn rebuild_composite(
    db: &mut Database,
    collection_id: u32,
    index_id: u32,
    field_ids: &[u32],
) -> Result<()> {
    clear_composite(db, collection_id, index_id)?;
    let pstart = keycodec::collection_prefix(collection_id);
    let pend = keycodec::successor(&pstart).ok_or_else(|| {
        Error::InvalidFormat("collection prefix has no successor".into())
    })?;
    let rows = db.scan(Some(&pstart), Some(&pend))?;
    // Feld-Keys pro Entity sammeln.
    let mut entity_fields: std::collections::HashMap<Vec<u8>, std::collections::HashMap<u32, Value>> =
        std::collections::HashMap::new();
    for (key, value_opt) in rows {
        let Some((_, ee, ef)) = keycodec::decode_entity_key(&key) else {
            continue;
        };
        if let Some(bytes) = value_opt {
            if let Ok(val) = codec::decode(&bytes) {
                entity_fields
                    .entry(ee.to_vec())
                    .or_default()
                    .insert(ef, val);
            }
        }
    }
    for (eid, fields) in entity_fields {
        let mut comps: Vec<Vec<u8>> = Vec::with_capacity(field_ids.len());
        let mut complete = true;
        for &fid in field_ids {
            match fields.get(&fid) {
                Some(v) => comps.push(ordering::encode_ordered(v)),
                None => {
                    complete = false;
                    break;
                }
            }
        }
        if !complete {
            continue; // fehlende Komponente → keine Composite-Index-Zeile
        }
        let ik = keycodec::encode_composite_index_key(collection_id, index_id, &comps, &eid);
        db.put(&ik, &[])?;
    }
    Ok(())
}

/// Löscht alle Index-Keys eines (collection, field).
pub fn clear(db: &mut Database, collection_id: u32, field_id: u32) -> Result<()> {
    let (start, end) = index_range(
        collection_id,
        field_id,
        &Bound::Unbounded,
        &Bound::Unbounded,
    );
    let rows = db.scan(Some(&start), end.as_deref())?;
    for (key, _) in rows {
        db.delete(&key)?;
    }
    Ok(())
}

/// Baut einen Index (vollständig) neu auf: erst alle alten Index-Keys löschen,
/// dann alle Entities der Collection scannen und den Index rekonstruieren.
/// Idempotent — ideal für Recovery nach einem Crash während `BUILDING`.
pub fn rebuild(db: &mut Database, collection_id: u32, field_id: u32) -> Result<()> {
    clear(db, collection_id, field_id)?;
    let pstart = keycodec::collection_prefix(collection_id);
    let pend = keycodec::successor(&pstart);
    let rows = db.scan(Some(&pstart), pend.as_deref())?;
    for (key, value_opt) in rows {
        let Some((ec, ee, ef)) = keycodec::decode_entity_key(&key) else {
            continue;
        };
        if ec != collection_id || ef != field_id {
            continue;
        }
        let Some(bytes) = value_opt else {
            continue; // Tombstone.
        };
        let value = codec::decode(&bytes)?;
        let enc = ordering::encode_ordered(&value);
        let ik = keycodec::encode_index_key(collection_id, field_id, &enc, ee);
        db.put(&ik, &[])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{Entity, EntityStore};

    fn u(id: &str, age: i64) -> Entity {
        let mut e = Entity::new();
        e.insert("age", Value::Int(age));
        e.insert("name", Value::String(format!("user-{id}")));
        e
    }

    fn ids(v: Vec<String>) -> Vec<String> {
        let mut v = v;
        v.sort();
        v
    }

    #[test]
    fn find_eq_and_range() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        col.put("u1", &u("u1", 30)).unwrap();
        col.put("u2", &u("u2", 31)).unwrap();
        col.put("u3", &u("u3", 31)).unwrap();
        col.put("u4", &u("u4", 40)).unwrap();

        assert_eq!(
            ids(col.find("age", FindOp::Eq(Value::Int(31))).unwrap()),
            vec!["u2", "u3"]
        );
        assert_eq!(
            ids(col.find("age", FindOp::Gte(Value::Int(31))).unwrap()),
            vec!["u2", "u3", "u4"]
        );
        assert_eq!(
            ids(col.find("age", FindOp::Gt(Value::Int(31))).unwrap()),
            vec!["u4"]
        );
        assert_eq!(
            ids(col.find("age", FindOp::Lt(Value::Int(31))).unwrap()),
            vec!["u1"]
        );
        assert_eq!(
            ids(col.find("age", FindOp::Lte(Value::Int(31))).unwrap()),
            vec!["u1", "u2", "u3"]
        );
        assert_eq!(
            ids(col
                .find("age", FindOp::Between(Value::Int(30), Value::Int(31)))
                .unwrap()),
            vec!["u1", "u2", "u3"]
        );
    }

    #[test]
    fn maintenance_on_update_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();
        col.put("u1", &u("u1", 30)).unwrap();
        assert_eq!(
            ids(col.find("age", FindOp::Eq(Value::Int(30))).unwrap()),
            vec!["u1"]
        );

        // Update 30 → 31: altes Index-Key weg, neues da.
        col.put("u1", &u("u1", 31)).unwrap();
        assert!(
            col.find("age", FindOp::Eq(Value::Int(30)))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(col.find("age", FindOp::Eq(Value::Int(31))).unwrap()),
            vec!["u1"]
        );

        // Delete: keine Treffer mehr.
        col.delete("u1").unwrap();
        assert!(
            col.find("age", FindOp::Eq(Value::Int(31)))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn find_requires_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let mut col = store.collection("users").unwrap();
        col.put("u1", &u("u1", 30)).unwrap();
        assert!(col.find("age", FindOp::Eq(Value::Int(30))).is_err());
    }

    #[test]
    fn index_ready_after_create_and_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let mut col = store.collection("users").unwrap();
        col.put("u1", &u("u1", 30)).unwrap();
        col.put("u2", &u("u2", 30)).unwrap();
        col.create_index("age").unwrap();
        assert_eq!(
            ids(col.find("age", FindOp::Eq(Value::Int(30))).unwrap()),
            vec!["u1", "u2"]
        );
    }
}
