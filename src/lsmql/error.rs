//! Fehlerklassen für LSMQL.
//!
//! Wichtig (Gate 4): **Parser-Fehler ≠ DB-Fehler**. LSMQL trennt syntaktische,
//! semantische und "nicht unterstützte" Fehler sauber von `my-lsm-db`-Fehlern.
//! Eine LSMQL-Abfrage schlägt niemals mit einem internen Storage-/Planner-Fehler
//! fehl, den der Aufrufer nicht als Query-Fehler erkennen kann.

use std::fmt;

/// Fehler einer LSMQL-Abfrage (Parsing / Semantik / Unsupported).
///
/// DB-Ebene-Fehler (`my_lsm_db::error::Error`) werden an der Engine-Grenze
/// gefangen und in [`LsmqlError::Execution`] verpackt — so bleibt die
/// Fehlerursache für den Aufrufer klar einer von vier Klassen zugeordnet.
#[derive(Debug, Clone, PartialEq)]
pub enum LsmqlError {
    /// Syntaxfehler im Lexer/Parser.
    Parse {
        message: String,
        line: usize,
        col: usize,
    },

    /// Semantische Validierung fehlgeschlagen (Collection/Feld/Typ/Parameter).
    Semantic(SemanticError),

    /// Feature ist in v1 bewusst nicht implementiert (z.B. GROUP BY, IS ABSENT).
    Unsupported { feature: String, reason: String },

    /// Ausführungsfehler auf DB-Ebene (Planner/Executor), von LSMQL
    /// transparent durchgereicht.
    Execution { message: String },
}

/// Semantische Fehler mit präziser Unterteilung (Gate 4).
#[derive(Debug, Clone, PartialEq)]
pub enum SemanticError {
    /// `FROM <x>`: Collection ist im Schema nicht bekannt.
    UnknownCollection { collection: String },
    /// Feld ist auf der Collection nicht definiert (Projektion/Aggregat/WHERE).
    UnknownField { collection: String, field: String },
    /// Operator vs. Wertetyp inkompatibel.
    TypeMismatch {
        field: String,
        expected: String,
        got: String,
    },
    /// `$name` wird in `params` nicht geliefert.
    UnboundParameter { name: String },
    /// Aggregation auf ein Feld angewendet, das nicht numerisch ist (vorgeprüft).
    NonNumericAggregate { function: String, field: String },
}

impl fmt::Display for LsmqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LsmqlError::Parse { message, line, col } => {
                write!(f, "parse error at {line}:{col}: {message}")
            }
            LsmqlError::Semantic(e) => match e {
                SemanticError::UnknownCollection { collection } => {
                    write!(f, "unknown collection: {collection}")
                }
                SemanticError::UnknownField { collection, field } => {
                    write!(f, "unknown field '{field}' on collection '{collection}'")
                }
                SemanticError::TypeMismatch {
                    field,
                    expected,
                    got,
                } => write!(
                    f,
                    "type mismatch on '{field}': expected {expected}, got {got}"
                ),
                SemanticError::UnboundParameter { name } => {
                    write!(f, "unbound parameter: ${name}")
                }
                SemanticError::NonNumericAggregate { function, field } => {
                    write!(f, "{function} requires a numeric field, '{field}' is not")
                }
            },
            LsmqlError::Unsupported { feature, reason } => {
                write!(f, "unsupported in v1: {feature} ({reason})")
            }
            LsmqlError::Execution { message } => write!(f, "execution error: {message}"),
        }
    }
}

impl std::error::Error for LsmqlError {}

/// Ergebnistyp für LSMQL-Operationen.
pub type LsmqlResult<T> = Result<T, LsmqlError>;
