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
use crate::schema::Schema;
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
