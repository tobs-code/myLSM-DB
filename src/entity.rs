//! Entity-Layer über der KV-Engine.
//!
//! Baut getypte Entitäten auf der "dummen" v0.1-KV-Maschine auf. Ein `Entity`
//! wird in mehrere Feld-Keys `E | collection_id | entity_id | field_id`
//! zerlegt (siehe [`keycodec`]) und beim Lesen wieder zu einem `Entity`
//! rekonstruiert. Die Collection-/Field-ID-Zuordnung ist persistent
//! (siehe [`schema`]).
//!
//! Die KV-Engine (`Database`) kennt diese Schicht NICHT — die Abhängigkeit
//! geht nur in eine Richtung.

use std::ops::Index;
use std::path::PathBuf;

use crate::codec::{self, Value};
use crate::error::{Error, Result};
use crate::keycodec;
use crate::schema::Schema;
use crate::Database;

/// Eine rekonstruierte Entität: eine (geordnete) Liste benannter getypter Werte.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entity {
    pub fields: Vec<(String, Value)>,
}

impl Entity {
    pub fn new() -> Entity {
        Entity { fields: Vec::new() }
    }

    pub fn insert(&mut self, name: impl Into<String>, value: Value) {
        self.fields.push((name.into(), value));
    }

    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

impl Index<&str> for Entity {
    type Output = Value;
    fn index(&self, name: &str) -> &Value {
        self.field(name).expect("field not found")
    }
}

impl Index<String> for Entity {
    type Output = Value;
    fn index(&self, name: String) -> &Value {
        &self[name.as_str()]
    }
}

/// Entitäts-Store: hält die KV-Engine + das persistente Schema.
pub struct EntityStore {
    db: Database,
    schema: Schema,
    schema_path: PathBuf,
}

/// Ein Handle auf eine einzelne Collection, das `put`/`get`/`delete` auf
/// Entitäten ermöglicht. Bricht die Namen zu stabilen IDs herunter.
pub struct CollectionHandle<'a> {
    store: &'a mut EntityStore,
    collection_id: u32,
}

impl EntityStore {
    /// Öffnet (oder erstellt) einen Entitäts-Store in `dir`. Legt darunter eine
    /// v0.1-KV-Engine an und lädt das persistente Schema.
    pub fn open(dir: impl AsRef<std::path::Path>) -> Result<EntityStore> {
        let dir = dir.as_ref();
        let db = Database::open(dir)?;
        let schema_path = dir.join("SCHEMA");
        let schema = Schema::load(&schema_path)?;
        Ok(EntityStore { db, schema, schema_path })
    }

    /// Gibt ein Handle auf eine Collection. Existiert die Collection noch
    /// nicht, wird sie (und ihre stabile ID) neu angelegt.
    pub fn collection<'a>(&'a mut self, name: &str) -> Result<CollectionHandle<'a>> {
        let collection_id = self.schema.collection_id(name);
        self.persist_schema()?;
        Ok(CollectionHandle { store: self, collection_id })
    }

    pub fn close(mut self) -> Result<()> {
        self.persist_schema()?;
        self.db.close()
    }

    /// Schreibt ein neues Schema, falls sich die Registry seit dem letzten
    /// `save` geändert hat.
    fn persist_schema(&mut self) -> Result<()> {
        if self.schema.is_changed() {
            self.schema.save(&self.schema_path)?;
        }
        Ok(())
    }

    /// Legt eine Entität an bzw. ersetzt sie. Nicht mehr vorhandene Felder der
    /// bisherigen Entität werden entfernt, sodass der gespeicherte Zustand exakt
    /// dem übergebenen `Entity` entspricht.
    fn put_entity(&mut self, collection_id: u32, entity_id: &[u8], entity: &Entity) -> Result<()> {
        // Zuerst alle Feld-IDs vergeben und das (geänderte) Schema persistieren,
        // BEVOR dauerhafte Entitätsdaten geschrieben werden — sonst könnte nach
        // einem Crash eine Feld-ID eine andere Bedeutung haben.
        let mut written: Vec<(u32, &Value)> = Vec::with_capacity(entity.fields.len());
        for (name, value) in &entity.fields {
            let field_id = self.schema.field_id(collection_id, name);
            written.push((field_id, value));
        }
        self.persist_schema()?;

        // Bisherige Felder der Entität ermitteln, um veraltete zu entfernen.
        let (start, end) = keycodec::entity_range(collection_id, entity_id);
        let existing = self.db.scan(Some(&start), end.as_deref())?;
        let new_field_ids: std::collections::HashSet<u32> =
            written.iter().map(|(f, _)| *f).collect();
        for (key, _) in &existing {
            if let Some((_, _, field_id)) = keycodec::decode_entity_key(key) {
                if !new_field_ids.contains(&field_id) {
                    self.db.delete(key)?;
                }
            }
        }

        // Neue Feldwerte schreiben.
        for (field_id, value) in written {
            let key = keycodec::encode_entity_key(collection_id, entity_id, field_id);
            let enc = codec::encode(value);
            self.db.put(&key, &enc)?;
        }
        Ok(())
    }

    /// Liest eine Entität vollständig aus ihren Feld-Keys und rekonstruiert sie.
    fn get_entity(&mut self, collection_id: u32, entity_id: &[u8]) -> Result<Option<Entity>> {
        let (start, end) = keycodec::entity_range(collection_id, entity_id);
        let rows = self.db.scan(Some(&start), end.as_deref())?;
        if rows.is_empty() {
            return Ok(None);
        }
        let mut entity = Entity::new();
        for (key, value_opt) in rows {
            let (_, _, field_id) = keycodec::decode_entity_key(&key)
                .ok_or_else(|| Error::InvalidFormat("bad entity key".into()))?;
            let value = match value_opt {
                Some(v) => codec::decode(&v)?,
                None => continue, // Tombstone, kein Feld.
            };
            let name = self
                .schema
                .field_name(collection_id, field_id)
                .ok_or_else(|| Error::InvalidFormat(format!("unknown field id {field_id}")))?;
            entity.fields.push((name.to_string(), value));
        }
        // Nur eine Entität liefern, wenn mindestens ein lebendes Feld existiert.
        // (Eine komplett gelöschte Entität hat nur Tombstones → None.)
        if entity.fields.is_empty() {
            return Ok(None);
        }
        Ok(Some(entity))
    }

    /// Löscht alle Feld-Keys einer Entität.
    fn delete_entity(&mut self, collection_id: u32, entity_id: &[u8]) -> Result<()> {
        let (start, end) = keycodec::entity_range(collection_id, entity_id);
        let rows = self.db.scan(Some(&start), end.as_deref())?;
        for (key, _) in rows {
            self.db.delete(&key)?;
        }
        Ok(())
    }
}

impl<'a> CollectionHandle<'a> {
    pub fn put(&mut self, entity_id: &str, entity: &Entity) -> Result<()> {
        self.store
            .put_entity(self.collection_id, entity_id.as_bytes(), entity)
    }

    pub fn get(&mut self, entity_id: &str) -> Result<Option<Entity>> {
        self.store
            .get_entity(self.collection_id, entity_id.as_bytes())
    }

    pub fn delete(&mut self, entity_id: &str) -> Result<()> {
        self.store
            .delete_entity(self.collection_id, entity_id.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Value;

    fn user(name: &str, age: i64, active: bool) -> Entity {
        let mut e = Entity::new();
        e.insert("name", Value::String(name.into()));
        e.insert("age", Value::Int(age));
        e.insert("active", Value::Bool(active));
        e
    }

    #[test]
    fn put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let entity = user("Tobias", 31, true);
        store.collection("users").unwrap().put("usr_123", &entity).unwrap();

        let got = store.collection("users").unwrap().get("usr_123").unwrap().expect("exists");
        assert_eq!(got.field("name"), Some(&Value::String("Tobias".into())));
        assert_eq!(got.field("age"), Some(&Value::Int(31)));
        assert_eq!(got.field("active"), Some(&Value::Bool(true)));
        // Zugriff per Index
        assert_eq!(got["age"], Value::Int(31));
        assert_eq!(got["name"], Value::String("Tobias".into()));
    }

    #[test]
    fn put_replaces_and_removes_stale_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let mut e = user("Tobias", 31, true);
        store.collection("users").unwrap().put("u1", &e).unwrap();

        // Feld "active" entfernen, "age" ändern
        e.fields.retain(|(n, _)| n != "active");
        e.fields.iter_mut().find(|(n, _)| n == "age").unwrap().1 = Value::Int(32);
        store.collection("users").unwrap().put("u1", &e).unwrap();

        let got = store.collection("users").unwrap().get("u1").unwrap().expect("exists");
        assert_eq!(got.field("age"), Some(&Value::Int(32)));
        assert!(got.field("active").is_none(), "stale field must be removed");
        assert_eq!(got.field("name"), Some(&Value::String("Tobias".into())));
    }

    #[test]
    fn delete_removes_entity() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let e = user("Tobias", 31, true);
        store.collection("users").unwrap().put("u1", &e).unwrap();
        assert!(store.collection("users").unwrap().get("u1").unwrap().is_some());
        store.collection("users").unwrap().delete("u1").unwrap();
        assert!(store.collection("users").unwrap().get("u1").unwrap().is_none());
    }

    #[test]
    fn schema_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = EntityStore::open(dir.path()).unwrap();
            let e = user("Tobias", 31, true);
            store.collection("users").unwrap().put("u1", &e).unwrap();
            store.close().unwrap();
        }
        // Neu öffnen: IDs müssen identisch bleiben, sonst wäre das Feld unlesbar.
        let mut store = EntityStore::open(dir.path()).unwrap();
        let got = store.collection("users").unwrap().get("u1").unwrap().expect("exists");
        assert_eq!(got.field("name"), Some(&Value::String("Tobias".into())));
        assert_eq!(got.field("age"), Some(&Value::Int(31)));
    }

    #[test]
    fn unicode_and_empty_values_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let mut e = Entity::new();
        e.insert("text", Value::String("hällo ünïcode 🚀".into()));
        e.insert("empty", Value::String(String::new()));
        e.insert("bytes", Value::Bytes(vec![]));
        e.insert("nil", Value::Null);
        e.insert("neg", Value::Int(-1234567890123));
        e.insert("big", Value::Int(i64::MAX));
        store.collection("doc").unwrap().put("d1", &e).unwrap();
        let got = store.collection("doc").unwrap().get("d1").unwrap().unwrap();
        assert_eq!(got, e);
    }
}