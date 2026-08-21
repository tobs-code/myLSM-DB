//! Korrektheits-Gates für den Old-Value-Cache (v0.7-write-cache, Variante A).
//!
//! Der Cache ist ein Prototyp, nur mit `--features bench-diag` einkompiliert.
//! Kerninvariante: Der Cache darf **keine neue Wahrheitsquelle** sein — ein
//! Cache-Hit (alter Indexwert aus dem Speicher) muss exakt dasselbe Ergebnis
//! liefern wie der bisherige Cold-Pfad (Disk-Point-Lookup bzw. Range-Scan).
//!
//! Zentrales Oracle: dieselbe logische Sequenz wird einmal über den **warmen**
//! Pfad (`put_entity`, nutzt field_hint + value_cache) und einmal über den
//! **kalten** Pfad (`Transaction::update`+`commit`, immer Cold-Scan, keine
//! Caches) ausgeführt. Nach jedem Schritt müssen die resultierenden Reads und
//! `find()`-Ergebnisse identisch sein. Jede Divergenz zeigt einen falschen
//! Index-Diff (→ kaputte Index-Einträge).
//!
//! Zusätzlich: Reopen (Cache leer → Cold-Fallback), Flush/Compaction zwischen
//! Updates, Delete→Reinsert, Index add/drop, mehrere indexierte Felder,
//! Transaktion+Rollback/Commit.

#![cfg(feature = "bench-diag")]

use my_lsm_db::Options;
use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::index::FindOp;

fn mk(age: i64, score: i64) -> Entity {
    let mut e = Entity::new();
    e.insert("name", Value::String("n".into()));
    e.insert("age", Value::Int(age));
    e.insert("score", Value::Int(score));
    e.insert("active", Value::Bool(true));
    e
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Zustand-Fingerabdruck: get_entity aller IDs + `find`-Ergebnisse über die
/// indexierten Felder. Vergleichbar über zwei Stores hinweg (Oracle).
fn fingerprint(store: &mut EntityStore, ids: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for id in ids {
        let ent = store.collection("users").unwrap().get(id).unwrap();
        out.push(format!("get({})={:?}", id, ent));
    }
    for field in ["age", "score"] {
        for probe in [20i64, 25, 30, 31, 33, 40, 50, 70, 72, 85, 88, 90, 95] {
            let r = store
                .collection("users")
                .unwrap()
                .find(field, FindOp::Eq(Value::Int(probe)))
                .unwrap();
            out.push(format!("find({}=={})={:?}", field, probe, sorted(r)));
        }
        let between = store
            .collection("users")
            .unwrap()
            .find(field, FindOp::Between(Value::Int(20), Value::Int(100)))
            .unwrap();
        out.push(format!("find({} in 20..100)={:?}", field, sorted(between)));
    }
    out
}

/// Wendet `w` auf den warmen und `c` auf den kalten Store an und prüft, dass
/// sich die Zustände nicht unterscheiden.
fn both<W: FnOnce(&mut EntityStore), C: FnOnce(&mut EntityStore)>(
    warm: &mut EntityStore,
    cold: &mut EntityStore,
    ids: &[&str],
    w: W,
    c: C,
) {
    w(warm);
    c(cold);
    let (fw, fc) = (fingerprint(warm, ids), fingerprint(cold, ids));
    assert_eq!(fw, fc, "Warm-Pfad (Old-Value-Cache) wich vom Cold-Pfad ab");
}

fn put(s: &mut EntityStore, id: &str, ent: &Entity) {
    s.collection("users").unwrap().put(id, ent).unwrap();
}
fn tx_put(s: &mut EntityStore, id: &str, ent: &Entity) {
    let mut t = s.transaction().unwrap();
    t.update("users", id, ent).unwrap();
    t.commit().unwrap();
}
fn del(s: &mut EntityStore, id: &str) {
    s.collection("users").unwrap().delete(id).unwrap();
}
fn tx_del(s: &mut EntityStore, id: &str) {
    let mut t = s.transaction().unwrap();
    t.delete("users", id).unwrap();
    t.commit().unwrap();
}

#[test]
fn warm_put_matches_cold_transaction_oracle() {
    let wdir = tempfile::tempdir().unwrap();
    let cdir = tempfile::tempdir().unwrap();
    let mut warm = EntityStore::open(wdir.path()).unwrap();
    let mut cold = EntityStore::open(cdir.path()).unwrap();
    for s in [&mut warm, &mut cold] {
        s.collection("users").unwrap().create_index("age").unwrap();
        s.collection("users")
            .unwrap()
            .create_index("score")
            .unwrap();
    }
    let ids = ["alice", "bob"];

    // 1) initial
    both(
        &mut warm,
        &mut cold,
        &ids,
        |s| {
            put(s, "alice", &mk(30, 90));
            put(s, "bob", &mk(25, 70));
        },
        |s| {
            tx_put(s, "alice", &mk(30, 90));
            tx_put(s, "bob", &mk(25, 70));
        },
    );
    // 2) beide Felder ändern (Cache-Hit auf age+score)
    both(
        &mut warm,
        &mut cold,
        &ids,
        |s| {
            put(s, "alice", &mk(31, 85));
        },
        |s| {
            tx_put(s, "alice", &mk(31, 85));
        },
    );
    // 3) nur score ändern
    both(
        &mut warm,
        &mut cold,
        &ids,
        |s| {
            put(s, "bob", &mk(25, 72));
        },
        |s| {
            tx_put(s, "bob", &mk(25, 72));
        },
    );
    // 4) Feld entfernen (Stale-Removal + Index-Delete)
    let mut e = mk(31, 85);
    e.fields.retain(|(n, _)| n != "score");
    both(
        &mut warm,
        &mut cold,
        &ids,
        |s| {
            put(s, "alice", &e);
        },
        |s| {
            tx_put(s, "alice", &e);
        },
    );
    // 5) Feld wieder hinzufügen (neuer Index-Eintrag)
    both(
        &mut warm,
        &mut cold,
        &ids,
        |s| {
            put(s, "alice", &mk(40, 50));
        },
        |s| {
            tx_put(s, "alice", &mk(40, 50));
        },
    );
    // 6) Delete beider
    both(
        &mut warm,
        &mut cold,
        &ids,
        |s| {
            del(s, "alice");
            del(s, "bob");
        },
        |s| {
            tx_del(s, "alice");
            tx_del(s, "bob");
        },
    );
    // 7) Delete → Reinsert
    both(
        &mut warm,
        &mut cold,
        &ids,
        |s| {
            put(s, "alice", &mk(33, 88));
        },
        |s| {
            tx_put(s, "alice", &mk(33, 88));
        },
    );
    // 8) weitere Entity interleaved
    both(
        &mut warm,
        &mut cold,
        &ids,
        |s| {
            put(s, "bob", &mk(25, 70));
        },
        |s| {
            tx_put(s, "bob", &mk(25, 70));
        },
    );
}

#[test]
fn reopen_clears_cache_then_cold_fallback_is_correct() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = EntityStore::open(dir.path()).unwrap();
        store
            .collection("users")
            .unwrap()
            .create_index("age")
            .unwrap();
        put(&mut store, "alice", &mk(30, 90));
        // Reopen erzwingt Cold-Fallback (Cache ist leer).
    }
    let mut store = EntityStore::open(dir.path()).unwrap();
    put(&mut store, "alice", &mk(31, 85));
    let got = store
        .collection("users")
        .unwrap()
        .find("age", FindOp::Eq(Value::Int(31)))
        .unwrap();
    assert_eq!(sorted(got), vec!["alice"]);
    // Alter Wert darf nicht mehr im Index stehen.
    let old = store
        .collection("users")
        .unwrap()
        .find("age", FindOp::Eq(Value::Int(30)))
        .unwrap();
    assert_eq!(sorted(old), Vec::<String>::new());
    assert!(
        store
            .collection("users")
            .unwrap()
            .get("alice")
            .unwrap()
            .is_some()
    );
}

#[test]
fn updates_across_flush_and_compaction_stay_consistent() {
    // Kleine MemTable → häufige Flushes; niedriger Compaction-Schwellwert.
    let opts = Options {
        memtable_limit: 4096,
        l0_compact_threshold: 2,
    };
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open_with(dir.path(), opts).unwrap();
    store
        .collection("users")
        .unwrap()
        .create_index("age")
        .unwrap();
    store
        .collection("users")
        .unwrap()
        .create_index("score")
        .unwrap();

    // Genug Writes, um Flushes + Compaction über die Updates hinweg zu erzwingen.
    for i in 0..400 {
        let id = format!("u{:03}", i % 50);
        let age = 30 + (i as i64 % 10);
        let score = 100 - (i as i64 % 20);
        if i % 3 == 0 {
            // mal nur ein Feld aktualisieren
            let mut e = mk(age, score);
            e.fields.retain(|(n, _)| n != "score");
            store.collection("users").unwrap().put(&id, &e).unwrap();
        } else {
            store
                .collection("users")
                .unwrap()
                .put(&id, &mk(age, score))
                .unwrap();
        }
        if i % 3 == 2 {
            // Entität entfernen, die später reinsertiert wird
            store.collection("users").unwrap().delete(&id).unwrap();
        }
    }

    // Der Index-Stand (via Cache-getriebene Diffs) muss exakt dem Datenstand
    // entsprechen. Jede Abweichung heißt: falscher Old-Value → falscher
    // Index-Diff → driftende Index-Einträge.
    let via_index = sorted(
        store
            .collection("users")
            .unwrap()
            .find("age", FindOp::Between(Value::Int(20), Value::Int(100)))
            .unwrap(),
    );
    let via_scan: Vec<String> = store
        .scan_collection("users")
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        sorted(via_scan),
        via_index,
        "Index driftete vom Datenstand ab"
    );
}

#[test]
fn index_add_and_remove_are_cache_safe() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();
    col.put("alice", &mk(30, 90)).unwrap();
    // age noch nicht indexiert → Put ohne Index-Diff
    col.create_index("age").unwrap();
    // Update auf jetzt-indexiertem Feld: Cache hatte den Wert nicht → Miss-Fallback.
    col.put("alice", &mk(31, 90)).unwrap();
    assert_eq!(
        sorted(col.find("age", FindOp::Eq(Value::Int(31))).unwrap()),
        vec!["alice"]
    );
    assert_eq!(
        sorted(col.find("age", FindOp::Eq(Value::Int(30))).unwrap()),
        Vec::<String>::new()
    );
    // Index entfernen → keine Index-Nutzung, Werte bleiben.
    col.drop_index("age").unwrap();
    assert!(col.get("alice").unwrap().is_some());
    // Index wieder anlegen und weiter updaten (Cache enthält alten Wert wieder).
    col.create_index("age").unwrap();
    col.put("alice", &mk(33, 90)).unwrap();
    assert_eq!(
        sorted(col.find("age", FindOp::Eq(Value::Int(33))).unwrap()),
        vec!["alice"]
    );
}

#[test]
fn multiple_indexed_fields_update_together() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("users").unwrap();
    col.create_index("age").unwrap();
    col.create_index("score").unwrap();
    col.put("alice", &mk(30, 90)).unwrap();
    // Beide Felder gleichzeitig ändern (zwei Cache-Hits in einem Put).
    col.put("alice", &mk(31, 85)).unwrap();
    assert_eq!(
        sorted(col.find("age", FindOp::Eq(Value::Int(31))).unwrap()),
        vec!["alice"]
    );
    assert_eq!(
        sorted(col.find("score", FindOp::Eq(Value::Int(85))).unwrap()),
        vec!["alice"]
    );
    assert_eq!(
        sorted(col.find("age", FindOp::Eq(Value::Int(30))).unwrap()),
        Vec::<String>::new()
    );
    assert_eq!(
        sorted(col.find("score", FindOp::Eq(Value::Int(90))).unwrap()),
        Vec::<String>::new()
    );
}

#[test]
fn transaction_rollback_and_commit_invalidate_cache() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    store
        .collection("users")
        .unwrap()
        .create_index("age")
        .unwrap();
    store
        .collection("users")
        .unwrap()
        .put("alice", &mk(30, 90))
        .unwrap();

    // Rollback: verändert nichts → Cache bleibt valide.
    {
        let mut t = store.transaction().unwrap();
        t.update("users", "alice", &mk(99, 1)).unwrap();
        t.abort().unwrap();
    }
    assert_eq!(
        sorted(
            store
                .collection("users")
                .unwrap()
                .find("age", FindOp::Eq(Value::Int(30)))
                .unwrap()
        ),
        vec!["alice"]
    );

    // Commit: invalidiert den Cache → nächster Warm-Put muss korrekt cold sein.
    {
        let mut t = store.transaction().unwrap();
        t.update("users", "alice", &mk(40, 50)).unwrap();
        t.commit().unwrap();
    }
    assert_eq!(
        sorted(
            store
                .collection("users")
                .unwrap()
                .find("age", FindOp::Eq(Value::Int(40)))
                .unwrap()
        ),
        vec!["alice"]
    );
    assert_eq!(
        sorted(
            store
                .collection("users")
                .unwrap()
                .find("age", FindOp::Eq(Value::Int(30)))
                .unwrap()
        ),
        Vec::<String>::new()
    );
    // Danach wieder über den normalen Warm-Pfad aktualisieren.
    store
        .collection("users")
        .unwrap()
        .put("alice", &mk(41, 51))
        .unwrap();
    assert_eq!(
        sorted(
            store
                .collection("users")
                .unwrap()
                .find("age", FindOp::Eq(Value::Int(41)))
                .unwrap()
        ),
        vec!["alice"]
    );
    assert_eq!(
        sorted(
            store
                .collection("users")
                .unwrap()
                .find("age", FindOp::Eq(Value::Int(40)))
                .unwrap()
        ),
        Vec::<String>::new()
    );
}
