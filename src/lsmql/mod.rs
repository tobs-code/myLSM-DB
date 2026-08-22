//! LSMQL — eine kleine, read-only Query Language über `my-lsm-db`.
//!
//! Siehe `docs/design-lsmql.md` für Grammatik, AST, Semantik und Oracle.
//! LSMQL ist eine reine Frontend-Schicht: sie übersetzt in den bestehenden
//! `QueryBuilder` und nutzt ausschließlich den v1.3-Planner. Kein zweiter
//! Query Planner, keine Storage-Änderung.
//!
//! Pipeline: `Lexer → Parser → AST → Semantic Validation → QueryBuilder →
//! EntityStore::execute_query / explain_query`.

pub mod ast;
pub mod engine;
pub mod error;
pub mod lexer;
pub mod parser;
pub mod translate;
pub mod validate;

pub use engine::{QueryResult, explain, run};
pub use error::{LsmqlError, LsmqlResult, SemanticError};
