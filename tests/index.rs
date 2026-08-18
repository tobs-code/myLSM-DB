//! v0.3-Oracle-Test: Der Index muss nach zufälligen Mutationen + Flush +
//! Compaction + Restart exakt dem Full-Scan der Entity-Daten entsprechen.
//!
//! Der Full-Scan (`scan_collection`) ist der Oracle. `find()` darf nie von
//! ihm abweichen (kein False Negative; False Positives werden verworfen).

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::index::FindOp;
use my_lsm_db::ordering::value_cmp;
use std::cmp::Ordering;

/// Deterministischer PRNG.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn entity(age: i64, tag: u64) -> Entity {
    let mut e = Entity::new();
    e.insert("age", Value::Int(age));
    e.insert("name", Value::String(format!("person-{tag}")));
    e
}

/// Erwartete Entity-IDs für eine Abfrage, berechnet aus dem Full-Scan-Oracle.
fn oracle_matches(op: &FindOp, field_val: Option<&Value>) -> bool {
    use my_lsm_db::index::Bound;
    let (lo, hi) = match op {
        FindOp::Eq(v) => (Bound::Inclusive(v.clone()), Bound::Inclusive(v.clone())),
        FindOp::Gt(v) => (Bound::Exclusive(v.clone()), Bound::Unbounded),
        FindOp::Gte(v) => (Bound::Inclusive(v.clone()), Bound::Unbounded),
        FindOp::Lt(v) => (Bound::Unbounded, Bound::Exclusive(v.clone())),
        FindOp::Lte(v) => (Bound::Unbounded, Bound::Inclusive(v.clone())),
        FindOp::Between(l, h) => (Bound::Inclusive(l.clone()), Bound::Inclusive(h.clone())),
    };
    let v = match field_val {
        Some(v) => v,
        None => return false,
    };
    let lo_ok = match lo {
        Bound::Unbounded => true,
        Bound::Inclusive(x) => value_cmp(v, &x) != Ordering::Less,
        Bound::Exclusive(x) => value_cmp(v, &x) == Ordering::Greater,
    };
    let hi_ok = match hi {
        Bound::Unbounded => true,
        Bound::Exclusive(x) => value_cmp(v, &x) == Ordering::Less,
        Bound::Inclusive(x) => value_cmp(v, &x) != Ordering::Greater,
    };
    lo_ok && hi_ok
}

fn expected_ids(all: &[(String, Entity)], op: &FindOp) -> Vec<String> {
    let mut out: Vec<String> = all
        .iter()
        .filter(|(_, e)| oracle_matches(op, e.field("age")))
        .map(|(id, _)| id.clone())
        .collect();
    out.sort();
    out
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[test]
fn oracle_index_matches_full_scan_after_random_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = Rng(0x0BAD_C0DE);

    let mut store = EntityStore::open(dir.path()).unwrap();
    {
        let mut col = store.collection("users").unwrap();
        col.create_index("age").unwrap();

        // 300 Entities anlegen.
        for i in 0..300u64 {
            let age = (rng.below(100) as i64) - 50;
            col.put(&format!("u{i}"), &entity(age, i)).unwrap();
        }
    }
    store.flush().unwrap();

    // 8000 zufällige Mutationen (Update / Delete / Re-Insert), periodisch Flush.
    for step in 0..8000u64 {
        let id = format!("u{}", rng.below(400)); // auch nie angelegte IDs
        let action = rng.below(3);
        let mut col = store.collection("users").unwrap();
        match action {
            0 => {
                // Update/Insert mit neuem Alter.
                let age = (rng.below(120) as i64) - 60;
                col.put(&id, &entity(age, step)).unwrap();
            }
            1 => {
                col.delete(&id).unwrap();
            }
            _ => {
                // Re-Insert.
                let age = (rng.below(100) as i64) - 50;
                col.put(&id, &entity(age, step)).unwrap();
            }
        }
        drop(col);
        if step % 1000 == 999 {
            store.flush().unwrap();
        }
    }
    store.flush().unwrap();
    store.close().unwrap();

    // Restart: Index-Definition + Daten müssen überleben.
    let mut store = EntityStore::open(dir.path()).unwrap();
    let all = store.scan_collection("users").unwrap();
    assert!(!all.is_empty(), "expected surviving entities");

    // Eine Reihe von Abfragen gegen den Oracle prüfen.
    let mut col = store.collection("users").unwrap();
    for probe in [-60, -50, -1, 0, 1, 30, 31, 59, 999] {
        let op = FindOp::Eq(Value::Int(probe));
        let got = sorted(col.find("age", op.clone()).unwrap());
        let exp = expected_ids(&all, &op);
        assert_eq!(got, exp, "Eq({probe})");
    }
    for (lo, hi) in [(-60i64, 0i64), (0, 60), (-10, 10), (-1, 1)] {
        let op = FindOp::Between(Value::Int(lo), Value::Int(hi));
        let got = sorted(col.find("age", op.clone()).unwrap());
        let exp = expected_ids(&all, &op);
        assert_eq!(got, exp, "Between({lo},{hi})");
    }
    for v in [-1i64, 0, 31, 100] {
        for op in [
            FindOp::Gt(Value::Int(v)),
            FindOp::Gte(Value::Int(v)),
            FindOp::Lt(Value::Int(v)),
            FindOp::Lte(Value::Int(v)),
        ] {
            let got = sorted(col.find("age", op.clone()).unwrap());
            let exp = expected_ids(&all, &op);
            assert_eq!(got, exp, "range {v:?}");
        }
    }
}
