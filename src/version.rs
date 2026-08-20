//! DB-weiter Format-Versionierungsvertrag (v0.9).
//!
//! Einzige autoritative Quelle für die Format-Familie der Datenbank ist die
//! `VERSION`-Datei im DB-Verzeichnis. Es werden **keine** einzelnen
//! SSTable-/WAL-/Key-/Value-Formate versioniert — genau das ist der zentrale
//! Vorteil dieses Designs: das Problem wird auf DB-Ebene gelöst, ohne sämtliche
//! On-Disk-Formate umzubauen.

use std::fs;
use std::path::Path;

use crate::error::{Error, Result};

/// Name der Versions-Datei im DB-Verzeichnis.
pub const VERSION_FILE: &str = "VERSION";

/// Version, die diese Binary schreibt (aktuell v1).
pub const FORMAT_VERSION: u32 = 1;

/// Kleinste Version, die diese Binary noch lesen kann.
pub const MIN_SUPPORTED_VERSION: u32 = 1;

/// Liest die `VERSION`-Datei.
///
/// - `Ok(Some(v))` – vorhanden und parsebar.
/// - `Ok(None)` – Datei fehlt (Legacy-v1-DB).
/// - `Err(InvalidFormat)` – vorhanden, aber nicht parsebar.
pub fn read_version(dir: &Path) -> Result<Option<u32>> {
    let path = dir.join(VERSION_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    parse_version(&text)
}

fn parse_version(text: &str) -> Result<Option<u32>> {
    let text = text.trim();
    let mut parts = text.split_whitespace();
    match parts.next() {
        Some("V") => {
            let v = parts
                .next()
                .ok_or_else(|| Error::InvalidFormat("VERSION: missing version number".into()))?
                .parse::<u32>()
                .map_err(|_| Error::InvalidFormat("VERSION: invalid version number".into()))?;
            if parts.next().is_some() {
                return Err(Error::InvalidFormat("VERSION: trailing tokens".into()));
            }
            Ok(Some(v))
        }
        _ => Err(Error::InvalidFormat("VERSION: missing 'V' marker".into())),
    }
}

/// Schreibt die `VERSION`-Datei atomar (tmp + rename).
pub fn write_version(dir: &Path, version: u32) -> Result<()> {
    let path = dir.join(VERSION_FILE);
    let tmp = dir.join("VERSION.tmp");
    fs::write(&tmp, format!("V {version}\n"))?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Prüft, ob eine gefundene Version von dieser Binary gelesen werden kann.
pub fn check_compatible(found: u32) -> Result<()> {
    if found < MIN_SUPPORTED_VERSION || found > FORMAT_VERSION {
        return Err(Error::UnsupportedFormatVersion {
            found,
            min_supported: MIN_SUPPORTED_VERSION,
            max_supported: FORMAT_VERSION,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ok_and_reject() {
        assert_eq!(parse_version("V 1").unwrap(), Some(1));
        assert_eq!(parse_version("V 1\n").unwrap(), Some(1));
        assert!(parse_version("X 1").is_err());
        assert!(parse_version("V").is_err());
        assert!(parse_version("V abc").is_err());
        assert!(parse_version("V 1 2").is_err());
        assert!(parse_version("garbage").is_err());
    }

    #[test]
    fn compatible_range() {
        assert!(check_compatible(1).is_ok());
        assert!(check_compatible(0).is_err());
        assert!(check_compatible(2).is_err());
    }
}
