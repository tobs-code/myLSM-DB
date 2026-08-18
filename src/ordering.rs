//! Order-preserving Encoding für Index-Werte.
//!
//! Der Index-Key ist `I | collection | field | encoded_value | entity`.
//! Damit Range-Scans funktionieren, muss `encoded_value` die **numerische
//! Ordnung** des Werts exakt als Byte-Ordnung widerspiegeln:
//!
//! ```text
//! a < b   ⟺   encode(a) < encode(b)
//! ```
//!
//! Dazu muss das Encoding **selbst-delimitierend** sein (es folgt die
//! entity_id) und eine **totale Ordnung** besitzen. WICHTIG: Kein
//! Length-Präfix für String/Bytes — das bräche die lexikografische Ordnung
//! (z. B. `"aa"` vs `"b"`). Stattdessen null-freies Escaping mit Terminator.
//!
//! Die totale Ordnung für Float ist: `-∞ < … < -0 == +0 < … < +∞ < NaN`
//! (NaN ist deterministisch das Maximum, `-0.0` sortiert knapp vor `+0.0`).
//! Dieselbe Ordnung gilt in Encode, Range-Scan und Verifikation.

use crate::codec::{Value, TAG_BOOL, TAG_BYTES, TAG_FLOAT, TAG_INT, TAG_NULL, TAG_STRING};

/// Totale Ordnung zweier Werte, konsistent mit [`encode_ordered`].
/// NaN ist das Maximum; `-0.0` sortiert knapp vor `+0.0`.
pub fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn fkey(f: f64) -> (u8, u64) {
        if f.is_nan() {
            (2, 0)
        } else {
            (0, ordered_f64_bits(f))
        }
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => fkey(*x).cmp(&fkey(*y)),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        // Verschiedene Typen: über Tag (deterministisch, aber unüblich).
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) => 2,
        Value::Float(_) => 3,
        Value::String(_) => 4,
        Value::Bytes(_) => 5,
    }
}

/// Gibt die Byte-Länge eines ordnungserhaltend kodierten Werts an (Tag +
/// Payload), sodass dahinter liegende Bytes (z. B. die entity_id im
/// Index-Key) eindeutig lokalisiert werden können.
pub fn ordered_value_len(data: &[u8]) -> crate::error::Result<usize> {
    if data.is_empty() {
        return Err(crate::error::Error::InvalidFormat("empty ordered value".into()));
    }
    match data[0] {
        TAG_NULL => Ok(1),
        TAG_BOOL => {
            if data.len() < 2 {
                Err(crate::error::Error::InvalidFormat("bool ordered too short".into()))
            } else {
                Ok(2)
            }
        }
        TAG_INT | TAG_FLOAT => {
            if data.len() < 9 {
                Err(crate::error::Error::InvalidFormat("numeric ordered too short".into()))
            } else {
                Ok(9)
            }
        }
        TAG_STRING | TAG_BYTES => {
            let mut i = 1;
            while i < data.len() {
                if data[i] == 0 {
                    if i + 1 >= data.len() {
                        return Err(crate::error::Error::InvalidFormat("truncated terminator".into()));
                    }
                    if data[i + 1] == 0 {
                        return Ok(i + 2); // Terminator 0x00 0x00
                    } else if data[i + 1] == 1 {
                        i += 2; // escaptes Null-Byte 0x00 0x01
                        continue;
                    } else {
                        return Err(crate::error::Error::InvalidFormat("invalid escape".into()));
                    }
                }
                i += 1;
            }
            Err(crate::error::Error::InvalidFormat("missing terminator".into()))
        }
        t => Err(crate::error::Error::InvalidFormat(format!("unknown ordered tag {t}"))),
    }
}

/// Kodiert einen Wert ordnungserhaltend. Format: `[type tag][payload]`.
pub fn encode_ordered(v: &Value) -> Vec<u8> {
    match v {
        Value::Null => vec![TAG_NULL],
        Value::Bool(b) => vec![TAG_BOOL, if *b { 1 } else { 0 }],
        Value::Int(i) => {
            // Vorzeichen-Bit umklappen → Big-Endian: -MAX … 0 … +MAX, byte-sortiert.
            let enc = (*i as u64) ^ (1u64 << 63);
            let mut out = Vec::with_capacity(9);
            out.push(TAG_INT);
            out.extend_from_slice(&enc.to_be_bytes());
            out
        }
        Value::Float(f) => {
            let mut out = Vec::with_capacity(9);
            out.push(TAG_FLOAT);
            out.extend_from_slice(&ordered_f64_bits(*f).to_be_bytes());
            out
        }
        Value::String(s) => {
            let mut out = Vec::with_capacity(1 + s.len() + 2);
            out.push(TAG_STRING);
            escape_null_free(s.as_bytes(), &mut out);
            out
        }
        Value::Bytes(b) => {
            let mut out = Vec::with_capacity(1 + b.len() + 2);
            out.push(TAG_BYTES);
            escape_null_free(b, &mut out);
            out
        }
    }
}

/// Wandelt ein `f64` in ein total geordnetes 64-Bit-Pattern um.
fn ordered_f64_bits(v: f64) -> u64 {
    if v.is_nan() {
        // NaN = deterministisches Maximum (über +∞).
        return u64::MAX;
    }
    let bits = v.to_bits();
    if bits >> 63 == 1 {
        // Negativ (Vorzeichenbit gesetzt): alle Bits umklappen.
        !bits
    } else {
        // Positiv/Null: Vorzeichenbit umklappen.
        bits ^ (1 << 63)
    }
}

/// Kodiert Bytes null-frei (0x00 → 0x00 0x01) mit Terminator 0x00 0x00.
/// Ordnungserhaltend und selbst-delimitierend.
fn escape_null_free(data: &[u8], out: &mut Vec<u8>) {
    for &b in data {
        if b == 0 {
            out.push(0);
            out.push(1);
        } else {
            out.push(b);
        }
    }
    out.push(0);
    out.push(0);
}

/// Dekodiert einen ordnungserhaltend kodierten Wert (für Tests).
pub fn decode_ordered(data: &[u8]) -> crate::error::Result<Value> {
    if data.is_empty() {
        return Err(crate::error::Error::InvalidFormat("empty ordered value".into()));
    }
    let tag = data[0];
    let payload = &data[1..];
    match tag {
        TAG_NULL => Ok(Value::Null),
        TAG_BOOL => {
            if payload.is_empty() {
                return Err(crate::error::Error::InvalidFormat("bool ordered too short".into()));
            }
            Ok(Value::Bool(payload[0] != 0))
        }
        TAG_INT => {
            if payload.len() < 8 {
                return Err(crate::error::Error::InvalidFormat("int ordered too short".into()));
            }
            let bits = u64::from_be_bytes(payload[0..8].try_into().unwrap());
            Ok(Value::Int((bits ^ (1u64 << 63)) as i64))
        }
        TAG_FLOAT => {
            if payload.len() < 8 {
                return Err(crate::error::Error::InvalidFormat("float ordered too short".into()));
            }
            let bits = u64::from_be_bytes(payload[0..8].try_into().unwrap());
            if bits == u64::MAX {
                return Ok(Value::Float(f64::NAN));
            }
            let bits = if bits >> 63 == 1 { bits ^ (1 << 63) } else { !bits };
            Ok(Value::Float(f64::from_bits(bits)))
        }
        TAG_STRING => {
            let raw = unescape_null_free(payload)?;
            let s = std::str::from_utf8(&raw)
                .map_err(|_| crate::error::Error::InvalidFormat("ordered string not utf8".into()))?;
            Ok(Value::String(s.to_string()))
        }
        TAG_BYTES => Ok(Value::Bytes(unescape_null_free(payload)?)),
        t => Err(crate::error::Error::InvalidFormat(format!("unknown ordered tag {t}"))),
    }
}

fn unescape_null_free(data: &[u8]) -> crate::error::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0 {
            // Terminator 0x00 0x00, danach darf nichts mehr kommen.
            if i + 1 >= data.len() {
                return Err(crate::error::Error::InvalidFormat("truncated terminator".into()));
            }
            if data[i + 1] == 0 {
                if i + 2 != data.len() {
                    return Err(crate::error::Error::InvalidFormat("trailing bytes after terminator".into()));
                }
                return Ok(out);
            } else if data[i + 1] == 1 {
                out.push(0);
                i += 2;
                continue;
            } else {
                return Err(crate::error::Error::InvalidFormat("invalid escape".into()));
            }
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    Err(crate::error::Error::InvalidFormat("missing terminator".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministischer xorshift-PRNG (kein externer Rand-Dependency).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn next_i64(&mut self) -> i64 {
            self.next() as i64
        }
        fn next_f64(&mut self) -> f64 {
            // Beliebiges Bitmuster (auch NaN, ±Inf, subnormal, -0).
            f64::from_bits(self.next())
        }
        fn next_bytes(&mut self, max_len: usize) -> Vec<u8> {
            let len = (self.next() % (max_len as u64 + 1)) as usize;
            (0..len).map(|_| self.next() as u8).collect()
        }
        fn next_bool(&mut self) -> bool {
            self.next() & 1 == 1
        }
    }

    #[test]
    fn int64_order_preserved() {
        let mut rng = Rng(0xC0FFEE);
        let mut vals: Vec<i64> = (0..2000).map(|_| rng.next_i64()).collect();
        vals.push(i64::MIN);
        vals.push(i64::MAX);
        vals.push(0);
        vals.push(-1);
        vals.push(1);
        vals.sort();
        for w in vals.windows(2) {
            let a = encode_ordered(&Value::Int(w[0]));
            let b = encode_ordered(&Value::Int(w[1]));
            assert!(a < b, "{:?} !< {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn string_order_preserved() {
        let mut rng = Rng(0xBEAD);
        let mut vals: Vec<String> = (0..2000).map(|_| {
            let bytes = rng.next_bytes(12);
            String::from_utf8_lossy(&bytes).into_owned()
        })
        .collect();
        // Bewusst auch Fälle mit Nullen und Präfix-Beziehungen.
        vals.push("".into());
        vals.push("a".into());
        vals.push("aa".into());
        vals.push("b".into());
        vals.push("ba".into());
        vals.push("a\0b".into());
        vals.push("\0".into());
        vals.push("\0\0".into());
        vals.sort();
        vals.dedup();
        for w in vals.windows(2) {
            let a = encode_ordered(&Value::String(w[0].clone()));
            let b = encode_ordered(&Value::String(w[1].clone()));
            assert!(a < b, "{:?} !< {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn bytes_order_preserved() {
        let mut rng = Rng(0xBAD0);
        let mut vals: Vec<Vec<u8>> = (0..2000).map(|_| rng.next_bytes(12)).collect();
        vals.push(vec![]);
        vals.push(vec![0]);
        vals.push(vec![0, 0]);
        vals.push(vec![1]);
        vals.push(vec![0, 1]);
        vals.sort();
        vals.dedup();
        for w in vals.windows(2) {
            let a = encode_ordered(&Value::Bytes(w[0].clone()));
            let b = encode_ordered(&Value::Bytes(w[1].clone()));
            assert!(a < b, "{:?} !< {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn float_total_order() {
        // Explizite erwartete Ordnung inkl. NaN, ±Inf, ±0, subnormal.
        let ordered = [
            f64::NEG_INFINITY,
            -1e300,
            -3.0,
            -0.0,
            0.0,
            3.0,
            1e300,
            f64::INFINITY,
            f64::NAN,
        ];
        for w in ordered.windows(2) {
            let a = encode_ordered(&Value::Float(w[0]));
            let b = encode_ordered(&Value::Float(w[1]));
            assert!(a < b, "{:?} !< {:?}", w[0], w[1]);
        }
        let mut rng = Rng(0xF10A7);
        let mut vals: Vec<f64> = (0..2000).map(|_| rng.next_f64()).collect();
        vals.sort_by(|x, y| value_cmp(&Value::Float(*x), &Value::Float(*y)));
        // Alle NaN kollabieren auf einen Wert → dedup (nur strikte Ordnung prüfen).
        let mut seen: Vec<f64> = Vec::new();
        for v in vals {
            let dup = seen.last().map_or(false, |l| {
                ordered_f64_bits(*l) == ordered_f64_bits(v)
            });
            if !dup {
                seen.push(v);
            }
        }
        for w in seen.windows(2) {
            let a = encode_ordered(&Value::Float(w[0]));
            let b = encode_ordered(&Value::Float(w[1]));
            assert!(a < b, "{:?} !< {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn injective_and_roundtrip() {
        let mut rng = Rng(0xABCD);
        for _ in 0..3000 {
            let v = match rng.next() % 6 {
                0 => Value::Null,
                1 => Value::Bool(rng.next_bool()),
                2 => Value::Int(rng.next_i64()),
                3 => Value::Float(rng.next_f64()),
                4 => Value::String(String::from_utf8_lossy(&rng.next_bytes(10)).into_owned()),
                _ => Value::Bytes(rng.next_bytes(10)),
            };
            let enc = encode_ordered(&v);
            let dec = decode_ordered(&enc).unwrap();
            assert_eq!(dec, v, "roundtrip failed for {v:?}");
        }
    }

    #[test]
    fn sort_stability_by_encoded() {
        // Der wertvollste Test: Sortieren per Byte-Encoding == Sortieren per Ordnung.
        let mut rng = Rng(0x5EED);
        let vals: Vec<Value> = (0..1000).map(|_| Value::Int(rng.next_i64())).collect();
        let mut by_cmp = vals.clone();
        by_cmp.sort_by(value_cmp);
        let mut by_enc: Vec<&Value> = vals.iter().collect();
        by_enc.sort_by(|a, b| encode_ordered(a).cmp(&encode_ordered(b)));
        for (i, v) in by_enc.iter().enumerate() {
            assert_eq!(*v, &by_cmp[i], "sort mismatch at {i}");
        }
    }

    #[test]
    fn string_escapes_terminator_correctly() {
        // Leerer String ist kleiner als "a" (Terminator 00 00 < 61 ...).
        assert!(encode_ordered(&Value::String(String::new()))
            < encode_ordered(&Value::String("a".into())));
        // "a" ist Präfix von "a\0b".
        assert!(encode_ordered(&Value::String("a".into()))
            < encode_ordered(&Value::String("a\0b".into())));
    }
}