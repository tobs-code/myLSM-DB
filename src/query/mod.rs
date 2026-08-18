//! Query-Schicht über der Entity-/Index-Layer (v0.5).
//!
//! Der Planner ist **rein lesend** — er plant und führt Queries aus, mutiert aber
//! nichts. Ergebnis einer Query ist immer `Vec<(String, Entity)>` (Entity-ID +
//! vollständige Entität).
//!
//! Architektur (siehe README):
//!
//! ```text
//! Query (Builder) → Logical Plan → Planner → Physical Plan → Executor
//! ```
//!
//! ## Filter-Invariante
//!
//! `IndexScan` liefert bereits **verifizierte** Entity-IDs (via `index::find_m`,
//! das jede Kandidaten-Entity gegen ihren echten Wert prüft). `Fetch` filtert
//! deshalb **niemals implizit** — die einzige Stelle mit Filter-Semantik ist der
//! `Filter`-Operator. Ein Residual-Filter prüft ausschließlich die Bedingungen,
//! die nicht durch den gewählten Index abgedeckt sind.

pub mod ast;
pub mod executor;
pub mod explain;
pub mod expression;
pub mod logical;
pub mod physical;
pub mod planner;

pub use ast::{Cmp, Predicate, eq, ge, gt, le, lt, ne};
pub use logical::{LogicalPlan, QueryBuilder, SortDir};
pub use planner::plan;
