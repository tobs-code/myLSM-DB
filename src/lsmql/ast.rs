//! LSMQL AST — die semantische Wahrheit der Sprache (Gate 1).
//!
//! Der AST ist **unabhängig** vom `my-lsm-db`-`QueryBuilder`/`Predicate`.
//! Das Mapping nach [`crate::query::ast::Predicate`] (in `translate.rs`) darf
//! keine Information verlieren — insbesondere bei `Not` und verschachteltem
//! `And`/`Or`. Daher ist `Expr` ein echter, verschachtelter Bool-Baum
//! (`And`/`Or`/`Not`/`Pred`), keine flache Filterliste.

/// Eine vollständige LSMQL-Abfrage.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub explain: bool,
    pub projection: Projection,
    pub source: String,
    pub predicate: Option<Expr>,
    pub group_by: Vec<String>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<usize>,
    pub offset: usize,
}

/// Projektion: `*` oder eine Liste von Feldern/Aggregaten.
#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    Star,
    Items(Vec<ProjItem>),
}

/// Ein Element der Projektionsliste.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjItem {
    Field(String),
    /// Aggregat über ein Feld (`COUNT(*)` → `field = None`).
    Agg {
        kind: AggKind,
        field: Option<String>,
    },
}

/// Aggregationsfunktionen (v0.8-Semantik).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// Sortier-Richtung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    pub fn as_query_dir(&self) -> crate::query::logical::SortDir {
        match self {
            SortDir::Asc => crate::query::logical::SortDir::Asc,
            SortDir::Desc => crate::query::logical::SortDir::Desc,
        }
    }
}

/// Ein `ORDER BY`-Element.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub field: String,
    pub dir: SortDir,
}

/// Boolescher Ausdruck (echter AST, verschachtelbar).
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Pred(Predicate),
}

impl Expr {
    /// Hilfs-Konstruktor für ein einzelnes Prädikat.
    pub fn pred(p: Predicate) -> Expr {
        Expr::Pred(p)
    }
}

/// Atomares Prädikat über einem Feld.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Eq(String, Value),
    Ne(String, Value),
    Lt(String, Value),
    Le(String, Value),
    Gt(String, Value),
    Ge(String, Value),
    In(String, Vec<Value>),
    /// `field IS NULL` — explizit gespeicherter Nullwert (`Value::Null`).
    /// ≠ `IsAbsent` (siehe Spec §4 / v1.2-v1.3-Semantik).
    IsNull(String),
    /// `field IS ABSENT` — Feld fehlt komplett. In v1 AST-reserviert;
    /// der Executor liefert `UnsupportedQuery` (wie `GROUP BY`).
    IsAbsent(String),
}

/// Literal-Wert in LSMQL (Parameter werden vor der Validierung substituiert).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Number(f64),
    Bool(bool),
    /// Expliziter Nullwert (entspricht `Value::Null` in der Engine).
    Null,
    /// `$name` — vor Semantic Validation durch `params` ersetzt.
    Param(String),
}
