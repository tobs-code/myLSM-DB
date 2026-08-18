//! Logischer Plan + Query-Builder.
//!
//! Der Logical Plan ist **unabhängig von Zugriffspfaden**: Er beschreibt nur,
//! *was* gefragt ist (Collection, Filter, Sort, Limit). Welche Indizes benutzt
//! werden, entscheidet allein der Planner.

use super::ast::Predicate;

/// Sortierrichtung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// Logischer Plan.
#[derive(Debug, Clone)]
pub enum LogicalPlan {
    /// Alle Entities einer Collection.
    Collection { name: String },
    /// Filtert die Eingabe nach `pred`.
    Filter {
        input: Box<LogicalPlan>,
        pred: Predicate,
    },
    /// Sortiert die Eingabe nach `field` (deterministisch: bei Gleichheit wird
    /// die Entity-ID als Tie-Breaker verwendet).
    Sort {
        input: Box<LogicalPlan>,
        field: String,
        dir: SortDir,
    },
    /// Begrenzt die Eingabe auf `n` Zeilen.
    ///
    /// **Reihenfolge-Semantik:** `limit` ohne vorgelagertes `Sort` liefert keine
    /// garantierte Reihenfolge (sie hängt von der Scan-/Union-Reihenfolge ab).
    /// Für deterministische Ergebnisse muss explizit sortiert werden.
    Limit { input: Box<LogicalPlan>, n: usize },
}

impl LogicalPlan {
    /// Alle Prädikate, die auf eine Collection angewendet werden (für
    /// Explain/Debugging).
    pub fn predicate(&self) -> Option<&Predicate> {
        match self {
            LogicalPlan::Filter { pred, .. } => Some(pred),
            _ => None,
        }
    }
}

/// Fluent-Builder für eine Query. Intern wird der Plan von innen aufgebaut:
/// die äußerste Schicht (z.B. `Filter`/`Limit`) ist `self.plan`.
#[derive(Debug, Clone)]
pub struct QueryBuilder {
    collection: String,
    plan: LogicalPlan,
}

impl QueryBuilder {
    pub(crate) fn new(collection: &str) -> QueryBuilder {
        QueryBuilder {
            collection: collection.to_string(),
            plan: LogicalPlan::Collection {
                name: collection.to_string(),
            },
        }
    }

    pub fn collection(&self) -> &str {
        &self.collection
    }

    /// Fügt ein Filter-Prädikat hinzu (alle Filter werden mit `And` kombiniert).
    pub fn filter(mut self, pred: Predicate) -> QueryBuilder {
        self.plan = match self.plan {
            LogicalPlan::Filter { pred: old, input } => LogicalPlan::Filter {
                input,
                pred: old.and(pred),
            },
            other => LogicalPlan::Filter {
                input: Box::new(other),
                pred,
            },
        };
        self
    }

    /// Sortiert nach `field` aufsteigend.
    pub fn sort(mut self, field: &str, dir: SortDir) -> QueryBuilder {
        self.plan = LogicalPlan::Sort {
            input: Box::new(self.plan),
            field: field.to_string(),
            dir,
        };
        self
    }

    /// Begrenzt auf `n` Zeilen (Reihenfolge ohne vorgelagertes `Sort` ist
    /// undefiniert).
    pub fn limit(mut self, n: usize) -> QueryBuilder {
        self.plan = LogicalPlan::Limit {
            input: Box::new(self.plan),
            n,
        };
        self
    }

    /// Liefert den aufgebauten Logical Plan (für den Planner).
    pub fn build(self) -> LogicalPlan {
        self.plan
    }
}
