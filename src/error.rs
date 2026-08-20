use std::io;

/// Fehler der LSM-Engine.
#[derive(Debug)]
pub enum Error {
    /// I/O- oder Systemfehler.
    Io(io::Error),
    /// Datensatz-Datei ist korrupt (CRC-Fehler).
    Corrupt(&'static str),
    /// Die Format-Version der Datenbank liegt ausserhalb des unterstützten
    /// Bereichs (zu alt oder von einer neueren Binary geschrieben).
    ///
    /// Konzeptionell getrennt von [`Error::InvalidFormat`] (Struktur
    /// unlesbar) und [`Error::Corrupt`] (bekannte Version, kaputte Daten).
    UnsupportedFormatVersion {
        /// Gefundene Version (bzw. `1` bei Legacy ohne `VERSION`).
        found: u32,
        /// Kleinste von dieser Binary lesbare Version.
        min_supported: u32,
        /// Neueste (geschriebene) Version dieser Binary.
        max_supported: u32,
    },
    /// Ein Eintrag existiert nicht.
    NotFound,
    /// Verzeichnis/Format ist ungültig oder inkompatibel (Korruption/Encoding).
    InvalidFormat(String),
    /// Argument-/Aufruf-Fehler: falsche Nutzung der API.
    InvalidArgument(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Corrupt(what) => write!(f, "corrupt data: {what}"),
            Error::UnsupportedFormatVersion {
                found,
                min_supported,
                max_supported,
            } => write!(
                f,
                "unsupported format version {found} (supported range {min_supported}..={max_supported})"
            ),
            Error::NotFound => write!(f, "not found"),
            Error::InvalidFormat(s) => write!(f, "invalid format: {s}"),
            Error::InvalidArgument(s) => write!(f, "invalid argument: {s}"),
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
