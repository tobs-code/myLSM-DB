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
//!
//! Seit v0.4 gibt es Transaktionen: Ein `Transaction` überlagert die committete
//! Engine mit einem Pending-Puffer (`TxMutator`), sodass Reads eigene Writes
//! sehen. Erst `commit()` macht die Pending-Mutationen über einen atomaren
//! WAL-Block (`Begin` → `TxPut`/`TxDelete` → `Commit` → fsync) dauerhaft und
//! wendet sie auf die MemTable an.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Index;
use std::path::{Path, PathBuf};

use crate::codec::{self, Value};
use crate::error::{Error, Result};
use crate::index::{self, FindOp};
use crate::keycodec;
use crate::ordering;
use crate::query::{self, QueryBuilder};
use crate::schema::{IndexStatus, Schema};
use crate::{Database, DirectMutator, Mutator, Options, ScanStream};

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
    /// Feld-Satz-Hint pro Entity (collection_id, entity_id) → Feld-IDs.
    ///
    /// **5a-Heuristik, nie Wahrheitsquelle.** Nur der nicht-transaktionale
    /// `put_entity`-Pfad pflegt und konsultiert ihn; er wird beim Öffnen
    /// verworfen (→ Cold-Scan) und nach Transaktions-Commit invalidiert.
    /// Erlaubt im warmen Pfad gezielte Point-Lookups statt des vollständigen
    /// Entity-Range-Scans (Read-Amplification v0.7-B).
    field_hint: HashMap<(u32, Vec<u8>), HashSet<u32>>,
    /// **Prototyp v0.7-write-cache, Variante A** (nur mit `bench-diag`):
    /// Alter-Wert-Cache indexierter Felder pro Entity — `(collection_id,
    /// entity_id)` → `field_id → Value`.
    ///
    /// Nie eine Wahrheitsquelle: Er wird nur vom nicht-transaktionalen
    /// `put_entity`-Pfad write-through befüllt und konsultiert; bei
    /// Cache-Miss wird exakt der bestehende Point-Lookup (`Mutator::get`)
    /// verwendet. Reopen leert ihn; Delete/Transaktions-Commit invalidieren
    /// die betroffenen Entities. Wird der Prototyp verworfen, entfernt dieses
    /// Feld samt Pfad im Release-Build jeden Overhead.
    #[cfg(feature = "bench-diag")]
    value_cache: HashMap<(u32, Vec<u8>), HashMap<u32, Value>>,
}

/// Ein Handle auf eine einzelne Collection, das `put`/`get`/`delete` auf
/// Entitäten ermöglicht. Bricht die Namen zu stabilen IDs herunter.
pub struct CollectionHandle<'a> {
    store: &'a mut EntityStore,
    collection_id: u32,
}

/// Zustand einer Transaktion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxState {
    Active,
    Committed,
    Aborted,
}

/// Eine (Einzel-)Transaktion über einem [`EntityStore`].
///
/// - Reads (`get`/`scan_collection`/`find`) sehen committete Daten UND die
///   eigenen, noch uncommitteten Schreiboperationen (Read-your-own-writes).
/// - Writes (`update`/`delete`) werden nur in den Pending-Puffer geschrieben;
///   nichts wird persistent, solange nicht `commit()` gerufen wird.
/// - `commit()` schreibt atomar `Begin` → `TxPut`/`TxDelete` → `Commit` in die
///   WAL, führt ein `fsync` (= Commit-Point) aus und wendet danach alle
///   Mutationen auf die MemTable an. Nach dem `Commit`-Record im WAL darf das
///   MemTable-Apply nicht mehr fehlschlagen.
/// - `abort()`/`drop` ohne Commit: Pending wird verworfen, es ist nichts
///   dauerhaft (auch der WAL wurde nicht berührt).
///
/// Es gibt bewusst keine Concurrency: `Transaction` lehnt sich an
/// `&mut EntityStore` an, sodass nur eine Transaktion pro Store gleichzeitig
/// existieren kann.
pub struct Transaction<'a> {
    store: &'a mut EntityStore,
    id: u64,
    pending: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    state: TxState,
}

/// Transaktionale KV-Sicht: committed Engine überlagert mit dem Pending-Puffer.
/// Lookup-Reihenfolge: **Pending zuerst, dann committed.** Bei `scan()` ein
/// Merge aus committed + pending, wobei Pending (inkl. Tombstones) gewinnt.
struct TxMutator<'a> {
    db: &'a mut Database,
    pending: &'a mut BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl<'a> Mutator for TxMutator<'a> {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(v) = self.pending.get(key) {
            return Ok(v.clone());
        }
        self.db.get(key)
    }
    fn scan<'s>(&'s mut self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<ScanStream<'s>> {
        let committed = self.db.scan_stream(start, end)?;
        let mut tx = TxScan {
            committed: Box::new(committed),
            pending: self.pending.iter(),
            pending_buf: None,
            committed_buf: None,
            start: start.map(Into::into),
            end: end.map(Into::into),
        };
        tx.load_pending();
        Ok(Box::new(tx))
    }
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.pending.insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    }
    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.pending.insert(key.to_vec(), None);
        Ok(())
    }
}

/// Lazy 2-Wege-Merge über committed Stream + Pending (in Sortierreihenfolge).
/// **Pending gewinnt** bei gleichem Key (inkl. Tombstones), der committed
/// Eintrag wird dann übersprungen (Shadowing).
struct TxScan<'s> {
    committed: crate::ScanStream<'s>,
    pending: std::collections::btree_map::Iter<'s, Vec<u8>, Option<Vec<u8>>>,
    pending_buf: Option<(Vec<u8>, Option<Vec<u8>>)>,
    committed_buf: Option<(Vec<u8>, Option<Vec<u8>>)>,
    start: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
}

impl<'s> TxScan<'s> {
    fn in_range(&self, k: &[u8]) -> bool {
        self.start.as_ref().is_none_or(|s| k >= s.as_slice())
            && self.end.as_ref().is_none_or(|e| k < e.as_slice())
    }
    fn load_pending(&mut self) {
        while self.pending_buf.is_none() {
            match self.pending.next() {
                None => break,
                Some((k, _v)) if !self.in_range(k) => continue,
                Some((k, v)) => self.pending_buf = Some((k.clone(), v.clone())),
            }
        }
    }
    fn load_committed(&mut self) -> Result<()> {
        while self.committed_buf.is_none() {
            match self.committed.next() {
                None => break,
                Some(Err(e)) => return Err(e),
                Some(Ok(x)) => self.committed_buf = Some(x),
            }
        }
        Ok(())
    }
}

impl<'s> Iterator for TxScan<'s> {
    type Item = Result<(Vec<u8>, Option<Vec<u8>>)>;
    fn next(&mut self) -> Option<Self::Item> {
        self.load_pending();
        if let Err(e) = self.load_committed() {
            return Some(Err(e));
        }
        match (self.committed_buf.take(), self.pending_buf.clone()) {
            (None, None) => None,
            // Nur noch Pending übrig.
            (None, Some(p)) => {
                self.pending_buf = None;
                Some(Ok(p))
            }
            // Nur noch committed übrig.
            (Some(c), None) => Some(Ok(c)),
            (Some(c), Some(p)) => match p.0.cmp(&c.0) {
                // Gleicher Key: Pending schattet committed (inkl. Tombstone).
                std::cmp::Ordering::Equal => {
                    self.pending_buf = None;
                    Some(Ok(p))
                }
                // Pending ist strikt kleiner: emittieren, committed behalten.
                std::cmp::Ordering::Less => {
                    self.committed_buf = Some(c);
                    self.pending_buf = None;
                    Some(Ok(p))
                }
                // Committed ist kleiner: emittieren, Pending behalten.
                std::cmp::Ordering::Greater => Some(Ok(c)),
            },
        }
    }
}

/// READY-Index-Feld-IDs einer Collection.
fn indexed_field_ids(schema: &Schema, collection_id: u32) -> Vec<u32> {
    schema
        .indexes()
        .iter()
        .filter(|i| i.collection_id == collection_id && i.status == IndexStatus::Ready)
        .map(|i| i.field_id)
        .collect()
}

/// Kern-Implementierung von `put_entity`, generalisiert über eine Mutator-Sicht
/// (committed ODER transaktional). Pflegt ggf. Sekundärindizes — in dieser
/// Reihenfolge, damit der Index temporär immer ein Superset (nie ein Subset)
/// der korrekten Einträge ist:
///
/// ```text
/// 1. PUT neuer Index-Eintrag
/// 2. PUT Entity (neue Felder) / DELETE veraltete Entity-Felder
/// 3. DELETE alter Index-Eintrag (erst wenn die Entity den Wert nicht mehr hat)
/// ```
///
/// `hint` ist die 5a-Heuristik (siehe [`EntityStore::field_hint`]). `Some` nur
/// im nicht-transaktionalen Pfad; bei vorhandenem Hint wird der bestehende
/// Feldzustand über **gezielte Point-Lookups** statt über den vollständigen
/// Entity-Range-Scan ermittelt. `None` (transaktional, unsicherer Zustand)
/// erzwingt den sicheren Cold-Scan. Der Hint ist nie die Wahrheitsquelle: Ein
/// fehlender Hint (z. B. nach Reopen) fällt auf den Scan zurück.
fn core_put_entity(
    schema: &mut Schema,
    m: &mut impl Mutator,
    mut hint: Option<&mut HashMap<(u32, Vec<u8>), HashSet<u32>>>,
    #[cfg(feature = "bench-diag")]
    mut value_cache: Option<&mut HashMap<(u32, Vec<u8>), HashMap<u32, Value>>>,
    collection_id: u32,
    entity_id: &[u8],
    entity: &Entity,
) -> Result<()> {
    // Entity-ID muss valid-UTF-8 sein (API-Contract); rohe, nicht-UTF-8-Bytes
    // werden abgelehnt, nie persistiert.
    std::str::from_utf8(entity_id)
        .map_err(|_| Error::InvalidArgument("entity id is not valid utf8".into()))?;
    // Zuerst alle Feld-IDs vergeben. Persistiert wird vom Caller (Commit bzw.
    // nicht-transaktionaler Pfad), damit eine abgebrochene Transaktion kein
    // Schema schreibt, BEVOR sie committed ist.
    let mut written: Vec<(u32, &Value)> = Vec::with_capacity(entity.fields.len());
    #[cfg(feature = "bench-diag")]
    let t_fid = std::time::Instant::now();
    for (name, value) in &entity.fields {
        let field_id = schema.field_id(collection_id, name);
        written.push((field_id, value));
    }
    #[cfg(feature = "bench-diag")]
    if crate::diag::active() {
        crate::diag::add_field_id_us(t_fid.elapsed().as_micros() as u64);
    }

    let new_ids: HashSet<u32> = written.iter().map(|(f, _)| *f).collect();
    #[cfg(feature = "bench-diag")]
    let t_idxf = std::time::Instant::now();
    let indexed = indexed_field_ids(schema, collection_id);
    #[cfg(feature = "bench-diag")]
    if crate::diag::active() {
        crate::diag::add_idx_fields_us(t_idxf.elapsed().as_micros() as u64);
    }

    // Bisherige Felder der Entität ermitteln (für Stale-Removal + Index-Diff).
    // Warm (Hint vorhanden): gezielte Point-Lookups der infrage kommenden Felder.
    // Cold (kein Hint / unsicher): vollständiger Entity-Range-Scan.
    let (old_values, stale_keys): (HashMap<u32, Value>, Vec<Vec<u8>>) = {
        let mut old_values: HashMap<u32, Value> = HashMap::new();
        let mut stale_keys: Vec<Vec<u8>> = Vec::new();
        let mut cold = true;
        if let Some(map) = hint.as_mut() {
            let key = (collection_id, entity_id.to_vec());
            // Prototyp: gecachte alte Werte indexierter Felder dieser Entity.
            #[cfg(feature = "bench-diag")]
            let cached_values: HashMap<u32, Value> = value_cache
                .as_mut()
                .map_or(HashMap::new(), |m| m.get(&key).cloned().unwrap_or_default());
            #[cfg(feature = "bench-diag")]
            let t_hint = std::time::Instant::now();
            let hint_hit = map.get(&key).cloned();
            let stale_cands: Vec<u32> = hint_hit
                .as_ref()
                .map_or(Vec::new(), |h| h.difference(&new_ids).copied().collect());
            #[cfg(feature = "bench-diag")]
            if crate::diag::active() {
                crate::diag::add_put_hint_us(t_hint.elapsed().as_micros() as u64);
            }
            if hint_hit.is_some() {
                cold = false;
                // Stale-Kandidaten: Felder, die die Entity lt. Hint hatte, jetzt
                // aber nicht mehr geschrieben werden. Per Point-Lookup verifizieren.
                for fid in stale_cands {
                    let k = keycodec::encode_entity_key(collection_id, entity_id, fid);
                    if let Some(bytes) = m.get(&k)? {
                        stale_keys.push(k);
                        if indexed.contains(&fid) {
                            if let Ok(v) = codec::decode(&bytes) {
                                old_values.insert(fid, v);
                            }
                        }
                    }
                }
                // Alte Werte indexierter geschriebener Felder (für den Index-Diff).
                // Prototyp: Cache-Hit liefert den Wert ohne Disk-Read (→ get_us ≈ 0);
                // Cache-Miss fällt exakt auf den bestehenden Point-Lookup zurück.
                for (fid, _value) in &written {
                    if !indexed.contains(fid) {
                        continue;
                    }
                    #[cfg(feature = "bench-diag")]
                    if let Some(v) = cached_values.get(fid) {
                        old_values.insert(*fid, v.clone());
                        crate::diag::add_cache_hit();
                        continue;
                    }
                    let k = keycodec::encode_entity_key(collection_id, entity_id, *fid);
                    if let Some(bytes) = m.get(&k)? {
                        if let Ok(v) = codec::decode(&bytes) {
                            old_values.insert(*fid, v);
                        }
                    }
                    #[cfg(feature = "bench-diag")]
                    crate::diag::add_cache_miss();
                }
            }
        }
        if cold {
            let (start, end) = keycodec::entity_range(collection_id, entity_id);
            #[cfg(feature = "bench-diag")]
            let t_scan = std::time::Instant::now();
            let existing: Vec<(Vec<u8>, Option<Vec<u8>>)> =
                m.scan(Some(&start), end.as_deref())?
                    .collect::<std::result::Result<_, _>>()?;
            #[cfg(feature = "bench-diag")]
            if crate::diag::active() {
                crate::diag::add_scan_collect_us(t_scan.elapsed().as_micros() as u64);
            }
            for (key, value_opt) in &existing {
                if let Some((_, _, field_id)) = keycodec::decode_entity_key(key) {
                    if !new_ids.contains(&field_id) {
                        stale_keys.push(key.clone());
                    }
                    if let Some(v) = value_opt {
                        if let Ok(val) = codec::decode(v) {
                            old_values.insert(field_id, val);
                        }
                    }
                }
            }
        }
        (old_values, stale_keys)
    };

    // 1) Neue Index-Einträge für geänderte/neu indexierte Felder schreiben
    //    (Bevor die Entity aktualisiert wird → kein False Negative).
    for (field_id, value) in &written {
        if !indexed.contains(field_id) {
            continue;
        }
        let changed = old_values.get(field_id) != Some(*value);
        if changed {
            #[cfg(feature = "bench-diag")]
            let t_enc = std::time::Instant::now();
            let ik = keycodec::encode_index_key(
                collection_id,
                *field_id,
                &ordering::encode_ordered(value),
                entity_id,
            );
            #[cfg(feature = "bench-diag")]
            if crate::diag::active() {
                crate::diag::add_idx_enc_us(t_enc.elapsed().as_micros() as u64);
            }
            m.put(&ik, &[])?;
        }
    }

    // 2) Entity-Felder schreiben + veraltete entfernen.
    for key in &stale_keys {
        m.delete(key)?;
    }
    for (field_id, value) in &written {
        let key = keycodec::encode_entity_key(collection_id, entity_id, *field_id);
        #[cfg(feature = "bench-diag")]
        let t_enc = std::time::Instant::now();
        let enc = codec::encode(value);
        #[cfg(feature = "bench-diag")]
        if crate::diag::active() {
            crate::diag::add_put_fieldenc_us(t_enc.elapsed().as_micros() as u64);
        }
        m.put(&key, &enc)?;
    }

    // 3) Alte Index-Einträge löschen — erst nachdem die Entity den Wert
    //    nicht mehr hat (sonst entstünde ein False Negative).
    for (field_id, old_value) in &old_values {
        if !indexed.contains(field_id) {
            continue;
        }
        let now_has_same = written
            .iter()
            .any(|(f, v)| f == field_id && *v == old_value);
        if !now_has_same {
            let ik = keycodec::encode_index_key(
                collection_id,
                *field_id,
                &ordering::encode_ordered(old_value),
                entity_id,
            );
            m.delete(&ik)?;
        }
    }

    // Hint aktualisieren: Die Entity hat danach genau die geschriebenen Felder.
    if let Some(map) = hint.as_mut() {
        map.insert((collection_id, entity_id.to_vec()), new_ids);
    }
    // Prototyp: Alte Werte indexierter Felder write-through aktualisieren. Die
    // Entity besitzt danach genau die geschriebenen indexierten Feldwerte; ein
    // späteres Update kann sie direkt für den Index-Diff nutzen (kein Disk-Read).
    // Entfernte Felder fallen automatisch heraus (whole-map-Ersetzung).
    #[cfg(feature = "bench-diag")]
    if let Some(map) = value_cache.as_mut() {
        let new_values: HashMap<u32, Value> = written
            .iter()
            .filter(|(f, _)| indexed.contains(f))
            .map(|&(f, v)| (f, v.clone()))
            .collect();
        map.insert((collection_id, entity_id.to_vec()), new_values);
    }
    Ok(())
}

/// Kern-Implementierung von `delete_entity`, generalisiert über eine Mutator-Sicht.
fn core_delete_entity(
    schema: &mut Schema,
    m: &mut impl Mutator,
    collection_id: u32,
    entity_id: &[u8],
) -> Result<()> {
    std::str::from_utf8(entity_id)
        .map_err(|_| Error::InvalidArgument("entity id is not valid utf8".into()))?;
    let (start, end) = keycodec::entity_range(collection_id, entity_id);
    let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = m
        .scan(Some(&start), end.as_deref())?
        .collect::<std::result::Result<_, _>>()?;
    // Erst die Feldwerte einsammeln (für die Index-Bereinigung) und die
    // Entity-Keys löschen; danach die Index-Einträge entfernen.
    let indexed = indexed_field_ids(schema, collection_id);
    let mut index_ops: Vec<(u32, Value)> = Vec::new();
    for (key, value_opt) in &rows {
        if let Some((_, _, field_id)) = keycodec::decode_entity_key(key) {
            if indexed.contains(&field_id) {
                if let Some(v) = value_opt {
                    if let Ok(val) = codec::decode(v) {
                        index_ops.push((field_id, val));
                    }
                }
            }
            m.delete(key)?;
        }
    }
    for (field_id, value) in index_ops {
        let ik = keycodec::encode_index_key(
            collection_id,
            field_id,
            &ordering::encode_ordered(&value),
            entity_id,
        );
        m.delete(&ik)?;
    }
    Ok(())
}

/// Kern-Implementierung von `get_entity`, generalisiert über eine Mutator-Sicht.
pub(crate) fn core_get_entity(
    schema: &Schema,
    m: &mut impl Mutator,
    collection_id: u32,
    entity_id: &[u8],
) -> Result<Option<Entity>> {
    let (start, end) = keycodec::entity_range(collection_id, entity_id);
    let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = m
        .scan(Some(&start), end.as_deref())?
        .collect::<std::result::Result<_, _>>()?;
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
        let name = schema
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

/// Kern-Implementierung von `scan_collection`, generalisiert über eine Mutator-Sicht.
pub(crate) fn core_scan_collection(
    schema: &Schema,
    m: &mut impl Mutator,
    collection_id: u32,
) -> Result<Vec<(String, Entity)>> {
    let pstart = keycodec::collection_prefix(collection_id);
    let pend = keycodec::successor(&pstart);
    let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = m
        .scan(Some(&pstart), pend.as_deref())?
        .collect::<std::result::Result<_, _>>()?;
    let mut map: BTreeMap<Vec<u8>, Entity> = Default::default();
    for (key, value_opt) in rows {
        let Some((_, ee, ef)) = keycodec::decode_entity_key(&key) else {
            continue;
        };
        let Some(bytes) = value_opt else {
            continue; // Tombstone.
        };
        let value = codec::decode(&bytes)?;
        let name = schema
            .field_name(collection_id, ef)
            .ok_or_else(|| Error::InvalidFormat(format!("unknown field id {ef}")))?;
        map.entry(ee.to_vec())
            .or_default()
            .fields
            .push((name.to_string(), value));
    }
    let mut out = Vec::with_capacity(map.len());
    for (ee, entity) in map {
        if !entity.fields.is_empty() {
            out.push((keycodec::decode_entity_id(&ee)?.to_string(), entity));
        }
    }
    Ok(out)
}

impl EntityStore {
    /// Öffnet (oder erstellt) einen Entitäts-Store in `dir`. Legt darunter eine
    /// v0.1-KV-Engine an und lädt das persistente Schema.
    pub fn open(dir: impl AsRef<std::path::Path>) -> Result<EntityStore> {
        Self::open_with(dir, Options::default())
    }

    /// Öffnet (oder erstellt) einen Entitäts-Store mit angepassten Engine-Optionen
    /// (z. B. größere MemTable-Limits für Diagnose-/Benchmark-Zwecke).
    pub fn open_with(dir: impl AsRef<std::path::Path>, opts: Options) -> Result<EntityStore> {
        let dir = dir.as_ref();
        let db = Database::open_with(dir, opts)?;
        let schema_path = dir.join("SCHEMA");
        let schema = Schema::load(&schema_path)?;
        let mut store = EntityStore {
            db,
            schema,
            schema_path,
            field_hint: HashMap::new(),
            #[cfg(feature = "bench-diag")]
            value_cache: HashMap::new(),
        };
        // Noch nicht fertige Indizes (BUILDING nach einem Crash) neu aufbauen.
        store.recover_indexes()?;
        Ok(store)
    }

    /// Gibt ein Handle auf eine Collection. Existiert die Collection noch
    /// nicht, wird sie (und ihre stabile ID) neu angelegt.
    pub fn collection<'a>(&'a mut self, name: &str) -> Result<CollectionHandle<'a>> {
        let collection_id = self.schema.collection_id(name);
        self.persist_schema()?;
        Ok(CollectionHandle {
            store: self,
            collection_id,
        })
    }

    /// Eröffnet eine neue Transaktion. Solange `tx` lebt, ist der Store
    /// exklusiv (keine parallelen Schreiber).
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        let id = self.db.alloc_tx_id();
        Ok(Transaction {
            store: self,
            id,
            pending: BTreeMap::new(),
            state: TxState::Active,
        })
    }

    /// Bequemlichkeits-Wrapper: führt `f` in einer Transaktion aus und
    /// committet bei `Ok`, bricht bei `Err` ab.
    pub fn transaction_with<T>(
        &mut self,
        f: impl FnOnce(&mut Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut tx = self.transaction()?;
        match f(&mut tx) {
            Ok(v) => {
                tx.commit()?;
                Ok(v)
            }
            Err(e) => {
                tx.abort()?;
                Err(e)
            }
        }
    }

    pub fn close(mut self) -> Result<()> {
        self.persist_schema()?;
        self.db.close()
    }

    /// Erzwingt das Flushen der MemTable (für Tests/Admin).
    pub fn flush(&mut self) -> Result<()> {
        self.db.flush()
    }

    /// Die beim Öffnen ermittelte Format-Version der Datenbank.
    pub fn format_version(&self) -> u32 {
        self.db.format_version()
    }

    /// Erstellt ein konsistentes Backup der Datenbank (delegiert an
    /// [`crate::Database::backup`]). Startet exklusiv über `&mut self`;
    /// pending Direct-Writes werden vor dem Kopieren durch `flush()` persistent
    /// gemacht.
    pub fn backup(&mut self, dest: &Path) -> Result<usize> {
        self.db.backup(dest)
    }

    /// Stellt eine zuvor erzeugte Backup-Kopie wieder her (delegiert an
    /// [`crate::Database::restore`]). Siehe Vertrag in [`crate::Database::restore`].
    pub fn restore(src: &Path, dest: &Path) -> Result<usize> {
        crate::Database::restore(src, dest)
    }

    /// Scannt alle Entities einer Collection. (Auch als Oracle für Tests.)
    pub fn scan_collection(&mut self, name: &str) -> Result<Vec<(String, Entity)>> {
        let Some(collection_id) = self.schema.lookup_collection_id(name) else {
            return Ok(Vec::new());
        };
        let mut m = DirectMutator { db: &mut self.db };
        core_scan_collection(&self.schema, &mut m, collection_id)
    }

    /// Schreibt ein neues Schema, falls sich die Registry seit dem letzten
    /// `save` geändert hat.
    fn persist_schema(&mut self) -> Result<()> {
        if self.schema.is_changed() {
            self.schema.save(&self.schema_path)?;
        }
        Ok(())
    }

    /// Legt eine Entität an bzw. ersetzt sie (nicht-transaktional).
    pub fn put_entity(
        &mut self,
        collection_id: u32,
        entity_id: &[u8],
        entity: &Entity,
    ) -> Result<()> {
        #[cfg(feature = "bench-diag")]
        let t = std::time::Instant::now();
        {
            let schema = &mut self.schema;
            let mut m = DirectMutator { db: &mut self.db };
            core_put_entity(
                schema,
                &mut m,
                Some(&mut self.field_hint),
                #[cfg(feature = "bench-diag")]
                Some(&mut self.value_cache),
                collection_id,
                entity_id,
                entity,
            )?;
        }
        self.persist_schema()?;
        #[cfg(feature = "bench-diag")]
        if crate::diag::active() {
            crate::diag::add_entity_put_us(t.elapsed().as_micros() as u64);
        }
        Ok(())
    }

    /// Liest eine Entität vollständig aus ihren Feld-Keys und rekonstruiert sie.
    pub fn get_entity(&mut self, collection_id: u32, entity_id: &[u8]) -> Result<Option<Entity>> {
        let mut m = DirectMutator { db: &mut self.db };
        core_get_entity(&self.schema, &mut m, collection_id, entity_id)
    }

    /// Löscht alle Feld-Keys einer Entität (und deren Index-Einträge).
    pub fn delete_entity(&mut self, collection_id: u32, entity_id: &[u8]) -> Result<()> {
        {
            let schema = &mut self.schema;
            let mut m = DirectMutator { db: &mut self.db };
            core_delete_entity(schema, &mut m, collection_id, entity_id)?;
        }
        // Hint entfernen: die Entity existiert nicht mehr; ein späterer Put muss
        // über den Cold-Scan den tatsächlichen (leeren) Zustand feststellen.
        self.field_hint.remove(&(collection_id, entity_id.to_vec()));
        #[cfg(feature = "bench-diag")]
        self.value_cache
            .remove(&(collection_id, entity_id.to_vec()));
        Ok(())
    }

    /// Legt einen Index auf einem Feld an. Existiert bereits ein Index, ist das
    /// ein No-Op. Statuswechsel: BUILDING (fsync) → Aufbau → READY (fsync).
    pub fn create_index(&mut self, collection_id: u32, field_id: u32) -> Result<()> {
        let id = self.schema.create_index(collection_id, field_id);
        self.schema.save(&self.schema_path)?; // BUILDING dauerhaft
        index::rebuild(&mut self.db, collection_id, field_id)?;
        self.schema.set_index_ready(id);
        self.schema.save(&self.schema_path)?; // READY dauerhaft
        Ok(())
    }

    /// Löscht einen Index (Definition + alle Index-Keys).
    pub fn drop_index(&mut self, collection_id: u32, field_id: u32) -> Result<()> {
        if let Some(def) = self.schema.find_index(collection_id, field_id) {
            index::clear(&mut self.db, collection_id, field_id)?;
            self.schema.drop_index(def.id);
            self.schema.save(&self.schema_path)?;
        }
        Ok(())
    }

    /// Baut alle noch nicht fertigen Indizes nach einem Open neu auf
    /// (idempotent, vollständiger Rebuild statt Reparatur).
    fn recover_indexes(&mut self) -> Result<()> {
        let pending: Vec<(u32, u32)> = self
            .schema
            .building_indexes()
            .map(|i| (i.collection_id, i.field_id))
            .collect();
        for (c, f) in pending {
            self.create_index(c, f)?;
        }
        Ok(())
    }

    /// Startet eine Query auf einer Collection (v0.5, read-only). Existiert die
    /// Collection nicht, liefert die Query ein leeres Ergebnis — es wird
    /// **kein** Schema-Eintrag angelegt.
    pub fn query(&mut self, collection: &str) -> Result<QueryBuilder> {
        Ok(QueryBuilder::new(collection))
    }

    /// Plant und führt eine Query aus. Ergebnis: `Vec<(Entity-ID, Entity)>`
    /// (wie `scan_collection`). Die Query ist rein lesend.
    ///
    /// Ist am Builder eine Aggregation gesetzt, wird `execute_aggregate`
    /// verlangt (beide Terminal-Schritte sind exklusiv). Eine leere
    /// Projektions-Feldliste ist ein `InvalidArgument`-Fehler.
    pub fn execute_query(&mut self, builder: QueryBuilder) -> Result<Vec<(String, Entity)>> {
        let projection = builder.projection.clone();
        let aggregation = builder.aggregation.clone();
        if aggregation.is_some() {
            return Err(Error::InvalidArgument(
                "query has an aggregation; use execute_aggregate".into(),
            ));
        }
        let projection = match projection {
            Some(p) if p.is_empty() => {
                return Err(Error::InvalidArgument("empty projection".into()));
            }
            Some(p) => Some(p),
            None => None,
        };
        let logical = builder.build();
        let physical = query::planner::plan(&self.schema, logical);
        let rows = query::executor::run(&mut self.db, &self.schema, &physical)?;
        Ok(match projection {
            Some(fields) => query::executor::project_rows(rows, &fields),
            None => rows,
        })
    }

    /// Aggregiert über das (gefilterte, sortierte, limitierte) Ergebnis einer
    /// Query. Liefert `Option<Value>` (SQL-artiges `NULL` = `None` bei leerer /
    /// Null-wertiger Menge, außer `Count`). Eine Projektion am Builder ist
    /// nicht zulässig (Terminal-Schritte sind exklusiv).
    pub fn execute_aggregate(&mut self, builder: QueryBuilder) -> Result<Option<Value>> {
        if builder.projection.is_some() {
            return Err(Error::InvalidArgument(
                "query has a projection; aggregation is mutually exclusive".into(),
            ));
        }
        let aggregation = match &builder.aggregation {
            Some(a) => a.clone(),
            None => return Err(Error::InvalidArgument("no aggregation specified".into())),
        };
        let logical = builder.build();
        let physical = query::planner::plan(&self.schema, logical);
        let rows = query::executor::run(&mut self.db, &self.schema, &physical)?;
        query::executor::aggregate_rows(&rows, &aggregation)
    }

    /// Plant eine Query und liefert die Text-Baumdarstellung des Physical Plans
    /// (Debugging des Optimizers — keine Ausführung).
    pub fn explain_query(&self, builder: &QueryBuilder) -> Result<String> {
        let logical = builder.clone().build();
        let physical = query::planner::plan(&self.schema, logical);
        Ok(query::explain::format(&physical))
    }
}

impl<'a> Transaction<'a> {
    fn check_active(&self) -> Result<()> {
        if self.state != TxState::Active {
            return Err(Error::InvalidArgument("transaction is not active".into()));
        }
        Ok(())
    }

    /// Transaktions-ID (monoton, nie wiederverwendet).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Legt eine Entität an bzw. ersetzt sie innerhalb der Transaktion.
    pub fn update(&mut self, collection: &str, entity_id: &str, entity: &Entity) -> Result<()> {
        self.check_active()?;
        let collection_id = self.store.schema.collection_id(collection);
        let (schema, db, pending) = (
            &mut self.store.schema,
            &mut self.store.db,
            &mut self.pending,
        );
        let mut m = TxMutator { db, pending };
        core_put_entity(
            schema,
            &mut m,
            None,
            #[cfg(feature = "bench-diag")]
            None,
            collection_id,
            entity_id.as_bytes(),
            entity,
        )
    }

    /// Löscht eine Entität innerhalb der Transaktion.
    pub fn delete(&mut self, collection: &str, entity_id: &str) -> Result<()> {
        self.check_active()?;
        let collection_id = self.store.schema.collection_id(collection);
        let (schema, db, pending) = (
            &mut self.store.schema,
            &mut self.store.db,
            &mut self.pending,
        );
        let mut m = TxMutator { db, pending };
        core_delete_entity(schema, &mut m, collection_id, entity_id.as_bytes())
    }

    /// Liest eine Entität — committed + eigene uncommittete Writes.
    pub fn get(&mut self, collection: &str, entity_id: &str) -> Result<Option<Entity>> {
        self.check_active()?;
        let Some(collection_id) = self.store.schema.lookup_collection_id(collection) else {
            return Ok(None);
        };
        let (start, end) = keycodec::entity_range(collection_id, entity_id.as_bytes());
        let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = {
            let mut m = TxMutator {
                db: &mut self.store.db,
                pending: &mut self.pending,
            };
            m.scan(Some(&start), end.as_deref())?
                .collect::<std::result::Result<_, _>>()?
        };
        core_get_entity(
            &self.store.schema,
            &mut DirectScan { rows: &rows },
            collection_id,
            entity_id.as_bytes(),
        )
    }

    /// Scannt alle Entities einer Collection — committed + eigene Writes.
    pub fn scan_collection(&mut self, collection: &str) -> Result<Vec<(String, Entity)>> {
        self.check_active()?;
        let Some(collection_id) = self.store.schema.lookup_collection_id(collection) else {
            return Ok(Vec::new());
        };
        let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = {
            let mut m = TxMutator {
                db: &mut self.store.db,
                pending: &mut self.pending,
            };
            let pstart = keycodec::collection_prefix(collection_id);
            let pend = keycodec::successor(&pstart);
            m.scan(Some(&pstart), pend.as_deref())?
                .collect::<std::result::Result<_, _>>()?
        };
        core_scan_collection(
            &self.store.schema,
            &mut DirectScan { rows: &rows },
            collection_id,
        )
    }

    /// Index-Abfrage — committed + eigene uncommittete Index-Writes.
    pub fn find(&mut self, collection: &str, field: &str, op: FindOp) -> Result<Vec<String>> {
        self.check_active()?;
        let Some(collection_id) = self.store.schema.lookup_collection_id(collection) else {
            return Ok(Vec::new());
        };
        let field_id = self
            .store
            .schema
            .lookup_field_id(collection_id, field)
            .ok_or_else(|| Error::InvalidArgument(format!("unknown field {field}")))?;
        let (lower, upper) = op.to_bounds();
        let mut m = TxMutator {
            db: &mut self.store.db,
            pending: &mut self.pending,
        };
        index::find_m(
            &mut m,
            &self.store.schema,
            collection_id,
            field_id,
            &lower,
            &upper,
        )
    }

    /// Startet eine Query auf einer Collection (v0.6, read-only). Existiert die
    /// Collection nicht, liefert die Query ein leeres Ergebnis — es wird
    /// **kein** Schema-Eintrag angelegt.
    pub fn query(&mut self, collection: &str) -> Result<QueryBuilder> {
        Ok(QueryBuilder::new(collection))
    }

    /// Plant und führt eine Query **innerhalb der Transaktion** aus (eager über
    /// das Pending-Overlay). Read-your-own-writes: sieht committete Daten UND
    /// die eigenen, noch uncommitteten Writes — für Entity- und Index-Daten
    /// über dasselbe Overlay. Schema bleibt read-only.
    ///
    /// Eine Aggregation am Builder verlangt `execute_aggregate`; eine leere
    /// Projektions-Feldliste ist ein `InvalidArgument`-Fehler.
    pub fn execute_query(&mut self, builder: QueryBuilder) -> Result<Vec<(String, Entity)>> {
        self.check_active()?;
        let projection = builder.projection.clone();
        let aggregation = builder.aggregation.clone();
        if aggregation.is_some() {
            return Err(Error::InvalidArgument(
                "query has an aggregation; use execute_aggregate".into(),
            ));
        }
        let projection = match projection {
            Some(p) if p.is_empty() => {
                return Err(Error::InvalidArgument("empty projection".into()));
            }
            Some(p) => Some(p),
            None => None,
        };
        let logical = builder.build();
        let physical = query::planner::plan(&self.store.schema, logical);
        let mut m = TxMutator {
            db: &mut self.store.db,
            pending: &mut self.pending,
        };
        let rows = query::executor::run_m(&mut m, &self.store.schema, &physical)?;
        Ok(match projection {
            Some(fields) => query::executor::project_rows(rows, &fields),
            None => rows,
        })
    }

    /// Aggregiert über das (gefilterte, sortierte, limitierte) Ergebnis einer
    /// Query **innerhalb der Transaktion** (siehe `EntityStore::execute_aggregate`).
    pub fn execute_aggregate(&mut self, builder: QueryBuilder) -> Result<Option<Value>> {
        self.check_active()?;
        if builder.projection.is_some() {
            return Err(Error::InvalidArgument(
                "query has a projection; aggregation is mutually exclusive".into(),
            ));
        }
        let aggregation = match &builder.aggregation {
            Some(a) => a.clone(),
            None => return Err(Error::InvalidArgument("no aggregation specified".into())),
        };
        let logical = builder.build();
        let physical = query::planner::plan(&self.store.schema, logical);
        let mut m = TxMutator {
            db: &mut self.store.db,
            pending: &mut self.pending,
        };
        let rows = query::executor::run_m(&mut m, &self.store.schema, &physical)?;
        query::executor::aggregate_rows(&rows, &aggregation)
    }

    /// Committet die Transaktion atomar: WAL (`Begin` → Mutationen → `Commit`),
    /// dann `fsync` (Commit-Point), dann MemTable-Apply. Idempotent.
    pub fn commit(&mut self) -> Result<()> {
        self.check_active()?;
        // Schema-Registry (neu vergebene Collection-/Feld-IDs) persistieren.
        self.store.persist_schema()?;
        let tx = self.id;
        {
            let db = &mut self.store.db;
            db.wal_begin(tx)?;
            for (key, value) in &self.pending {
                match value {
                    Some(v) => db.wal_tx_put(tx, key, v)?,
                    None => db.wal_tx_delete(tx, key)?,
                }
            }
            db.wal_commit(tx)?;
            db.wal_sync()?; // COMMIT POINT — ab hier durable.
        }
        // Post-Commit: MemTable-Apply darf nicht mehr fehlschlagen. Die
        // Mutationen sind bereits über den WAL durable; ein Flush-Fehler hier
        // ist best-effort (Durability bleibt über den WAL erhalten).
        {
            let db = &mut self.store.db;
            for (key, value) in &self.pending {
                match value {
                    Some(v) => db.mem_put(key, v),
                    None => db.mem_delete(key),
                }
            }
            let _ = db.flush_if_over_limit();
        }
        // Transaktionale Writes gehen am Feld-Satz-Hint vorbei → Hint für die
        // betroffenen Entities invalidieren (der nächste Put fällt auf den
        // sicheren Cold-Scan zurück; der Hint ist nie die Wahrheitsquelle).
        for key in self.pending.keys() {
            if let Some((coll, eid, _fid)) = keycodec::decode_entity_key(key) {
                self.store.field_hint.remove(&(coll, eid.to_vec()));
                #[cfg(feature = "bench-diag")]
                self.store.value_cache.remove(&(coll, eid.to_vec()));
            }
        }
        self.state = TxState::Committed;
        Ok(())
    }

    /// Bricht die Transaktion ab: verwirft alle uncommitteten Writes. Es wurde
    /// noch nichts Persistentes geschrieben, daher ist hier nichts zu tun.
    pub fn abort(&mut self) -> Result<()> {
        self.check_active()?;
        self.pending.clear();
        self.state = TxState::Aborted;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        // Kein WAL-Write ohne Commit → nichts zu bereinigen. Pending verfällt.
        if self.state == TxState::Active {
            self.pending.clear();
        }
    }
}

/// Adaptiert eine fertige `Vec<(key, Option<value>)>` als Lesesicht (für die
/// Kern-Funktionen nach einem Overlay-Scan). Schreiboperationen sind unsinnig
/// und geben einen Fehler zurück.
struct DirectScan<'a> {
    rows: &'a Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl<'a> Mutator for DirectScan<'a> {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .rows
            .iter()
            .find(|(k, _)| k.as_slice() == key)
            .and_then(|(_, v)| v.clone()))
    }
    fn scan<'s>(&'s mut self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<ScanStream<'s>> {
        let rows = self.rows;
        let start = start.map(|s| s.to_vec());
        let end = end.map(|e| e.to_vec());
        let iter = rows.iter().filter(move |(k, _)| {
            let in_start = start.as_ref().is_none_or(|s| k.as_slice() >= s.as_slice());
            let in_end = end.as_ref().is_none_or(|e| k.as_slice() < e.as_slice());
            in_start && in_end
        });
        Ok(Box::new(iter.cloned().map(Ok)))
    }
    fn put(&mut self, _key: &[u8], _value: &[u8]) -> Result<()> {
        Err(Error::InvalidArgument("read-only view".into()))
    }
    fn delete(&mut self, _key: &[u8]) -> Result<()> {
        Err(Error::InvalidArgument("read-only view".into()))
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

    /// Legt einen Index auf `field` an (erstellt ihn auch mit bestehenden Daten).
    pub fn create_index(&mut self, field: &str) -> Result<()> {
        let field_id = self.store.schema.field_id(self.collection_id, field);
        self.store.create_index(self.collection_id, field_id)
    }

    /// Löscht den Index auf `field`.
    pub fn drop_index(&mut self, field: &str) -> Result<()> {
        let field_id = match self.store.schema.lookup_field_id(self.collection_id, field) {
            Some(f) => f,
            None => return Ok(()),
        };
        self.store.drop_index(self.collection_id, field_id)
    }

    /// Führt eine Index-Abfrage aus und liefert die verifizierten Entity-IDs.
    pub fn find(&mut self, field: &str, op: FindOp) -> Result<Vec<String>> {
        let field_id = self
            .store
            .schema
            .lookup_field_id(self.collection_id, field)
            .ok_or_else(|| Error::InvalidArgument(format!("unknown field {field}")))?;
        let (lower, upper) = op.to_bounds();
        index::find(
            &mut self.store.db,
            &self.store.schema,
            self.collection_id,
            field_id,
            &lower,
            &upper,
        )
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
        store
            .collection("users")
            .unwrap()
            .put("usr_123", &entity)
            .unwrap();

        let got = store
            .collection("users")
            .unwrap()
            .get("usr_123")
            .unwrap()
            .expect("exists");
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

        let got = store
            .collection("users")
            .unwrap()
            .get("u1")
            .unwrap()
            .expect("exists");
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
        assert!(
            store
                .collection("users")
                .unwrap()
                .get("u1")
                .unwrap()
                .is_some()
        );
        store.collection("users").unwrap().delete("u1").unwrap();
        assert!(
            store
                .collection("users")
                .unwrap()
                .get("u1")
                .unwrap()
                .is_none()
        );
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
        let got = store
            .collection("users")
            .unwrap()
            .get("u1")
            .unwrap()
            .expect("exists");
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

    fn ids(v: Vec<String>) -> Vec<String> {
        let mut v = v;
        v.sort();
        v
    }

    #[test]
    fn transaction_commits_atomically_with_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        store
            .collection("users")
            .unwrap()
            .create_index("age")
            .unwrap();
        let e1 = user("Tobias", 31, true);
        let e2 = user("Anna", 40, false);
        store
            .transaction_with(|tx| {
                tx.update("users", "u1", &e1)?;
                tx.update("users", "u2", &e2)?;
                Ok(())
            })
            .unwrap();

        let mut col = store.collection("users").unwrap();
        assert_eq!(
            col.get("u1").unwrap().unwrap().field("age"),
            Some(&Value::Int(31))
        );
        assert_eq!(
            col.get("u2").unwrap().unwrap().field("age"),
            Some(&Value::Int(40))
        );
        assert_eq!(
            ids(col.find("age", FindOp::Eq(Value::Int(31))).unwrap()),
            vec!["u1"]
        );
        assert_eq!(
            ids(col.find("age", FindOp::Eq(Value::Int(40))).unwrap()),
            vec!["u2"]
        );
    }

    #[test]
    fn transaction_abort_leaves_no_trace() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        store
            .collection("users")
            .unwrap()
            .create_index("age")
            .unwrap();
        let e = user("Tobias", 31, true);
        let mut tx = store.transaction().unwrap();
        tx.update("users", "u1", &e).unwrap();
        tx.abort().unwrap();
        drop(tx);

        let mut col = store.collection("users").unwrap();
        assert!(col.get("u1").unwrap().is_none());
        assert!(
            col.find("age", FindOp::Eq(Value::Int(31)))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn transaction_with_error_aborts() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let e = user("Tobias", 31, true);
        let res: Result<()> = store.transaction_with(|tx| {
            tx.update("users", "u1", &e)?;
            Err(crate::error::Error::NotFound)
        });
        assert!(res.is_err());

        let mut col = store.collection("users").unwrap();
        assert!(col.get("u1").unwrap().is_none());
    }

    #[test]
    fn tx_reads_own_writes_for_get_scan_find() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let e = user("Tobias", 31, true);
        store.collection("users").unwrap().put("u1", &e).unwrap();
        store
            .collection("users")
            .unwrap()
            .create_index("age")
            .unwrap();

        let mut tx = store.transaction().unwrap();
        // Committete Base ist sichtbar.
        assert_eq!(
            tx.get("users", "u1").unwrap().unwrap().field("age"),
            Some(&Value::Int(31))
        );
        assert_eq!(
            ids(tx.find("users", "age", FindOp::Eq(Value::Int(31))).unwrap()),
            vec!["u1"]
        );

        // update A → get/scan/find sehen 32.
        let mut e32 = e.clone();
        e32.fields.iter_mut().find(|(n, _)| n == "age").unwrap().1 = Value::Int(32);
        tx.update("users", "u1", &e32).unwrap();
        assert_eq!(
            tx.get("users", "u1").unwrap().unwrap().field("age"),
            Some(&Value::Int(32))
        );
        let scan = tx.scan_collection("users").unwrap();
        assert_eq!(scan[0].1.field("age"), Some(&Value::Int(32)));
        assert!(
            tx.find("users", "age", FindOp::Eq(Value::Int(31)))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(tx.find("users", "age", FindOp::Eq(Value::Int(32))).unwrap()),
            vec!["u1"]
        );
        tx.commit().unwrap();
        drop(tx);

        // Nach Commit ist das auch ausserhalb sichtbar.
        let mut col = store.collection("users").unwrap();
        assert_eq!(
            col.get("u1").unwrap().unwrap().field("age"),
            Some(&Value::Int(32))
        );
    }

    #[test]
    fn tx_read_your_own_writes_matrix() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        {
            let mut col = store.collection("users").unwrap();
            col.create_index("age").unwrap();
            col.put("u1", &user("x", 30, true)).unwrap();
        }

        let mut tx = store.transaction().unwrap();
        // put A, put A, put A
        tx.update("users", "u1", &user("x", 31, true)).unwrap();
        tx.update("users", "u1", &user("x", 32, true)).unwrap();
        tx.update("users", "u1", &user("x", 33, true)).unwrap();
        assert_eq!(
            tx.get("users", "u1").unwrap().unwrap().field("age"),
            Some(&Value::Int(33))
        );
        assert!(
            tx.find("users", "age", FindOp::Eq(Value::Int(30)))
                .unwrap()
                .is_empty()
        );
        assert!(
            tx.find("users", "age", FindOp::Eq(Value::Int(31)))
                .unwrap()
                .is_empty()
        );
        assert!(
            tx.find("users", "age", FindOp::Eq(Value::Int(32)))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(tx.find("users", "age", FindOp::Eq(Value::Int(33))).unwrap()),
            vec!["u1"]
        );
        tx.commit().unwrap();
        drop(tx);

        // put A, delete A
        let mut tx = store.transaction().unwrap();
        tx.update("users", "u1", &user("x", 40, true)).unwrap();
        tx.delete("users", "u1").unwrap();
        assert!(tx.get("users", "u1").unwrap().is_none());
        assert!(
            tx.find("users", "age", FindOp::Eq(Value::Int(40)))
                .unwrap()
                .is_empty()
        );
        tx.commit().unwrap();
        drop(tx);
        assert!(
            store
                .collection("users")
                .unwrap()
                .get("u1")
                .unwrap()
                .is_none()
        );

        // delete A, put A
        let mut tx = store.transaction().unwrap();
        tx.delete("users", "u1").unwrap();
        tx.update("users", "u1", &user("x", 50, true)).unwrap();
        assert_eq!(
            tx.get("users", "u1").unwrap().unwrap().field("age"),
            Some(&Value::Int(50))
        );
        assert_eq!(
            ids(tx.find("users", "age", FindOp::Eq(Value::Int(50))).unwrap()),
            vec!["u1"]
        );
        tx.commit().unwrap();
        drop(tx);

        // update A, update A, delete A, put A
        let mut tx = store.transaction().unwrap();
        tx.update("users", "u1", &user("x", 51, true)).unwrap();
        tx.update("users", "u1", &user("x", 52, true)).unwrap();
        tx.delete("users", "u1").unwrap();
        tx.update("users", "u1", &user("x", 53, true)).unwrap();
        assert_eq!(
            tx.get("users", "u1").unwrap().unwrap().field("age"),
            Some(&Value::Int(53))
        );
        assert_eq!(
            ids(tx.find("users", "age", FindOp::Eq(Value::Int(53))).unwrap()),
            vec!["u1"]
        );
        assert!(
            tx.find("users", "age", FindOp::Eq(Value::Int(52)))
                .unwrap()
                .is_empty()
        );
        tx.commit().unwrap();
        drop(tx);

        let mut col = store.collection("users").unwrap();
        assert_eq!(
            col.get("u1").unwrap().unwrap().field("age"),
            Some(&Value::Int(53))
        );
        assert_eq!(
            ids(col.find("age", FindOp::Eq(Value::Int(53))).unwrap()),
            vec!["u1"]
        );
    }

    #[test]
    fn crash_before_commit_discards_tx() {
        let dir = tempfile::tempdir().unwrap();
        let e = user("Tobias", 31, true);
        {
            let mut store = EntityStore::open(dir.path()).unwrap();
            let mut tx = store.transaction().unwrap();
            tx.update("users", "u1", &e).unwrap();
            // Kein commit → wie ein Crash / Abbruch. Pending wird nie angewandt.
            drop(tx);
        }
        let mut store = EntityStore::open(dir.path()).unwrap();
        assert!(
            store
                .collection("users")
                .unwrap()
                .get("u1")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn crash_after_commit_survives() {
        let dir = tempfile::tempdir().unwrap();
        let e = user("Tobias", 31, true);
        {
            let mut store = EntityStore::open(dir.path()).unwrap();
            let mut tx = store.transaction().unwrap();
            tx.update("users", "u1", &e).unwrap();
            tx.commit().unwrap();
            drop(tx);
        }
        let mut store = EntityStore::open(dir.path()).unwrap();
        let got = store
            .collection("users")
            .unwrap()
            .get("u1")
            .unwrap()
            .expect("exists");
        assert_eq!(got.field("name"), Some(&Value::String("Tobias".into())));
    }

    /// Führt eine Update-Sequenz aus: anlegen, Feld hinzufügen+entfernen+ändern,
    /// Feld entfernen. Bei `reopen_each = true` wird vor jedem Schritt neu
    /// geöffnet (→ Feld-Satz-Hint ist nie gesetzt, reiner Cold-Scan-Pfad).
    /// Dient als semantisches Oracle für den warmen Pfad (5a+2).
    fn run_update_seq(dir: &std::path::Path, reopen_each: bool) -> Entity {
        let mut store = EntityStore::open(dir).unwrap();
        let mut e = user("Tobias", 31, true);
        store.collection("users").unwrap().put("u1", &e).unwrap();
        if reopen_each {
            store.close().unwrap();
            store = EntityStore::open(dir).unwrap();
        }
        // add + remove + change in einem Update
        e.insert("city", Value::String("Berlin".into()));
        e.fields.retain(|(n, _)| n != "active");
        e.fields.iter_mut().find(|(n, _)| n == "age").unwrap().1 = Value::Int(32);
        store.collection("users").unwrap().put("u1", &e).unwrap();
        if reopen_each {
            store.close().unwrap();
            store = EntityStore::open(dir).unwrap();
        }
        // city wieder entfernen (Stale-Removal über vorherigen Zustand)
        e.fields.retain(|(n, _)| n != "city");
        store.collection("users").unwrap().put("u1", &e).unwrap();
        let got = store
            .collection("users")
            .unwrap()
            .get("u1")
            .unwrap()
            .expect("exists");
        store.close().unwrap();
        got
    }

    #[test]
    fn warm_path_matches_cold_path_semantics() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let warm = run_update_seq(d1.path(), false);
        let cold = run_update_seq(d2.path(), true);
        assert_eq!(warm, cold);
        assert_eq!(warm.field("age"), Some(&Value::Int(32)));
        assert!(warm.field("active").is_none());
        assert!(warm.field("city").is_none());
        assert_eq!(warm.field("name"), Some(&Value::String("Tobias".into())));
    }

    #[test]
    fn warm_update_adds_and_removes_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let mut col = store.collection("users").unwrap();
        let mut e = user("Tobias", 31, true);
        col.put("u1", &e).unwrap();
        // add + remove + change (warm: gleiche Session, Hint vorhanden)
        e.insert("city", Value::String("Berlin".into()));
        e.fields.retain(|(n, _)| n != "active");
        e.fields.iter_mut().find(|(n, _)| n == "age").unwrap().1 = Value::Int(32);
        col.put("u1", &e).unwrap();
        let got = col.get("u1").unwrap().expect("exists");
        assert_eq!(got.field("age"), Some(&Value::Int(32)));
        assert_eq!(got.field("city"), Some(&Value::String("Berlin".into())));
        assert!(
            got.field("active").is_none(),
            "stale field must be removed (warm)"
        );
        assert_eq!(got.field("name"), Some(&Value::String("Tobias".into())));
    }

    #[test]
    fn index_diff_on_warm_update_change_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        store
            .collection("users")
            .unwrap()
            .create_index("age")
            .unwrap();
        let e1 = user("Tobias", 31, true);
        store.collection("users").unwrap().put("u1", &e1).unwrap();
        assert_eq!(
            ids(store
                .collection("users")
                .unwrap()
                .find("age", FindOp::Eq(Value::Int(31)))
                .unwrap()),
            vec!["u1"]
        );
        // Warm-Update: age ändern → alter Index-Eintrag muss weg.
        let mut e2 = e1.clone();
        e2.fields.iter_mut().find(|(n, _)| n == "age").unwrap().1 = Value::Int(32);
        store.collection("users").unwrap().put("u1", &e2).unwrap();
        assert!(
            store
                .collection("users")
                .unwrap()
                .find("age", FindOp::Eq(Value::Int(31)))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(store
                .collection("users")
                .unwrap()
                .find("age", FindOp::Eq(Value::Int(32)))
                .unwrap()),
            vec!["u1"]
        );
    }

    #[test]
    fn update_across_flush_removes_stale_from_sstable() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let mut e = user("Tobias", 31, true);
        store.collection("users").unwrap().put("u1", &e).unwrap();
        store.flush().unwrap(); // Feld liegt jetzt in einer SSTable
        // Warm-Update entfernt ein Feld, das bereits geflusht ist.
        e.fields.retain(|(n, _)| n != "active");
        store.collection("users").unwrap().put("u1", &e).unwrap();
        let got = store
            .collection("users")
            .unwrap()
            .get("u1")
            .unwrap()
            .expect("exists");
        assert!(
            got.field("active").is_none(),
            "stale field in SSTable must be removed"
        );
    }

    #[test]
    fn cold_start_update_removes_stale_field() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = EntityStore::open(dir.path()).unwrap();
            let e = user("Tobias", 31, true);
            store.collection("users").unwrap().put("u1", &e).unwrap();
            store.flush().unwrap();
            store.close().unwrap();
        }
        // Reopen → Hint leer → Cold-Scan muss Stale-Feld trotzdem entfernen.
        let mut store = EntityStore::open(dir.path()).unwrap();
        let mut e = user("Tobias", 31, true);
        e.fields.retain(|(n, _)| n != "active");
        store.collection("users").unwrap().put("u1", &e).unwrap();
        let got = store
            .collection("users")
            .unwrap()
            .get("u1")
            .unwrap()
            .expect("exists");
        assert!(
            got.field("active").is_none(),
            "cold start must remove stale field"
        );
    }

    #[test]
    fn tx_commit_invalidates_field_hint() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = EntityStore::open(dir.path()).unwrap();
        let e = user("Tobias", 31, true);
        store.collection("users").unwrap().put("u1", &e).unwrap();
        // Transaktion ändert die Entity (geht an Store-Hint vorbei) → Hint muss
        // danach invalidiert sein, sonst wäre die folgende Non-Tx-Put unsicher.
        let mut e2 = e.clone();
        e2.fields.retain(|(n, _)| n != "active");
        store
            .transaction_with(|tx| {
                tx.update("users", "u1", &e2)?;
                Ok(())
            })
            .unwrap();
        // Entferntes Feld darf nicht wieder auftauchen.
        let got = store
            .collection("users")
            .unwrap()
            .get("u1")
            .unwrap()
            .expect("exists");
        assert!(got.field("active").is_none());
    }
}
