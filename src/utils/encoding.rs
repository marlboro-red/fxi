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

/// Delta-decode a list of u32s
pub fn delta_decode(buf: &[u8]) -> Vec<u32> {
    let mut result = Vec::new();
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
pub fn write_u32_le<W: Write>(writer: &mut W, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

/// Read a u32 in little-endian format
pub fn read_u32_le<R: Read>(reader: &mut R) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Write a u64 in little-endian format
pub fn write_u64_le<W: Write>(writer: &mut W, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

/// Read a u64 in little-endian format
pub fn read_u64_le<R: Read>(reader: &mut R) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

/// Write a u16 in little-endian format
pub fn write_u16_le<W: Write>(writer: &mut W, value: u16) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

/// Read a u16 in little-endian format
pub fn read_u16_le<R: Read>(reader: &mut R) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
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
}
