//! LSMQL Oracle (30-Query-Matrix-Slice).
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
            ("assignee".into(), Value::Null), // explizit NULL
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
    // t1: assignee fehlt komplett (absent)
    let mut e1 = task("A", "todo", 2, Some("Tobias"), "p1", Some(3));
    e1.fields.retain(|(n, _)| n != "assignee");
    col.put("t1", &e1).unwrap();
    // t2: assignee ist explizit NULL
    col.put("t2", &task_with_null("B", "todo")).unwrap();
    // t3: assignee ist "Anna"
    col.put("t3", &task("C", "todo", 1, Some("Anna"), "p1", None))
        .unwrap();

    let res_null = lsmql::run(
        &mut store,
        "SELECT * FROM tasks WHERE assignee IS NULL",
        &std::collections::HashMap::new(),
    )
    .unwrap();
    match res_null {
        lsmql::QueryResult::Rows(rows) => assert_eq!(rows.len(), 1), // nur t2
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
