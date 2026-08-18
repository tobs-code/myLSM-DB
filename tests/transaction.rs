//! Zufalls-basierter Modell-Tester für v0.4-Transaktionen.
//!
//! Mischt `begin / update / delete / commit / abort / crash / restart / index /
//! entity` durcheinander und hält gegen ein In-Memory-Referenzmodell der
//! committeten Welt. Nach jedem Commit und nach jedem Restart (Reopen) wird der
//! tatsächliche Store gegen das Modell geprüft (get, scan, find).

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::error::Result;
use my_lsm_db::index::FindOp;

/// Referenzmodell: entity_id -> (name, age) der committeten Welt.
type Model = std::collections::BTreeMap<u32, (String, i64)>;
/// Transaktions-lokales Modell: id -> Some(entity) / None (gelöscht). Nicht
/// enthaltene IDs fallen auf das committete Modell zurück.
type TxModel = std::collections::BTreeMap<u32, Option<(String, i64)>>;

struct Rng {
    seed: u64,
}

impl Rng {
    fn next(&mut self) -> u64 {
        self.seed = self
            .seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.seed >> 33
    }
}

fn entity(name: &str, age: i64) -> Entity {
    let mut e = Entity::new();
    e.insert("name", Value::String(name.to_string()));
    e.insert("age", Value::Int(age));
    e
}

fn effective(model: &Model, txm: &TxModel, id: u32) -> Option<(String, i64)> {
    if let Some(v) = txm.get(&id) {
        return v.clone();
    }
    model.get(&id).cloned()
}

fn ids(v: Vec<String>) -> Vec<String> {
    let mut v = v;
    v.sort();
    v
}

/// Prüft, dass der Store vollständig dem committeten Modell entspricht.
fn verify_store(store: &mut EntityStore, model: &Model) {
    let all = store.scan_collection("users").unwrap();
    assert_eq!(all.len(), model.len(), "scan count mismatch");
    for (id, (name, age)) in model {
        let got = store
            .collection("users")
            .unwrap()
            .get(&id.to_string())
            .unwrap()
            .expect("entity must exist");
        assert_eq!(got.field("name"), Some(&Value::String(name.clone())));
        assert_eq!(got.field("age"), Some(&Value::Int(*age)));
    }
    // find über den age-Index muss dem Modell entsprechen.
    let mut by_age: std::collections::BTreeMap<i64, Vec<String>> = Default::default();
    for (id, (_, age)) in model {
        by_age.entry(*age).or_default().push(id.to_string());
    }
    for (age, expected) in &by_age {
        let mut expected = expected.clone();
        expected.sort();
        let got = ids(store
            .collection("users")
            .unwrap()
            .find("age", FindOp::Eq(Value::Int(*age)))
            .unwrap());
        assert_eq!(got, expected, "index mismatch at age {age}");
    }
}

/// Führt eine zufällige Transaktion zu Ende aus (in eigenem Scope, sodass der
/// Store-Borrow beim Return wieder freigegeben wird). Liefert zurück, ob sie
/// committet wurde, samt Transaktionsmodell.
fn run_random_tx(store: &mut EntityStore, model: &Model, rng: &mut Rng) -> Result<(bool, TxModel)> {
    let mut t = store.transaction()?;
    let mut txm: TxModel = TxModel::new();
    let n = 1 + (rng.next() % 5) as usize;
    for _ in 0..n {
        if rng.next() % 10 < 8 {
            let id = (rng.next() % 6) as u32;
            let age = (rng.next() % 100) as i64;
            let name = format!("u{id}");
            t.update("users", &id.to_string(), &entity(&name, age))
                .unwrap();
            txm.insert(id, Some((name, age)));
        } else {
            let id = (rng.next() % 6) as u32;
            t.delete("users", &id.to_string()).unwrap();
            txm.insert(id, None);
        }
        // Read-your-own-writes-Check gegen das Transaktionsmodell.
        let id = txm.iter().next().map(|(k, _)| *k).unwrap_or(0);
        let want = effective(model, &txm, id);
        let got = t.get("users", &id.to_string()).unwrap();
        match want {
            Some((name, age)) => {
                let got = got.expect("entity visible");
                assert_eq!(got.field("age"), Some(&Value::Int(age)));
                assert_eq!(got.field("name"), Some(&Value::String(name)));
            }
            None => assert!(got.is_none(), "entity must be absent"),
        }
    }
    let commit = rng.next().is_multiple_of(3);
    if commit {
        t.commit().unwrap();
    } else {
        t.abort().unwrap();
    }
    Ok((commit, txm))
}

#[test]
fn random_transaction_model_oracle() {
    let mut rng = Rng {
        seed: 0x9E3779B97F4A7C15,
    };
    const IDS: u64 = 6;

    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    store
        .collection("users")
        .unwrap()
        .create_index("age")
        .unwrap();

    let mut model: Model = Model::new();

    for _ in 0..3000 {
        let r = rng.next() % 10;
        if r < 3 {
            // Zufällige Transaktion (commit/abort) → ggf. Modell anwenden.
            let (committed, txm) = run_random_tx(&mut store, &model, &mut rng).unwrap();
            if committed {
                for (id, val) in &txm {
                    match val {
                        Some((name, age)) => {
                            model.insert(*id, (name.clone(), *age));
                        }
                        None => {
                            model.remove(id);
                        }
                    }
                }
            }
            verify_store(&mut store, &model);
        } else if r < 6 {
            // Direkter (nicht-transaktionaler) Put.
            let id = (rng.next() % IDS) as u32;
            let age = (rng.next() % 100) as i64;
            let name = format!("u{id}");
            store
                .collection("users")
                .unwrap()
                .put(&id.to_string(), &entity(&name, age))
                .unwrap();
            model.insert(id, (name, age));
        } else if r < 8 {
            // Crash / Restart: committete Welt muss das Modell reproduzieren.
            drop(store);
            store = EntityStore::open(dir.path()).unwrap();
            store
                .collection("users")
                .unwrap()
                .create_index("age")
                .unwrap();
            verify_store(&mut store, &model);
        }
        // sonst: nichts tun
    }

    // Abschluss: sauber schließen + Reopen.
    drop(store);
    store = EntityStore::open(dir.path()).unwrap();
    store
        .collection("users")
        .unwrap()
        .create_index("age")
        .unwrap();
    verify_store(&mut store, &model);
    let _: Result<()> = store.close().map(|_| ());
}
