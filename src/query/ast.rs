//! Predicate-AST für Queries.
//!
//! Nur **eine** AST-Form (keine Duplikate wie `Between`): `Between` wird vom
//! Builder zu `Gte ∧ Lte` umgebaut. Das hält die Planner-Regeln einfach.

use crate::codec::Value;

/// Vergleichsoperator auf einem Feld.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
}

impl Cmp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Cmp::Eq => "=",
            Cmp::Ne => "!=",
            Cmp::Lt => "<",
            Cmp::Lte => "<=",
            Cmp::Gt => ">",
            Cmp::Gte => ">=",
        }
    }
}

/// Ein logisches Prädikat über einem Feld einer Entität.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
    Field(String, Cmp, Value),
}

impl Predicate {
    /// Löst einen `Field(Cmp::Between-Äquivalent)` auf, sofern möglich: ein
    /// `Field(f, Gte(l)) ∧ Field(f, Lte(h))` wird zu `Field(f, Eq, l)` **nicht**
    /// vereinfacht — die Bound-Merging-Regel liegt im Planner. Diese Methode
    /// erzeugt nur die Basis-Atome.
    pub fn field(field: impl Into<String>, cmp: Cmp, value: Value) -> Predicate {
        Predicate::Field(field.into(), cmp, value)
    }

    pub fn and(self, other: Predicate) -> Predicate {
        Predicate::And(Box::new(self), Box::new(other))
    }

    pub fn or(self, other: Predicate) -> Predicate {
        Predicate::Or(Box::new(self), Box::new(other))
    }

    /// Negation (`Not`). `negate` statt `not`, um nicht mit `std::ops::Not`
    /// zu kollidieren.
    pub fn negate(self) -> Predicate {
        Predicate::Not(Box::new(self))
    }
}

/// Baut `Field(f, Eq, v)`.
pub fn eq(field: impl Into<String>, value: Value) -> Predicate {
    Predicate::field(field, Cmp::Eq, value)
}

/// Baut `Field(f, Ne, v)`.
pub fn ne(field: impl Into<String>, value: Value) -> Predicate {
    Predicate::field(field, Cmp::Ne, value)
}

/// Baut `Field(f, Lt, v)`.
pub fn lt(field: impl Into<String>, value: Value) -> Predicate {
    Predicate::field(field, Cmp::Lt, value)
}

/// Baut `Field(f, Lte, v)`.
pub fn le(field: impl Into<String>, value: Value) -> Predicate {
    Predicate::field(field, Cmp::Lte, value)
}

/// Baut `Field(f, Gt, v)`.
pub fn gt(field: impl Into<String>, value: Value) -> Predicate {
    Predicate::field(field, Cmp::Gt, value)
}

/// Baut `Field(f, Gte, v)`.
pub fn ge(field: impl Into<String>, value: Value) -> Predicate {
    Predicate::field(field, Cmp::Gte, value)
}
