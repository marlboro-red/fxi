use std::io::{self, Read, Write};

/// Encode a u32 as a variable-length integer
pub fn encode_varint(mut value: u32, buf: &mut Vec<u8>) {
    loop {
        if value < 0x80 {
            buf.push(value as u8);
            break;
        }
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
}

/// Decode a variable-length integer from a slice
/// Returns (value, bytes_consumed)
pub fn decode_varint(buf: &[u8]) -> Option<(u32, usize)> {
    let mut result: u32 = 0;
    let mut shift = 0;

    for (i, &byte) in buf.iter().enumerate() {
        if shift >= 32 {
            return None; // Overflow
        }

        result |= ((byte & 0x7F) as u32) << shift;

        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }

        shift += 7;
    }

    None // Incomplete
}

/// Encode a u64 as a variable-length integer
#[allow(dead_code)]
pub fn encode_varint_u64(mut value: u64, buf: &mut Vec<u8>) {
    loop {
        if value < 0x80 {
            buf.push(value as u8);
            break;
        }
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
}

/// Decode a u64 variable-length integer
#[allow(dead_code)]
pub fn decode_varint_u64(buf: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;

    for (i, &byte) in buf.iter().enumerate() {
        if shift >= 64 {
            return None;
        }

        result |= ((byte & 0x7F) as u64) << shift;

        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }

        shift += 7;
    }

    None
}

/// Delta-encode a sorted list of u32s
pub fn delta_encode(values: &[u32], buf: &mut Vec<u8>) {
    let mut prev = 0u32;
    for &value in values {
        let delta = value - prev;
        encode_varint(delta, buf);
        prev = value;
    }
}

/// Delta-decode a list of u32s.
///
/// OPTIMIZATION: Pre-allocates result vector based on estimated element count.
/// Delta-encoded u32s average ~2-3 bytes each, so we estimate capacity as buf.len()/2.
pub fn delta_decode(buf: &[u8]) -> Vec<u32> {
    // Estimate capacity: delta-encoded u32s average ~2-3 bytes each
    // Using buf.len()/2 provides a reasonable estimate that avoids most reallocations
    let estimated_capacity = buf.len() / 2;
    let mut result = Vec::with_capacity(estimated_capacity.max(8));
    let mut prev = 0u32;
    let mut pos = 0;

    while pos < buf.len() {
        if let Some((delta, consumed)) = decode_varint(&buf[pos..]) {
            prev = prev.saturating_add(delta);
            result.push(prev);
            pos += consumed;
        } else {
            break;
        }
    }

    result
}

/// Write a u32 in little-endian format
#[allow(dead_code)]
pub fn write_u32_le<W: Write>(writer: &mut W, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

/// Read a u32 in little-endian format
#[allow(dead_code)]
pub fn read_u32_le<R: Read>(reader: &mut R) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Write a u64 in little-endian format
#[allow(dead_code)]
pub fn write_u64_le<W: Write>(writer: &mut W, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

/// Read a u64 in little-endian format
#[allow(dead_code)]
pub fn read_u64_le<R: Read>(reader: &mut R) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

/// Write a u16 in little-endian format
#[allow(dead_code)]
pub fn write_u16_le<W: Write>(writer: &mut W, value: u16) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

/// Read a u16 in little-endian format
#[allow(dead_code)]
pub fn read_u16_le<R: Read>(reader: &mut R) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

/// Encode position postings: a list of (doc_id, positions) pairs.
/// Format: for each entry, delta-encoded doc_id followed by position count,
/// then delta-encoded positions within that document.
pub fn encode_position_postings(doc_positions: &[(u32, &[u32])], buf: &mut Vec<u8>) {
    let mut prev_doc_id = 0u32;
    for &(doc_id, positions) in doc_positions {
        // Delta-encode doc_id
        let delta = doc_id - prev_doc_id;
        encode_varint(delta, buf);
        prev_doc_id = doc_id;

        // Position count
        encode_varint(positions.len() as u32, buf);

        // Delta-encode positions
        let mut prev_pos = 0u32;
        for &pos in positions {
            let pos_delta = pos - prev_pos;
            encode_varint(pos_delta, buf);
            prev_pos = pos;
        }
    }
}

/// Decode position postings back to a list of (doc_id, positions) pairs.
pub fn decode_position_postings(buf: &[u8]) -> Vec<(u32, Vec<u32>)> {
    let mut result = Vec::new();
    let mut pos = 0;
    let mut prev_doc_id = 0u32;

    while pos < buf.len() {
        // Decode doc_id delta
        let (delta, consumed) = match decode_varint(&buf[pos..]) {
            Some(v) => v,
            None => break,
        };
        pos += consumed;
        prev_doc_id = prev_doc_id.saturating_add(delta);

        // Decode position count
        let (count, consumed) = match decode_varint(&buf[pos..]) {
            Some(v) => v,
            None => break,
        };
        pos += consumed;

        // Decode positions
        let mut positions = Vec::with_capacity(count as usize);
        let mut prev_pos = 0u32;
        for _ in 0..count {
            let (pos_delta, consumed) = match decode_varint(&buf[pos..]) {
                Some(v) => v,
                None => break,
            };
            pos += consumed;
            prev_pos = prev_pos.saturating_add(pos_delta);
            positions.push(prev_pos);
        }

        result.push((prev_doc_id, positions));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_varint_roundtrip() {
        let values = [0, 1, 127, 128, 16383, 16384, u32::MAX];
        for value in values {
            let mut buf = Vec::new();
            encode_varint(value, &mut buf);
            let (decoded, _) = decode_varint(&buf).unwrap();
            assert_eq!(value, decoded);
        }
    }

    #[test]
    fn test_delta_encoding() {
        let values = vec![1, 5, 10, 15, 100, 1000];
        let mut buf = Vec::new();
        delta_encode(&values, &mut buf);
        let decoded = delta_decode(&buf);
        assert_eq!(values, decoded);
    }

    #[test]
    fn test_position_postings_roundtrip() {
        let data: Vec<(u32, &[u32])> = vec![
            (1, &[0, 3, 7]),
            (5, &[2, 10]),
            (100, &[0]),
        ];

        let mut buf = Vec::new();
        encode_position_postings(&data, &mut buf);
        let decoded = decode_position_postings(&buf);

        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], (1, vec![0, 3, 7]));
        assert_eq!(decoded[1], (5, vec![2, 10]));
        assert_eq!(decoded[2], (100, vec![0]));
    }

    #[test]
    fn test_position_postings_empty() {
        let data: Vec<(u32, &[u32])> = vec![];
        let mut buf = Vec::new();
        encode_position_postings(&data, &mut buf);
        let decoded = decode_position_postings(&buf);
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_position_postings_single_doc() {
        let data: Vec<(u32, &[u32])> = vec![(42, &[0, 1, 2, 3, 100])];
        let mut buf = Vec::new();
        encode_position_postings(&data, &mut buf);
        let decoded = decode_position_postings(&buf);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], (42, vec![0, 1, 2, 3, 100]));
    }

    #[test]
    fn test_position_postings_consecutive_docs() {
        let data: Vec<(u32, &[u32])> = vec![
            (1, &[0]),
            (2, &[5]),
            (3, &[10, 20]),
        ];
        let mut buf = Vec::new();
        encode_position_postings(&data, &mut buf);
        let decoded = decode_position_postings(&buf);
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], (1, vec![0]));
        assert_eq!(decoded[1], (2, vec![5]));
        assert_eq!(decoded[2], (3, vec![10, 20]));
    }
}
