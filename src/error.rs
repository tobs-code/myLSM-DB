use std::io;

/// Fehler der LSM-Engine.
#[derive(Debug)]
pub enum Error {
    /// I/O- oder Systemfehler.
    Io(io::Error),
    /// Datensatz-Datei ist korrupt (CRC-Fehler).
    Corrupt(&'static str),
    /// Ein Eintrag existiert nicht.
    NotFound,
    /// Verzeichnis/Format ist ungültig oder inkompatibel.
    InvalidFormat(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Corrupt(what) => write!(f, "corrupt data: {what}"),
            Error::NotFound => write!(f, "not found"),
            Error::InvalidFormat(s) => write!(f, "invalid format: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
