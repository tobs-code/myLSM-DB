//! Persistente Field-/Collection-Registry.
//!
//! Vergibt `collection_id` und `field_id` dauerhaft und lädt sie bei jedem
//! Start wieder. Das ist wichtig, damit die Bedeutung eines gespeicherten Keys
//! zwischen zwei Datenbankstarts identisch bleibt — also NICHT `Hash("name")`
//! und auch kein Neuvergeben pro Start.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};

/// Status eines Index-Aufbaus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    /// Index wird gerade aufgebaut; nach einem Crash wird er neu gebaut.
    Building,
    /// Index ist vollständig und nutzbar.
    Ready,
}

impl IndexStatus {
    fn code(&self) -> &'static str {
        match self {
            IndexStatus::Building => "BUILDING",
            IndexStatus::Ready => "READY",
        }
    }
    fn from_code(s: &str) -> Result<IndexStatus> {
        match s {
            "BUILDING" => Ok(IndexStatus::Building),
            "READY" => Ok(IndexStatus::Ready),
            _ => Err(Error::InvalidFormat("unknown index status".into())),
        }
    }
}

/// Definition eines Secondary Index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexDef {
    pub id: u32,
    pub collection_id: u32,
    pub field_id: u32,
    pub status: IndexStatus,
}

/// Persistente Zuordnung von Collection-/Field-Namen zu stabilen IDs.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    next_collection_id: u32,
    next_field_id: u32,
    /// collection_id -> Name.
    collection_name: HashMap<u32, String>,
    /// Name -> collection_id.
    collection_id_of: HashMap<String, u32>,
    /// (collection_id, field_id) -> Feldname.
    field_name: HashMap<(u32, u32), String>,
    /// (collection_id, Feldname) -> field_id.
    field_id_of: HashMap<(u32, String), u32>,
    /// `true`, wenn neue IDs vergeben wurden und noch nicht persistiert sind.
    changed: bool,
    next_index_id: u32,
    indexes: Vec<IndexDef>,
}

impl Schema {
    pub fn new() -> Schema {
        Schema::default()
    }

    /// Liefert die ID einer Collection. Existiert sie noch nicht, wird sie
    /// neu angelegt.
    pub fn collection_id(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.collection_id_of.get(name) {
            return id;
        }
        let id = self.next_collection_id;
        self.next_collection_id += 1;
        self.changed = true;
        self.collection_name.insert(id, name.to_string());
        self.collection_id_of.insert(name.to_string(), id);
        id
    }

    /// Liefert die ID eines bereits bekannten Feldes (ohne es anzulegen).
    pub fn lookup_field_id(&self, collection_id: u32, name: &str) -> Option<u32> {
        self.field_id_of
            .get(&(collection_id, name.to_string()))
            .copied()
    }

    /// Liefert die ID eines Feldes einer Collection. Existiert es noch nicht,
    /// wird es neu angelegt.
    pub fn field_id(&mut self, collection_id: u32, name: &str) -> u32 {
        if let Some(&id) = self.field_id_of.get(&(collection_id, name.to_string())) {
            return id;
        }
        let id = self.next_field_id;
        self.next_field_id += 1;
        self.changed = true;
        self.field_name
            .insert((collection_id, id), name.to_string());
        self.field_id_of
            .insert((collection_id, name.to_string()), id);
        id
    }

    /// Name einer Collection (für die Rekonstruktion).
    pub fn collection_name(&self, id: u32) -> Option<&str> {
        self.collection_name.get(&id).map(String::as_str)
    }

    /// Name eines Feldes (für die Rekonstruktion).
    pub fn field_name(&self, collection_id: u32, field_id: u32) -> Option<&str> {
        self.field_name
            .get(&(collection_id, field_id))
            .map(String::as_str)
    }

    /// `true`, wenn seit dem letzten `save` neue IDs vergeben wurden.
    pub fn is_changed(&self) -> bool {
        self.changed
    }

    /// Registriert einen neuen Index (Status BUILDING) für (collection, field).
    pub fn create_index(&mut self, collection_id: u32, field_id: u32) -> u32 {
        if let Some(i) = self
            .indexes
            .iter()
            .find(|i| i.collection_id == collection_id && i.field_id == field_id)
        {
            return i.id;
        }
        let id = self.next_index_id;
        self.next_index_id += 1;
        self.changed = true;
        self.indexes.push(IndexDef {
            id,
            collection_id,
            field_id,
            status: IndexStatus::Building,
        });
        id
    }

    /// Setzt einen Index auf READY.
    pub fn set_index_ready(&mut self, index_id: u32) {
        if let Some(i) = self.indexes.iter_mut().find(|i| i.id == index_id) {
            if i.status != IndexStatus::Ready {
                i.status = IndexStatus::Ready;
                self.changed = true;
            }
        }
    }

    /// Entfernt einen Index.
    pub fn drop_index(&mut self, index_id: u32) {
        let before = self.indexes.len();
        self.indexes.retain(|i| i.id != index_id);
        if self.indexes.len() != before {
            self.changed = true;
        }
    }

    /// Liefert den Index zu (collection, field), falls vorhanden.
    pub fn find_index(&self, collection_id: u32, field_id: u32) -> Option<&IndexDef> {
        self.indexes
            .iter()
            .find(|i| i.collection_id == collection_id && i.field_id == field_id)
    }

    pub fn index_by_id(&self, id: u32) -> Option<&IndexDef> {
        self.indexes.iter().find(|i| i.id == id)
    }

    /// Alle Index-Definitionen (für Rebuild/Recovery).
    pub fn indexes(&self) -> &[IndexDef] {
        &self.indexes
    }

    /// Indizes, die (noch) gebaut werden müssen.
    pub fn building_indexes(&self) -> impl Iterator<Item = &IndexDef> {
        self.indexes
            .iter()
            .filter(|i| i.status == IndexStatus::Building)
    }

    /// Persistiert das Schema atomar.
    pub fn save(&mut self, path: &Path) -> Result<()> {
        let mut buf = String::new();
        buf.push_str(&format!("NC {}\n", self.next_collection_id));
        buf.push_str(&format!("NF {}\n", self.next_field_id));
        let mut collections: Vec<(&u32, &String)> = self.collection_name.iter().collect();
        collections.sort_by_key(|(id, _)| **id);
        for (id, name) in collections {
            buf.push_str(&format!("C {} {}\n", id, escape(name)));
        }
        let mut fields: Vec<(&(u32, u32), &String)> = self.field_name.iter().collect();
        fields.sort_by_key(|((c, f), _)| (*c, *f));
        for ((c, f), name) in fields {
            buf.push_str(&format!("F {} {} {}\n", c, f, escape(name)));
        }
        buf.push_str(&format!("NI {}\n", self.next_index_id));
        let mut indexes = self.indexes.clone();
        indexes.sort_by_key(|i| i.id);
        for i in &indexes {
            buf.push_str(&format!(
                "IX {} {} {} {}\n",
                i.id,
                i.collection_id,
                i.field_id,
                i.status.code()
            ));
        }
        let tmp = path.with_extension("schema.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(buf.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        self.changed = false;
        Ok(())
    }

    /// Lädt das Schema von der Platte (oder liefert ein leeres, falls keine
    /// Datei existiert).
    pub fn load(path: &Path) -> Result<Schema> {
        if !path.exists() {
            return Ok(Schema::new());
        }
        let text = fs::read_to_string(path)?;
        let mut s = Schema::new();
        for line in text.lines() {
            // Names sind escaped (keine Literal-Leerzeichen), daher ist das
            // Aufteilen an Whitespace sicher und "F c f name" bleibt 4 Tokens.
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("NC") => {
                    s.next_collection_id = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema NC".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("schema NC id".into()))?;
                }
                Some("NF") => {
                    s.next_field_id = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema NF".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("schema NF id".into()))?;
                }
                Some("C") => {
                    let id: u32 = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema C id".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("schema C".into()))?;
                    let name = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema C name".into()))?;
                    let name = unescape(name);
                    s.collection_name.insert(id, name.clone());
                    s.collection_id_of.insert(name, id);
                }
                Some("F") => {
                    let c: u32 = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema F col".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("schema F".into()))?;
                    let f: u32 = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema F field".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("schema F field".into()))?;
                    let name = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema F name".into()))?;
                    let name = unescape(name);
                    s.field_name.insert((c, f), name.clone());
                    s.field_id_of.insert((c, name), f);
                }
                Some("NI") => {
                    s.next_index_id = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema NI".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("schema NI id".into()))?;
                }
                Some("IX") => {
                    let id: u32 = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema IX id".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("schema IX id".into()))?;
                    let c: u32 = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema IX col".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("schema IX col".into()))?;
                    let f: u32 = parts
                        .next()
                        .ok_or_else(|| Error::InvalidFormat("schema IX field".into()))?
                        .parse()
                        .map_err(|_| Error::InvalidFormat("schema IX field".into()))?;
                    let status = IndexStatus::from_code(
                        parts
                            .next()
                            .ok_or_else(|| Error::InvalidFormat("schema IX status".into()))?,
                    )?;
                    s.indexes.push(IndexDef {
                        id,
                        collection_id: c,
                        field_id: f,
                        status,
                    });
                }
                _ => {}
            }
        }
        Ok(s)
    }
}

/// Escaped einen Namen für die textuelle Persistenz (Leerzeichen via %20,
/// '%' via %25), damit Namen mit Leerzeichen nicht die Zeilenformatierung
/// brechen.
fn escape(s: &str) -> String {
    s.replace('%', "%25").replace(' ', "%20")
}

fn unescape(s: &str) -> String {
    s.replace("%20", " ").replace("%25", "%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_stable_ids() {
        let mut s = Schema::new();
        let users = s.collection_id("users");
        assert_eq!(s.collection_id("users"), users); // stabil
        let name = s.field_id(users, "name");
        assert_eq!(s.field_id(users, "name"), name); // stabil
        assert_ne!(s.field_id(users, "age"), name); // verschiedene Felder
        // Eine andere Collection bekommt andere IDs, aber der Name-Mapping bleibt.
        let posts = s.collection_id("posts");
        assert_ne!(posts, users);
        assert_eq!(s.collection_name(posts), Some("posts"));
        assert_eq!(s.field_name(users, name), Some("name"));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SCHEMA");
        let mut s = Schema::new();
        let users = s.collection_id("users");
        s.field_id(users, "name");
        s.field_id(users, "age");
        let posts = s.collection_id("my posts"); // Name mit Leerzeichen
        s.field_id(posts, "body");
        s.save(&path).unwrap();

        let mut loaded = Schema::load(&path).unwrap();
        assert_eq!(loaded.collection_id("users"), users);
        assert_eq!(loaded.collection_id("my posts"), posts);
        assert_eq!(loaded.field_id(users, "name"), s.field_id(users, "name"));
        assert_eq!(loaded.field_id(posts, "body"), s.field_id(posts, "body"));
        assert_eq!(loaded.collection_name(users), Some("users"));
        assert_eq!(loaded.collection_name(posts), Some("my posts"));
        assert_eq!(loaded.field_name(users, 0), Some("name"));
        assert_eq!(loaded.field_name(users, 1), Some("age"));
    }

    #[test]
    fn load_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope");
        let mut s = Schema::load(&path).unwrap();
        assert_eq!(s.collection_id("users"), 0); // startet bei 0
    }

    #[test]
    fn index_save_load_roundtrip_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SCHEMA");
        let mut s = Schema::new();
        let users = s.collection_id("users");
        let age = s.field_id(users, "age");
        let name = s.field_id(users, "name");
        let age_idx = s.create_index(users, age);
        s.set_index_ready(age_idx);
        let name_idx = s.create_index(users, name); // bleibt BUILDING
        s.save(&path).unwrap();

        let mut loaded = Schema::load(&path).unwrap();
        assert_eq!(
            loaded.index_by_id(age_idx).unwrap().status,
            IndexStatus::Ready
        );
        assert_eq!(
            loaded.index_by_id(name_idx).unwrap().status,
            IndexStatus::Building
        );
        assert_eq!(loaded.find_index(users, age).unwrap().id, age_idx);
        assert!(loaded.find_index(users, name).is_some());
        // Stabilität über Reload.
        assert_eq!(loaded.find_index(users, age).unwrap().id, age_idx);

        // drop
        loaded.drop_index(age_idx);
        assert!(loaded.index_by_id(age_idx).is_none());
    }
}
