//! Phase K — Real-World Usage Harness auf `911c4c7` (kein Feature-Branch, kein
//! Umbau). Eine kleine lokale Aufgaben-/Projekt-Datenbank, die die vorhandene
//! Produktfläche tatsächlich benutzt:
//!
//! - CRUD über `EntityStore` / `CollectionHandle`
//! - Composite Indexes (`tasks(project_id,status)`, `(project_id,assignee)`,
//!   `(status,priority)`, `events(task_id,created_at)`)
//! - CAS / Partial Update (`Expected` + `Patch`)
//! - Queries (Filter, ORDER BY + LIMIT, Aggregation, Projektion)
//! - Transaktion (todo→doing + Assignee + Event + Commit) inkl. Reopen-Check
//! - Backup/Restore mit semantischem Vorher/Nachher-Vergleich
//!
//! Ziel ist nicht „wie schnell ist die DB“, sondern: funktionieren die
//! Workflows, gibt es API-Lücken / überraschende Semantik / reproduzierbare
//! Fehler, und wo beginnt sie real zu schwächeln. Ein Scale-Knob
//! (`PHASE_K_BIG=1`) treibt die Task-Menge hoch, um echte Bottlenecks zu
//! provozieren — standardmäßig bleibt die Menge klein und der Test schnell.

use std::time::Instant;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore, Expected, Patch};
use my_lsm_db::error::Result;
use my_lsm_db::index::Bound;
use my_lsm_db::query::expression::eval;
use my_lsm_db::query::{Aggregate, Predicate, SortDir, eq, ge};

const PROJECTS: &[&str] = &["p1", "p2", "p3"];
const STATUSES: &[&str] = &["todo", "doing", "done"];
const ASSIGNEES: &[&str] = &["alice", "bob", "tobias", "carol"];

fn tasks_per_project() -> usize {
    if let Ok(v) = std::env::var("PHASE_K_N") {
        v.parse().unwrap_or(80)
    } else if std::env::var("PHASE_K_BIG").is_ok() {
        200_000
    } else {
        80
    }
}

fn e(fields: &[(&str, Value)]) -> Entity {
    let mut x = Entity::new();
    for (n, v) in fields {
        x.insert(*n, v.clone());
    }
    x
}

fn seed(store: &mut EntityStore) -> Result<()> {
    // Schema + Composite Indexes anlegen.
    {
        let mut col = store.collection("projects").unwrap();
        col.create_index("status").unwrap();
        col.create_index("owner").unwrap();
    }
    {
        let mut col = store.collection("tasks").unwrap();
        col.create_composite_index(&["project_id", "status"])
            .unwrap();
        col.create_composite_index(&["project_id", "assignee"])
            .unwrap();
        col.create_composite_index(&["status", "priority"]).unwrap();
    }
    {
        let mut col = store.collection("events").unwrap();
        col.create_composite_index(&["task_id", "created_at"])
            .unwrap();
    }

    for p in PROJECTS {
        store
            .collection("projects")
            .unwrap()
            .put(
                p,
                &e(&[
                    ("name", Value::String(format!("Project {p}"))),
                    ("status", Value::String("active".into())),
                    ("owner", Value::String("tobias".into())),
                    ("created_at", Value::Int(1)),
                ]),
            )
            .unwrap();
    }

    let n = tasks_per_project();
    for p in PROJECTS {
        for i in 0..n {
            let id = format!("t-{p}-{i}");
            let status = STATUSES[i % STATUSES.len()];
            let priority = (i % 5) as i64 + 1;
            let assignee = ASSIGNEES[(i / 2) % ASSIGNEES.len()];
            store
                .collection("tasks")
                .unwrap()
                .put(
                    &id,
                    &e(&[
                        ("project_id", Value::String(p.to_string())),
                        ("title", Value::String(format!("Task {i} of {p}"))),
                        ("status", Value::String(status.into())),
                        ("priority", Value::Int(priority)),
                        ("assignee", Value::String(assignee.into())),
                        ("created_at", Value::Int(i as i64)),
                        ("due_at", Value::Int((i as i64) + 10)),
                        ("estimate", Value::Int(((i % 8) as i64) + 1)),
                    ]),
                )
                .unwrap();
            // 1–3 Events pro Task.
            let ne = 1 + (i % 3);
            for k in 0..ne {
                let eid = format!("e-{id}-{k}");
                store
                    .collection("events")
                    .unwrap()
                    .put(
                        &eid,
                        &e(&[
                            ("task_id", Value::String(id.clone())),
                            ("kind", Value::String("update".into())),
                            ("actor", Value::String(assignee.into())),
                            ("created_at", Value::Int((i as i64) * 10 + k as i64)),
                            ("payload", Value::String(format!("step {k}"))),
                        ]),
                    )
                    .unwrap();
            }
        }
    }
    Ok(())
}

/// Naive Referenz (Full-Scan + `eval`), analog zum Oracle-Test.
fn naive(store: &mut EntityStore, coll: &str, pred: &Predicate) -> Vec<String> {
    let all = store.scan_collection(coll).unwrap();
    let mut ids: Vec<String> = all
        .into_iter()
        .filter(|(_, ent)| eval(ent, pred).unwrap_or(false))
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    ids
}

fn run_ids(store: &mut EntityStore, coll: &str, pred: Predicate) -> Vec<String> {
    let mut b = store.query(coll).unwrap();
    b = b.filter(pred);
    let rows = store.execute_query(b).unwrap();
    let mut ids: Vec<String> = rows.into_iter().map(|(id, _)| id).collect();
    ids.sort();
    ids
}

/// Vergleicht eine Composite-Index-Query mit der naiven Full-Scan-Auswertung.
fn assert_composite_eq_naive(store: &mut EntityStore, coll: &str, pred: Predicate, label: &str) {
    let got = run_ids(store, coll, pred.clone());
    let exp = naive(store, coll, &pred);
    assert_eq!(got, exp, "Phase K: {label}: composite query != naive");
}

/// Sammelt eine Menge semantischer Ergebnisse für den Backup/Restore-Vergleich.
fn snapshot(store: &mut EntityStore) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    out.push((
        "tasks:p1:todo".into(),
        run_ids(
            store,
            "tasks",
            eq("project_id", Value::String("p1".into()))
                .and(eq("status", Value::String("todo".into()))),
        ),
    ));
    out.push((
        "tasks:p1:tobias".into(),
        run_ids(
            store,
            "tasks",
            eq("project_id", Value::String("p1".into()))
                .and(eq("assignee", Value::String("tobias".into()))),
        ),
    ));
    out.push((
        "tasks:doing>=3".into(),
        run_ids(
            store,
            "tasks",
            eq("status", Value::String("doing".into())).and(ge("priority", Value::Int(3))),
        ),
    ));
    // Aggregationen.
    let mut b = store.query("tasks").unwrap();
    b = b.aggregate(Aggregate::Count);
    out.push((
        "count(tasks)".into(),
        vec![fmt(store.execute_aggregate(b).unwrap())],
    ));
    let mut b = store.query("tasks").unwrap();
    b = b.filter(eq("status", Value::String("done".into())));
    b = b.aggregate(Aggregate::Count);
    out.push((
        "count(done)".into(),
        vec![fmt(store.execute_aggregate(b).unwrap())],
    ));
    let mut b = store.query("tasks").unwrap();
    b = b.aggregate(Aggregate::Avg("estimate".into()));
    out.push((
        "avg(estimate)".into(),
        vec![fmt(store.execute_aggregate(b).unwrap())],
    ));
    out.push((
        "events:t-p1-0".into(),
        run_ids(
            store,
            "events",
            eq("task_id", Value::String("t-p1-0".into())),
        ),
    ));
    out
}

fn fmt(v: Option<Value>) -> String {
    match v {
        None => "null".into(),
        Some(Value::Int(i)) => i.to_string(),
        Some(Value::Float(f)) => f.to_string(),
        Some(other) => format!("{other:?}"),
    }
}

#[test]
fn phase_k_task_project_db() -> Result<()> {
    eprintln!("=== Phase K: local task/project DB on 911c4c7 ===");
    let dir_a = tempfile::tempdir().unwrap();
    let mut store = EntityStore::open(dir_a.path()).unwrap();
    let t0 = Instant::now();
    seed(&mut store)?;
    eprintln!(
        "OBSERVE seed {} tasks/project in {:.2?}",
        crate::tasks_per_project(),
        t0.elapsed()
    );

    // --- Reopen-Check #1: Schema + Composite-Indizes müssen nach Neubau
    //     (Commit/fsync) lesbar sein und mit naive übereinstimmen. ---
    {
        let mut reopened = EntityStore::open(dir_a.path()).unwrap();
        assert_composite_eq_naive(
            &mut reopened,
            "tasks",
            eq("project_id", Value::String("p1".into()))
                .and(eq("status", Value::String("todo".into()))),
            "reopen#1 project_id=p1 & status=todo",
        );
        assert_composite_eq_naive(
            &mut reopened,
            "tasks",
            eq("status", Value::String("doing".into())).and(ge("priority", Value::Int(3))),
            "reopen#1 status=doing & priority>=3",
        );
        assert_composite_eq_naive(
            &mut reopened,
            "events",
            eq("task_id", Value::String("t-p1-0".into())),
            "reopen#1 events task_id=t-p1-0",
        );
    }

    // --- CRUD ---
    store
        .collection("tasks")
        .unwrap()
        .put(
            "t-crud",
            &e(&[
                ("project_id", Value::String("p1".into())),
                ("title", Value::String("CRUD probe".into())),
                ("status", Value::String("todo".into())),
                ("priority", Value::Int(2)),
                ("assignee", Value::String("alice".into())),
                ("created_at", Value::Int(9999)),
                ("due_at", Value::Int(10000)),
                ("estimate", Value::Int(3)),
            ]),
        )
        .unwrap();
    assert!(store.collection("tasks").unwrap().get("t-crud").is_ok());
    store.collection("tasks").unwrap().delete("t-crud").unwrap();
    assert!(
        store
            .collection("tasks")
            .unwrap()
            .get("t-crud")
            .unwrap()
            .is_none()
    );
    store
        .collection("tasks")
        .unwrap()
        .put(
            "t-crud",
            &e(&[
                ("project_id", Value::String("p1".into())),
                ("title", Value::String("CRUD probe 2".into())),
                ("status", Value::String("todo".into())),
                ("priority", Value::Int(2)),
                ("assignee", Value::String("alice".into())),
                ("created_at", Value::Int(9999)),
                ("due_at", Value::Int(10000)),
                ("estimate", Value::Int(3)),
            ]),
        )
        .unwrap();

    // --- CAS / Partial Update ---
    // (a) Fehlschlagender Versuch: Task ist bereits "todo", aber wir erwarten
    //     "doing" → Conflict.
    let conflict = store.cas_update(
        "tasks",
        "t-crud",
        &Expected::Field("status".into(), Value::String("doing".into())),
        &[
            Patch::Set("status".into(), Value::String("doing".into())),
            Patch::Set("assignee".into(), Value::String("tobias".into())),
            Patch::Increment("priority".into(), Value::Int(1)),
        ],
    );
    assert!(
        conflict.is_err(),
        "Phase K: CAS mit falscher Erwartung hätte fehlschlagen müssen"
    );
    // (b) Erfolgreicher Versuch: status==todo → doing, assignee=tobias, priority+=1.
    let applied = store
        .cas_update(
            "tasks",
            "t-crud",
            &Expected::Field("status".into(), Value::String("todo".into())),
            &[
                Patch::Set("status".into(), Value::String("doing".into())),
                Patch::Set("assignee".into(), Value::String("tobias".into())),
                Patch::Increment("priority".into(), Value::Int(1)),
            ],
        )
        .unwrap();
    assert_eq!(
        applied.field("status"),
        Some(&Value::String("doing".into()))
    );
    assert_eq!(
        applied.field("assignee"),
        Some(&Value::String("tobias".into()))
    );
    assert_eq!(applied.field("priority"), Some(&Value::Int(3)));

    // --- Queries: Composite vs Naive, ORDER BY+LIMIT, Aggregation, Projektion ---
    assert_composite_eq_naive(
        &mut store,
        "tasks",
        eq("project_id", Value::String("p1".into()))
            .and(eq("status", Value::String("todo".into()))),
        "project_id=p1 & status=todo",
    );
    assert_composite_eq_naive(
        &mut store,
        "tasks",
        eq("project_id", Value::String("p1".into()))
            .and(eq("assignee", Value::String("tobias".into()))),
        "project_id=p1 & assignee=tobias",
    );
    assert_composite_eq_naive(
        &mut store,
        "tasks",
        eq("status", Value::String("doing".into())).and(ge("priority", Value::Int(3))),
        "status=doing & priority>=3",
    );

    // ORDER BY priority LIMIT 20 (Composite deckt status/priority, Sort im Executor).
    let mut b = store.query("tasks").unwrap();
    b = b
        .filter(eq("status", Value::String("doing".into())).and(ge("priority", Value::Int(3))))
        .sort("priority", SortDir::Asc)
        .limit(20);
    let rows = store.execute_query(b).unwrap();
    assert!(rows.len() <= 20);
    let mut prio_ok = true;
    let mut last: Option<i64> = None;
    for (_, ent) in &rows {
        if let Some(Value::Int(p)) = ent.field("priority") {
            if let Some(l) = last {
                if p < &l {
                    prio_ok = false;
                }
            }
            last = Some(*p);
        }
    }
    assert!(
        prio_ok,
        "Phase K: ORDER BY priority Lieferung nicht aufsteigend"
    );

    // Aggregationen.
    let mut b = store.query("tasks").unwrap();
    b = b.aggregate(Aggregate::Count);
    let c_all = store.execute_aggregate(b).unwrap();
    eprintln!("OBSERVE count(tasks) = {:?}", c_all);
    let mut b = store.query("tasks").unwrap();
    b = b.aggregate(Aggregate::Avg("estimate".into()));
    let avg_est = store.execute_aggregate(b).unwrap();
    eprintln!("OBSERVE avg(estimate) = {:?}", avg_est);
    let mut b = store.query("tasks").unwrap();
    b = b
        .filter(eq("status", Value::String("done".into())))
        .aggregate(Aggregate::Sum("estimate".into()));
    let sum_done = store.execute_aggregate(b).unwrap();
    eprintln!("OBSERVE sum(estimate) where status=done = {:?}", sum_done);

    // Projektion.
    let mut b = store.query("tasks").unwrap();
    b = b
        .filter(eq("project_id", Value::String("p1".into())))
        .project(&["id", "title", "priority", "assignee"]);
    let proj = store.execute_query(b).unwrap();
    for (id, ent) in &proj {
        assert!(
            ent.field("project_id").is_none(),
            "Phase K: Projektion enthält nicht angefragtes Feld project_id bei {id}"
        );
        assert!(
            ent.field("priority").is_some(),
            "Phase K: Projektion verliert priority bei {id}"
        );
    }

    // --- Transaktion: todo→doing + Assignee + Event + Commit ---
    let tx_task = "t-p2-0"; // ist (0 % 3 == 0) → status todo
    {
        let mut tx = store.transaction()?;
        let t = tx.get("tasks", tx_task)?.unwrap();
        // Vollersetzung (tx.update ersetzt die Entity).
        let mut fields = t.fields.clone();
        for (k, v) in &mut fields {
            if *k == "status" {
                *v = Value::String("doing".into());
            }
            if *k == "assignee" {
                *v = Value::String("tobias".into());
            }
        }
        let updated = Entity { fields };
        tx.update("tasks", tx_task, &updated)?;
        tx.update(
            "events",
            &format!("e-{tx_task}-tx"),
            &e(&[
                ("task_id", Value::String(tx_task.into())),
                ("kind", Value::String("transition".into())),
                ("actor", Value::String("tobias".into())),
                ("created_at", Value::Int(1_000_000)),
                ("payload", Value::String("todo->doing".into())),
            ]),
        )?;
        tx.commit()?;
    }
    // Ersten Store sauber schließen, bevor wir das Verzeichnis neu öffnen.
    // (Realistische Nutzung: pro DB-Verzeichnis genau ein Store-Handle; ein
    //  gleichzeitiges zweites Open sieht WAL-only-Daten des ersten nicht.)
    store.close()?;

    // Reopen-Check #2: Zustand, Index und Event müssen nach Neubau konsistent sein.
    {
        let mut reopened = EntityStore::open(dir_a.path()).unwrap();
        let t = reopened
            .collection("tasks")
            .unwrap()
            .get(tx_task)
            .unwrap()
            .unwrap();
        assert_eq!(t.field("status"), Some(&Value::String("doing".into())));
        assert_eq!(t.field("assignee"), Some(&Value::String("tobias".into())));
        // Neuer Event vorhanden.
        let ev = run_ids(
            &mut reopened,
            "events",
            eq("task_id", Value::String(tx_task.into()))
                .and(eq("kind", Value::String("transition".into()))),
        );
        assert!(
            ev.contains(&format!("e-{tx_task}-tx")),
            "Phase K: Tx-Event fehlt nach Reopen"
        );
        // Composite (project_id,status) zeigt die verschobene Task.
        let comp = reopened
            .collection("tasks")
            .unwrap()
            .find_composite(
                &["project_id", "status"],
                &[
                    (
                        0,
                        Bound::Inclusive(Value::String("p2".into())),
                        Bound::Inclusive(Value::String("p2".into())),
                    ),
                    (
                        1,
                        Bound::Inclusive(Value::String("doing".into())),
                        Bound::Inclusive(Value::String("doing".into())),
                    ),
                ],
            )
            .unwrap();
        assert!(
            comp.contains(&tx_task.to_string()),
            "Phase K: Composite (p2,doing) enthält verschobene Task nicht"
        );
    }

    // --- Backup / Restore mit semantischem Vorher/Nachher-Vergleich ---
    let mut reopened = EntityStore::open(dir_a.path()).unwrap();
    let before = snapshot(&mut reopened);
    let backup_dir = tempfile::tempdir().unwrap();
    reopened.backup(backup_dir.path()).unwrap();
    // Weiterarbeiten auf A (darf den Restore-Snapshot nicht verfälschen).
    reopened
        .collection("tasks")
        .unwrap()
        .put(
            "t-extra-after-backup",
            &e(&[
                ("project_id", Value::String("p3".into())),
                ("title", Value::String("after backup".into())),
                ("status", Value::String("todo".into())),
                ("priority", Value::Int(1)),
                ("assignee", Value::String("alice".into())),
                ("created_at", Value::Int(5_000_000)),
                ("due_at", Value::Int(5_000_010)),
                ("estimate", Value::Int(2)),
            ]),
        )
        .unwrap();
    reopened.close()?;
    let restore_dir = tempfile::tempdir().unwrap();
    EntityStore::restore(backup_dir.path(), restore_dir.path()).unwrap();
    let mut restored = EntityStore::open(restore_dir.path()).unwrap();
    let after = snapshot(&mut restored);
    assert_eq!(
        before, after,
        "Phase K: semantische Ergebnisse vor/nach Restore weichen ab"
    );
    // Auch auf der restored DB: Composite == Naive.
    assert_composite_eq_naive(
        &mut restored,
        "tasks",
        eq("project_id", Value::String("p1".into()))
            .and(eq("status", Value::String("todo".into()))),
        "restored project_id=p1 & status=todo",
    );
    restored.close()?;

    eprintln!("=== Phase K: OK (alle Workflows semantisch konsistent) ===");
    Ok(())
}
