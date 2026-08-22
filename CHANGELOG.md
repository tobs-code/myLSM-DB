# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org).

## [Unreleased]

### Added
- LSMQL read-only query interface (`src/lsmql/`): lexer, parser, AST, semantic validation, translation to existing QueryBuilder, EXPLAIN bridge, parameter binding (`$name`), aggregation support (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`).
- 28 integration tests in `tests/lsmql_oracle.rs` covering the full §9 matrix slice + regression gates.
- HTTP contract tests in taskdb (`POST /api/query`, `POST /api/query/explain`).

### Changed
- `QueryResult::Rows` changed from `Vec<(String, Entity)>` to `Vec<(String, HashMap<String, Value>>)` for HTTP serialization without adding `serde_json` to `my-lsm-db`.

### Fixed
- Semantic validation now allows `id` as document-key field in projections/filters.
- `IS ABSENT` is correctly routed through the `UnsupportedQuery` path even when nested under `AND`/`OR`/`NOT` (previous implementation panicked in `translate.rs`).

### Release boundaries (v1.4.0-rc)
- **In scope:** read-only LSMQL (`SELECT`, `WHERE`, `AND`/`OR`/`NOT`, `IN`, `$param`, `IS NULL`, reserved `IS ABSENT` → `UnsupportedQuery`, `ORDER BY`, `LIMIT`/`OFFSET`, aggregations, `EXPLAIN`).
- **Out of scope (explicit):** mutations, `GROUP BY`, MVCC/snapshots, new index types, index intersection, remote backup, storage/WAL changes, own query planner, second query semantics alongside `QueryBuilder`.
- **Architecture invariant:** `LSMQL AST → QueryBuilder → existing Planner`. No second optimizer.

## [1.3.0] - 2025-10-15
- Composite (multi-field) secondary indexes.
- `explain_query()` public API.
- Backup/restore with version check.
