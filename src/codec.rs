//! Typed Value Codec — ein eindeutiges, versionierbares Binärformat für
//! Entitäts-Werte. Format: `[type tag u8][payload]`.
//!
//! Unterstützte Typen (v0.2): Null, Bool, Int64, Float64, String, Bytes.
//! Später erweiterbar um Timestamp, UUID, Array, Object.

use crate::error::{Error, Result};

/// Ein getypter Wert, wie er in einer Entität gespeichert wird.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// 64-Bit Ganzzahl (LE).
    Int(i64),
    /// 64-Bit Fließkommazahl (IEEE-754-Bits, LE).
    Float(f64),
    /// UTF-8-String.
    String(String),
    /// Rohe Bytes.
    Bytes(Vec<u8>),
}

/// Type-Tags (versioniert über den Tag-Wert).
pub const TAG_NULL: u8 = 0;
pub const TAG_BOOL: u8 = 1;
pub const TAG_INT: u8 = 2;
pub const TAG_FLOAT: u8 = 3;
pub const TAG_STRING: u8 = 4;
pub const TAG_BYTES: u8 = 5;

/// Kodiert einen Wert in sein Binärformat.
pub fn encode(v: &Value) -> Vec<u8> {
    match v {
        Value::Null => vec![TAG_NULL],
        Value::Bool(b) => vec![TAG_BOOL, if *b { 1 } else { 0 }],
        Value::Int(i) => {
            let mut out = Vec::with_capacity(9);
            out.push(TAG_INT);
            out.extend_from_slice(&i.to_le_bytes());
            out
        }
        Value::Float(f) => {
            let mut out = Vec::with_capacity(9);
            out.push(TAG_FLOAT);
            out.extend_from_slice(&f.to_bits().to_le_bytes());
            out
        }
        Value::String(s) => {
            let bytes = s.as_bytes();
            let mut out = Vec::with_capacity(5 + bytes.len());
            out.push(TAG_STRING);
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
            out
        }
        Value::Bytes(b) => {
            let mut out = Vec::with_capacity(5 + b.len());
            out.push(TAG_BYTES);
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
            out
        }
    }
}

/// Dekodiert einen Wert aus seinem Binärformat.
pub fn decode(data: &[u8]) -> Result<Value> {
    if data.is_empty() {
        return Err(Error::InvalidFormat("empty value".into()));
    }
    match data[0] {
        TAG_NULL => Ok(Value::Null),
        TAG_BOOL => {
            if data.len() < 2 {
                return Err(Error::InvalidFormat("bool value too short".into()));
            }
            Ok(Value::Bool(data[1] != 0))
        }
        TAG_INT => {
            if data.len() < 9 {
                return Err(Error::InvalidFormat("int value too short".into()));
            }
            let bytes: [u8; 8] = data[1..9].try_into().unwrap();
            Ok(Value::Int(i64::from_le_bytes(bytes)))
        }
        TAG_FLOAT => {
            if data.len() < 9 {
                return Err(Error::InvalidFormat("float value too short".into()));
            }
            let bytes: [u8; 8] = data[1..9].try_into().unwrap();
            Ok(Value::Float(f64::from_bits(u64::from_le_bytes(bytes))))
        }
        TAG_STRING => {
            let (len, s) = read_len_prefix(data)?;
            let bytes = s
                .get(..len)
                .ok_or_else(|| Error::InvalidFormat("string too short".into()))?;
            let text = std::str::from_utf8(bytes)
                .map_err(|_| Error::InvalidFormat("invalid utf8 string".into()))?;
            Ok(Value::String(text.to_string()))
        }
        TAG_BYTES => {
            let (len, s) = read_len_prefix(data)?;
            let bytes = s
                .get(..len)
                .ok_or_else(|| Error::InvalidFormat("bytes too short".into()))?;
            Ok(Value::Bytes(bytes.to_vec()))
        }
        other => Err(Error::InvalidFormat(format!("unknown value tag {other}"))),
    }
}

/// Liest das u32-Längen-Präfix ab Byte 1 und liefert (länge, rest ab Byte 5).
fn read_len_prefix(data: &[u8]) -> Result<(usize, &[u8])> {
    if data.len() < 5 {
        return Err(Error::InvalidFormat("len-prefixed value too short".into()));
    }
    let len_bytes: [u8; 4] = data[1..5].try_into().unwrap();
    let len = u32::from_le_bytes(len_bytes) as usize;
    Ok((len, &data[5..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: Value) {
        let enc = encode(&v);
        let dec = decode(&enc).unwrap();
        assert_eq!(dec, v);
    }

    #[test]
    fn roundtrips_all_types() {
        roundtrip(Value::Null);
        roundtrip(Value::Bool(true));
        roundtrip(Value::Bool(false));
        roundtrip(Value::Int(0));
        roundtrip(Value::Int(-42));
        roundtrip(Value::Int(i64::MIN));
        roundtrip(Value::Int(i64::MAX));
        roundtrip(Value::Float(0.0));
        roundtrip(Value::Float(-3.5));
        roundtrip(Value::Float(f64::NEG_INFINITY));
        roundtrip(Value::String(String::new()));
        roundtrip(Value::String("hello".into()));
        roundtrip(Value::String("hällo wörld ünïcode 🚀".into()));
        roundtrip(Value::Bytes(Vec::new()));
        roundtrip(Value::Bytes(vec![0, 1, 2, 255]));
    }

    #[test]
    fn decode_errors_on_bad_input() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[TAG_BOOL]).is_err()); // fehlendes Payload
        assert!(decode(&[99]).is_err()); // unbekannter Tag
        assert!(decode(&[TAG_STRING, 100, 0, 0, 0]).is_err()); // Länge > Daten
        // ungültiges UTF-8
        let bad = encode(&Value::Bytes(vec![0xFF, 0xFE]));
        // Bytes mit Tag überschreiben, um ungültige String-Daten zu simulieren
        let mut enc = encode(&Value::String("x".into()));
        enc[5] = 0xFF; // kaputtes UTF-8
        assert!(decode(&enc).is_err());
        assert!(decode(&bad).is_ok());
    }

    #[test]
    fn tags_are_stable() {
        assert_eq!(encode(&Value::Null), vec![0]);
        assert_eq!(encode(&Value::Bool(true)), vec![1, 1]);
        assert_eq!(encode(&Value::String(String::new())), vec![4, 0, 0, 0, 0]);
    }
}
