//! Physical Plan — die ausführbaren Zugriffspfad-Operatoren.
//!
//! ## Datenfluss-Invariante
//!
//! - `FullScan` liefert direkt `(id, Entity)`-Zeilen.
//! - `IndexScan` liefert eine Liste von **verifizierten** Entity-IDs
//!   (Kandidaten, die `index::find_m` bereits gegen die Entity geprüft hat).
//!   `Fetch` materialisiert daraus die Entitäten.
//! - `Fetch` **filtert niemals implizit** — die einzige Filter-Stelle ist
//!   `Filter`.

use crate::index::Bound;

use super::ast::Predicate;
use super::logical::SortDir;

/// Ein physischer Operator (nur Query-Ausführung, keine Mutation).
#[derive(Debug, Clone)]
pub enum PhysicalPlan {
    /// Scan über einen READY-Sekundärindex im Range `lower..upper`; liefert
    /// **verifizierte** Entity-IDs. `(Bound, Bound)` statt `FindOp`, damit auch
    /// gemischte Exklusivität (z.B. `>30 AND <40`) darstellbar ist.
    IndexScan {
        collection: String,
        field: String,
        lower: Bound,
        upper: Bound,
    },
    /// Scan über alle Entities einer Collection; liefert `(id, Entity)`.
    FullScan { collection: String },
    /// Geordneter Index-Scan (v0.6, Teil 3): streamt **verifizierte**
    /// `(id, Entity)`-Zeilen in Index-Reihenfolge (`dir`) über den Index-Range
    /// `lower..upper`. Nur einsetzbar, wenn das Feld für jede mögliche Treffer-
    /// Zeile garantiert vorhanden ist (Presence-Garantie des Planners).
    IndexOrderScan {
        collection: String,
        field: String,
        lower: Bound,
        upper: Bound,
        dir: SortDir,
    },
    /// Vereinigt die IDs mehrerer Teil-Pläne (OR-Klauseln) und **dedupliziert**.
    /// Die Teil-Pläne müssen allesamt `IndexScan`/`UnionIds` (ID-Quellen) sein.
    UnionIds { branches: Vec<PhysicalPlan> },
    /// Materialisiert Entitäten aus IDs (Kandidaten → `(id, Entity)`).
    Fetch {
        input: Box<PhysicalPlan>,
        collection: String,
    },
    /// Filtert Zeilen nach `pred` (Residual-Filter). Einzige Filter-Stelle.
    Filter {
        input: Box<PhysicalPlan>,
        pred: Predicate,
    },
    /// Sortiert nach `field`; bei Gleichheit wird die Entity-ID als
    /// Tie-Breaker verwendet.
    Sort {
        input: Box<PhysicalPlan>,
        field: String,
        dir: SortDir,
    },
    /// Begrenzt auf `n` Zeilen (Reihenfolge ohne vorgelagertes `Sort` undefiniert).
    Limit { input: Box<PhysicalPlan>, n: usize },
}

impl PhysicalPlan {
    pub fn input(&self) -> Option<&PhysicalPlan> {
        match self {
            PhysicalPlan::FullScan { .. }
            | PhysicalPlan::IndexScan { .. }
            | PhysicalPlan::IndexOrderScan { .. } => None,
            PhysicalPlan::UnionIds { .. } => None,
            PhysicalPlan::Fetch { input, .. }
            | PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::Limit { input, .. } => Some(input),
        }
    }

    /// Erzeugt eine zur Laufzeit passende "Type"-Bezeichnung für Explain.
    pub fn kind(&self) -> &'static str {
        match self {
            PhysicalPlan::IndexScan { .. } => "IndexScan",
            PhysicalPlan::IndexOrderScan { .. } => "IndexOrderScan",
            PhysicalPlan::FullScan { .. } => "FullScan",
            PhysicalPlan::UnionIds { .. } => "UnionIds",
            PhysicalPlan::Fetch { .. } => "Fetch",
            PhysicalPlan::Filter { .. } => "Filter",
            PhysicalPlan::Sort { .. } => "Sort",
            PhysicalPlan::Limit { .. } => "Limit",
        }
    }

    /// Liefert die collection (für Fetch/FullScan/IndexScan).
    pub fn collection(&self) -> Option<&str> {
        match self {
            PhysicalPlan::IndexScan { collection, .. }
            | PhysicalPlan::IndexOrderScan { collection, .. }
            | PhysicalPlan::FullScan { collection }
            | PhysicalPlan::Fetch { collection, .. } => Some(collection),
            _ => None,
        }
    }
}
