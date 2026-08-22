//! LSMQL Oracle (30-Query-Matrix-Slice) — Consumer-tauglich.
//!
//! Stützt sich auf `docs/design-lsmql.md` §9. Prüft Pipeline:
//! Lexer → Parser → Semantic Validation → QueryBuilder → Execution.
//! IS ABSENT / GROUP BY werden als `UnsupportedQuery` erwartet.

use tempfile::TempDir;

use my_lsm_db::codec::Value;
use my_lsm_db::entity::{Entity, EntityStore};
use my_lsm_db::lsmql;

fn task(
    title: &str,
    status: &str,
    priority: i64,
    assignee: Option<&str>,
    project_id: &str,
    estimate: Option<i64>,
) -> Entity {
    let mut fields = vec![
        ("title".into(), Value::String(title.into())),
        ("status".into(), Value::String(status.into())),
        ("priority".into(), Value::Int(priority)),
        ("project_id".into(), Value::String(project_id.into())),
    ];
    if let Some(a) = assignee {
        fields.push(("assignee".into(), Value::String(a.into())));
    }
    if let Some(e) = estimate {
        fields.push(("estimate".into(), Value::Int(e)));
    }
    Entity { fields }
}

fn task_with_null(title: &str, status: &str) -> Entity {
    Entity {
        fields: vec![
            ("title".into(), Value::String(title.into())),
            ("status".into(), Value::String(status.into())),
            ("priority".into(), Value::Int(1)),
            ("project_id".into(), Value::String("p1".into())),
            ("assignee".into(), Value::Null),
        ],
    }
}

#[test]
fn oracle_basic_select_star() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("tasks").unwrap();
    col.put("t1", &task("A", "todo", 2, Some("Tobias"), "p1", Some(3)))
        .unwrap();
    col.put("t2", &task("B", "doing", 1, Some("Anna"), "p1", Some(5)))
        .unwrap();

    let mut params = std::collections::HashMap::new();
    params.insert("status".into(), Value::String("todo".into()));
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE status = $status",
        &params,
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => assert_eq!(rows.len(), 1),
        _ => panic!("expected rows"),
    }
}

#[test]
fn oracle_and_or_predicates() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("tasks").unwrap();
    col.put("t1", &task("A", "todo", 2, Some("Tobias"), "p1", Some(3)))
        .unwrap();
    col.put("t2", &task("B", "todo", 5, Some("Anna"), "p1", Some(8)))
        .unwrap();
    col.put("t3", &task("C", "doing", 3, Some("Tobias"), "p1", Some(2)))
        .unwrap();

    let mut params = std::collections::HashMap::new();
    params.insert("p".into(), Value::String("p1".into()));
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE project_id = $p AND (status = 'todo' OR status = 'doing')",
        &params,
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => assert_eq!(rows.len(), 3),
        _ => panic!("expected rows"),
    }
}

#[test]
fn oracle_not_predicate() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("tasks").unwrap();
    col.put("t1", &task("A", "todo", 2, Some("Tobias"), "p1", Some(3)))
        .unwrap();
    col.put("t2", &task("B", "done", 9, Some("Anna"), "p1", Some(1)))
        .unwrap();

    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE NOT status = 'done'",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => assert_eq!(rows.len(), 1),
        _ => panic!("expected rows"),
    }
}

#[test]
fn oracle_is_null_vs_absent() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("tasks").unwrap();
    let mut e1 = task("A", "todo", 2, Some("Tobias"), "p1", Some(3));
    e1.fields.retain(|(n, _)| n != "assignee");
    col.put("t1", &e1).unwrap();
    col.put("t2", &task_with_null("B", "todo")).unwrap();
    col.put("t3", &task("C", "todo", 1, Some("Anna"), "p1", None))
        .unwrap();

    let res_null = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE assignee IS NULL",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res_null {
        lsmql::QueryResult::Rows(rows) => assert_eq!(rows.len(), 1),
        _ => panic!("expected rows"),
    }
}

#[test]
fn oracle_in_operator() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("tasks").unwrap();
    col.put("t1", &task("A", "todo", 2, Some("Tobias"), "p1", Some(3)))
        .unwrap();
    col.put("t2", &task("B", "doing", 1, Some("Anna"), "p1", Some(5)))
        .unwrap();
    col.put("t3", &task("C", "done", 9, Some("Max"), "p1", Some(1)))
        .unwrap();

    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE status IN ('todo', 'doing')",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => assert_eq!(rows.len(), 2),
        _ => panic!("expected rows"),
    }
}

#[test]
fn oracle_order_limit_offset() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("tasks").unwrap();
    col.put("t1", &task("A", "todo", 1, Some("Tobias"), "p1", Some(3)))
        .unwrap();
    col.put("t2", &task("B", "todo", 5, Some("Anna"), "p1", Some(8)))
        .unwrap();
    col.put("t3", &task("C", "todo", 3, Some("Max"), "p1", Some(2)))
        .unwrap();

    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE status = 'todo' ORDER BY priority DESC LIMIT 2",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].0, "t2");
            assert_eq!(rows[1].0, "t3");
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn oracle_aggregate_count() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("tasks").unwrap();
    col.put("t1", &task("A", "todo", 2, Some("Tobias"), "p1", Some(3)))
        .unwrap();
    col.put("t2", &task("B", "doing", 1, Some("Anna"), "p1", Some(5)))
        .unwrap();
    col.put("t3", &task("C", "done", 9, Some("Max"), "p1", Some(1)))
        .unwrap();

    let res = lsmql::run(
        &mut store,
        "SELECT COUNT(*) FROM tasks",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Scalar(Some(Value::Int(n))) => assert_eq!(n, 3),
        _ => panic!("expected scalar count"),
    }
}

#[test]
fn oracle_explain() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("tasks").unwrap();
    col.put("t1", &task("A", "todo", 2, Some("Tobias"), "p1", Some(3)))
        .unwrap();
    let plan = lsmql::explain(
        &store,
        "SELECT * FROM tasks WHERE status = 'todo'",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    assert!(
        plan.contains("CompositeIndexScan") || plan.contains("FullScan") || plan.contains("tasks")
    );
}

#[test]
fn oracle_unsupported_group_by() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("tasks").unwrap();
    col.put("t1", &task("A", "todo", 2, Some("Tobias"), "p1", Some(3)))
        .unwrap();

    let res = lsmql::run(
        &mut store,
        "SELECT status, COUNT(*) FROM tasks GROUP BY status",
        &std::collections::HashMap::new(),
    );
    assert!(matches!(
        res,
        Err(my_lsm_db::lsmql::LsmqlError::Unsupported { .. })
    ));
}

#[test]
fn oracle_unsupported_is_absent() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("tasks").unwrap();
    col.put("t1", &task("A", "todo", 2, Some("Tobias"), "p1", Some(3)))
        .unwrap();

    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE assignee IS ABSENT",
        &std::collections::HashMap::new(),
    );
    assert!(matches!(
        res,
        Err(my_lsm_db::lsmql::LsmqlError::Unsupported { .. })
    ));
}

#[test]
fn oracle_unknown_collection() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM no_such_collection",
        &std::collections::HashMap::new(),
    );
    assert!(matches!(
        res,
        Err(my_lsm_db::lsmql::LsmqlError::Semantic(
            my_lsm_db::lsmql::SemanticError::UnknownCollection { .. }
        ))
    ));
}

// --- §9 Vollständige 30er-Matrix (Rest, als permanente Regression) ---

/// Befüllt `tasks` mit einer deterministischen Menge für die Matrix-Abnahme.
fn seed_tasks(store: &mut EntityStore) {
    let mut col = store.collection("tasks").unwrap();
    col.put("t1", &task("A", "todo", 2, Some("tobias"), "p1", Some(3)))
        .unwrap();
    col.put("t2", &task("B", "todo", 5, Some("anna"), "p1", Some(8)))
        .unwrap();
    col.put("t3", &task("C", "doing", 3, Some("tobias"), "p1", Some(2)))
        .unwrap();
    col.put("t4", &task("D", "done", 9, Some("max"), "p1", Some(1)))
        .unwrap();
    col.put("t5", &task("E", "todo", 1, None, "p2", Some(4)))
        .unwrap();
    // t6 hat explizit-NULL assignee
    let mut e6 = task("F", "todo", 7, Some("x"), "p1", Some(6));
    e6.fields.retain(|(n, _)| n != "assignee");
    col.put("t6", &e6).unwrap();
}

#[test]
fn matrix_02_composite_prefix() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE project_id = 'p1' AND status = 'todo'",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            let mut ids: Vec<_> = rows.iter().map(|(id, _)| id.clone()).collect();
            ids.sort();
            assert_eq!(ids, vec!["t1", "t2", "t6"]);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_03_status_only() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE status = 'todo'",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => assert_eq!(rows.len(), 4),
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_04_projection_fields() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT id, title FROM tasks WHERE project_id = 'p1'",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            assert_eq!(rows.len(), 5);
            // id ist der Row-Key (Tupel.0), title in den projizierten Feldern.
            for (id, fields) in &rows {
                assert!(!id.is_empty());
                assert!(fields.contains_key("title"));
                assert!(!fields.contains_key("assignee"));
                assert!(!fields.contains_key("priority"));
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_05_priority_residual() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE priority >= 2",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            // t5 (priority 1) ausgeschlossen; Rest (2..9) inkludiert = 5
            assert_eq!(rows.len(), 5);
            assert!(!rows.iter().any(|(id, _)| id == "t5"));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_09_assignee_eq() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE assignee = 'tobias'",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            let mut ids: Vec<_> = rows.iter().map(|(id, _)| id.clone()).collect();
            ids.sort();
            assert_eq!(ids, vec!["t1", "t3"]);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_14_limit_offset() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE project_id = 'p1' ORDER BY priority ASC LIMIT 2 OFFSET 2",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            // p1: t1(2) t3(3) t2(5) t6(7) t4(9), OFFSET 2 → t2, t6
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].0, "t2");
            assert_eq!(rows[1].0, "t6");
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_16_count_sum_avg() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT COUNT(*), SUM(estimate), AVG(estimate) FROM tasks WHERE project_id = 'p1'",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Scalar(_) => {}
        _ => panic!("expected scalar"),
    }
}

#[test]
fn matrix_17_global_min_max() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT MIN(estimate), MAX(estimate) FROM tasks",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Scalar(Some(Value::Int(_))) => {}
        _ => panic!("expected scalar int (min/max collapse to one value)"),
    }
}

#[test]
fn matrix_20_explain_fullscan() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let plan = lsmql::explain(
        &store,
        "SELECT * FROM tasks WHERE title = 'foo'",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    assert!(plan.contains("FullScan") || plan.contains("tasks"));
}

#[test]
fn matrix_21_params() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let mut params = std::collections::HashMap::new();
    params.insert("p".into(), Value::String("p1".into()));
    params.insert("s".into(), Value::String("todo".into()));
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE project_id = $p AND status = $s",
        &params,
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            let mut ids: Vec<_> = rows.iter().map(|(id, _)| id.clone()).collect();
            ids.sort();
            assert_eq!(ids, vec!["t1", "t2", "t6"]);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_22_lt_gt() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE priority < 5 AND priority > 1",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            let mut ids: Vec<_> = rows.iter().map(|(id, _)| id.clone()).collect();
            ids.sort();
            // priority: t5=1(t), t1=2, t3=3, t2=5(f), t6=7, t4=9
            assert_eq!(ids, vec!["t1", "t3"]);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_23_ne() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE assignee != 'x'",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            // t6 hat kein assignee (absent) → != 'x' matcht (absent ≠ x)
            assert_eq!(rows.len(), 6);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_24_events_activity_feed() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    let mut col = store.collection("events").unwrap();
    col.put(
        "e1",
        &Entity {
            fields: vec![
                ("task_id".into(), Value::String("t1".into())),
                ("created_at".into(), Value::Int(10)),
            ],
        },
    )
    .unwrap();
    col.put(
        "e2",
        &Entity {
            fields: vec![
                ("task_id".into(), Value::String("t1".into())),
                ("created_at".into(), Value::Int(30)),
            ],
        },
    )
    .unwrap();
    col.put(
        "e3",
        &Entity {
            fields: vec![
                ("task_id".into(), Value::String("t2".into())),
                ("created_at".into(), Value::Int(20)),
            ],
        },
    )
    .unwrap();

    let res = lsmql::run(
        &mut store,
        "SELECT * FROM events WHERE task_id = 't1' ORDER BY created_at DESC LIMIT 50",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].0, "e2"); // created_at 30
            assert_eq!(rows[1].0, "e1"); // created_at 10
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_25_not_is_absent_unsupported() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE project_id = 'p1' AND NOT(assignee IS ABSENT)",
        &std::collections::HashMap::new(),
    );
    // IS ABSENT ist reserviert → Unsupported, auch verschachtelt unter NOT
    assert!(matches!(
        res,
        Err(my_lsm_db::lsmql::LsmqlError::Unsupported { .. })
    ));
}

#[test]
fn matrix_26_title_projection_limit() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT title FROM tasks WHERE project_id = 'p1' AND status = 'todo' LIMIT 5",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            assert!(rows.len() <= 5);
            for (_, fields) in &rows {
                assert!(fields.contains_key("title"));
                assert!(!fields.contains_key("priority"));
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_27_done_estimate() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let res = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE status = 'done' AND estimate >= 8",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res {
        lsmql::QueryResult::Rows(rows) => {
            // t4: done, estimate 1 → aus; keine weiteren done
            assert_eq!(rows.len(), 0);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn matrix_29_explain_count() {
    let dir = TempDir::new().unwrap();
    let mut store = EntityStore::open(dir.path()).unwrap();
    seed_tasks(&mut store);
    let plan = lsmql::explain(
        &store,
        "SELECT COUNT(*) FROM tasks WHERE project_id = 'p1'",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    assert!(!plan.is_empty());
}
