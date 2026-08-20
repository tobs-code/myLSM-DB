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

/// Aggregationsfunktion für `execute_aggregate` (Terminal-Schritt, läuft über
/// die gefilterte, sortierte, limitierte Ergebnismenge).
///
/// Semantik (siehe `design-v0.8-query.md` §3):
/// - `Count`: Anzahl der Zeilen (immer `Some(Int)`).
/// - `Sum`/`Avg`/`Min`/`Max(field)`: nur numerische Werte des Feldes;
///   NULL/absent/non-numeric werden übersprungen. `Sum` akkumuliert in `i128`
///   und sättigt auf `i64`; sobald ein `Float` auftritt, wird zu `Float64`.
///   `Avg` liefert immer `Float64`. `Min`/`Max` bleiben `Int64` bzw. werden bei
///   Typmischung zu `Float64` promoviert. Nicht-endliche Floats werden
///   übersprungen. Leere/Null-wertige Menge → `None` (außer `Count`).
#[derive(Debug, Clone, PartialEq)]
pub enum Aggregate {
    /// Anzahl der (gefilterten, sortierten, limitierten) Zeilen.
    Count,
    /// Summe der numerischen Werte von `field`.
    Sum(String),
    /// Arithmetisches Mittel der numerischen Werte von `field` (Float64).
    Avg(String),
    /// Minimum der numerischen Werte von `field`.
    Min(String),
    /// Maximum der numerischen Werte von `field`.
    Max(String),
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
///
/// `projection` und `aggregation` sind **Terminal-Schritte** (wechselseitig
/// exklusiv): sie werden nach Scan/Filter/Sort/Limit auf die Ergebnismenge
/// angewandt. Beide gleichzeitig ist ein Fehler (siehe `execute_query` /
/// `execute_aggregate`).
#[derive(Debug, Clone)]
pub struct QueryBuilder {
    collection: String,
    plan: LogicalPlan,
    pub(crate) projection: Option<Vec<String>>,
    pub(crate) aggregation: Option<Aggregate>,
}

impl QueryBuilder {
    pub(crate) fn new(collection: &str) -> QueryBuilder {
        QueryBuilder {
            collection: collection.to_string(),
            plan: LogicalPlan::Collection {
                name: collection.to_string(),
            },
            projection: None,
            aggregation: None,
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

    /// Projiziert das Ergebnis auf die angegebenen Felder (Result-Form, kein
    /// Storage-/Decode-Trick). Die Reihenfolge der Ergebnisfelder folgt der
    /// Reihenfolge der Anfrage; ein angefordertes, aber auf einer Entity
    /// fehlendes Feld wird für diese Entity weggelassen (nicht als `Null`
    /// eingefügt). Eine leere Feldliste ist ein Fehler (`InvalidArgument`,
    /// geprüft bei der Ausführung). Projektion und Aggregation sind exklusiv.
    pub fn project(mut self, fields: &[&str]) -> QueryBuilder {
        self.projection = Some(fields.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Setzt eine Aggregation als Terminal-Schritt (siehe [`Aggregate`]).
    /// Projektion und Aggregation sind exklusiv.
    pub fn aggregate(mut self, agg: Aggregate) -> QueryBuilder {
        self.aggregation = Some(agg);
        self
    }

    /// Liefert den aufgebauten Logical Plan (für den Planner).
    pub fn build(self) -> LogicalPlan {
        self.plan
    }
}
