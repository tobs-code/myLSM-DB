//! Binäres Key-Encoding für Entity-Keys. Kein String-Kleben — ein echtes,
//! length-/type-sicheres Format, damit Keys auch bei Sonderzeichen in IDs
//! eindeutig bleiben.
//!
//! Layout eines Entity-Keys:
//! ```text
//! [0x45 'E'] [collection_id u32 LE] [entity_id_len u32 LE] [entity_id] [field_id u32 LE]
//! ```

/// Namespace-Tag für Entity-Keys.
pub const ENTITY_TAG: u8 = b'E'; // 0x45

/// Kodiert einen Entity-Feld-Schlüssel.
pub fn encode_entity_key(collection_id: u32, entity_id: &[u8], field_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 + 4 + entity_id.len() + 4);
    out.push(ENTITY_TAG);
    out.extend_from_slice(&collection_id.to_le_bytes());
    out.extend_from_slice(&(entity_id.len() as u32).to_le_bytes());
    out.extend_from_slice(entity_id);
    out.extend_from_slice(&field_id.to_le_bytes());
    out
}

/// Dekodiert einen Entity-Key in (collection_id, entity_id, field_id).
/// Gibt `None` zurück, wenn der Key kein gültiger Entity-Key ist.
pub fn decode_entity_key(key: &[u8]) -> Option<(u32, &[u8], u32)> {
    if key.len() < 1 + 4 + 4 + 4 || key[0] != ENTITY_TAG {
        return None;
    }
    let mut off = 1;
    let collection_id = u32::from_le_bytes(key[off..off + 4].try_into().unwrap());
    off += 4;
    let entity_len = u32::from_le_bytes(key[off..off + 4].try_into().unwrap()) as usize;
    off += 4;
    if off + entity_len + 4 > key.len() {
        return None;
    }
    let entity_id = &key[off..off + entity_len];
    off += entity_len;
    let field_id = u32::from_le_bytes(key[off..off + 4].try_into().unwrap());
    Some((collection_id, entity_id, field_id))
}

/// Liefert das gemeinsame Präfix aller Feld-Keys einer Entity.
pub fn entity_prefix(collection_id: u32, entity_id: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 + 4 + entity_id.len());
    out.push(ENTITY_TAG);
    out.extend_from_slice(&collection_id.to_le_bytes());
    out.extend_from_slice(&(entity_id.len() as u32).to_le_bytes());
    out.extend_from_slice(entity_id);
    out
}

/// Liefert den Bereich [start, end) aller Feld-Keys einer Entity.
/// `end = None` bedeutet "bis zum Ende" (wenn das Präfix vollständig 0xFF ist).
pub fn entity_range(collection_id: u32, entity_id: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let start = entity_prefix(collection_id, entity_id);
    let end = successor(&start);
    (start, end)
}

/// Liefert den lexikografischen Nachfolger eines Präfixes: die kleinste
/// Bytefolge, die größer als alle mit `prefix` beginnenden Keys ist.
/// `None`, wenn alle Bytes 0xFF sind (dann gibt es keinen Nachfolger).
fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = prefix.to_vec();
    let mut i = p.len();
    while i > 0 {
        i -= 1;
        if p[i] != 0xFF {
            p[i] += 1;
            p.truncate(i + 1);
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        for collection in [0u32, 1, 7, 4294967295] {
            for entity in ["123", "usr_8f31", "a|b|c", "", "höllo", "\u{1}\u{2}\u{FF}"] {
                for field in [0u32, 1, 100, 4294967295] {
                    let key = encode_entity_key(collection, entity.as_bytes(), field);
                    let (c, e, f) = decode_entity_key(&key).unwrap();
                    assert_eq!(c, collection);
                    assert_eq!(e, entity.as_bytes());
                    assert_eq!(f, field);
                }
            }
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        assert_eq!(decode_entity_key(&[]), None);
        assert_eq!(decode_entity_key(&[0x46]), None); // falscher Tag 'F'
        // zu kurz
        assert_eq!(decode_entity_key(&[ENTITY_TAG, 1, 0, 0, 0]), None);
    }

    #[test]
    fn range_covers_all_fields_and_not_other_entities() {
        // Entity "123"
        let (start, end) = entity_range(1, b"123");
        let end = end.expect("should have successor");
        // Feld-Keys von "123"
        for f in [1u32, 2, 3] {
            let k = encode_entity_key(1, b"123", f);
            assert!(k.as_slice() >= start.as_slice() && k.as_slice() < end.as_slice(), "field {f} in range");
        }
        // Andere Entities müssen außerhalb liegen.
        let other1 = encode_entity_key(1, b"122", 1);
        let other2 = encode_entity_key(1, b"124", 1);
        let other_col = encode_entity_key(2, b"123", 1);
        assert!(other1.as_slice() < start.as_slice());
        assert!(other2.as_slice() >= end.as_slice());
        assert!(other_col.as_slice() >= end.as_slice());
    }

    #[test]
    fn byte_sorting_respects_collection() {
        let k1 = encode_entity_key(1, b"x", 1);
        let k2 = encode_entity_key(2, b"x", 1);
        assert!(k1 < k2);
    }

    #[test]
    fn successor_basics() {
        assert_eq!(successor(b"abc"), Some(b"abd".to_vec()));
        assert_eq!(successor(b"ab\xFF"), Some(b"ac".to_vec()));
        assert_eq!(successor(b"\xFF"), None);
        assert_eq!(successor(b""), None);
    }
}