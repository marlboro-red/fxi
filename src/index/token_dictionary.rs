//! Lossless token dictionary records. Token strings remain borrowed from mmap.
//! Legacy fixed-width records and compact varint metadata share this decoder.
use crate::utils::{decode_varint, decode_varint_u64};
use anyhow::{Context, Result, ensure};
use std::io::Write;
const MAGIC: &[u8; 8] = b"\xff\xff\xff\xffFXT1";
#[derive(Clone, Copy)]
pub(crate) struct Header {
    pub count: usize,
    pub start: usize,
    pub compact: bool,
}
#[derive(Clone, Copy)]
pub(crate) struct Entry<'a> {
    pub token: &'a str,
    pub offset: u64,
    pub length: u32,
    pub doc_freq: u32,
    pub pos_offset: u64,
    pub pos_length: u32,
}
pub(crate) fn header(bytes: &[u8], positions: bool) -> Result<Header> {
    ensure!(bytes.len() >= 4, "Truncated token dictionary header");
    let compact = bytes[..4] == MAGIC[..4];
    let (start, count) = if compact {
        ensure!(
            bytes.len() >= 12 && &bytes[..8] == MAGIC,
            "Unsupported token dictionary format"
        );
        (
            12,
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize,
        )
    } else {
        (
            4,
            u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize,
        )
    };
    let minimum = if compact {
        if positions { 7 } else { 5 }
    } else if positions {
        30
    } else {
        18
    };
    ensure!(
        count <= (bytes.len() - start) / minimum,
        "Token dictionary count exceeds file bounds"
    );
    Ok(Header {
        count,
        start,
        compact,
    })
}
pub(crate) fn token(bytes: &[u8]) -> Result<(&str, usize)> {
    ensure!(bytes.len() >= 2, "Truncated token length");
    let length = u16::from_le_bytes(bytes[..2].try_into().unwrap()) as usize;
    let end = 2 + length;
    ensure!(end <= bytes.len(), "Truncated token string");
    Ok((
        std::str::from_utf8(&bytes[2..end]).context("Invalid token UTF-8")?,
        end,
    ))
}
pub(crate) fn entry(bytes: &[u8], compact: bool, positions: bool) -> Result<(Entry<'_>, usize)> {
    let (token, mut cursor) = token(bytes)?;
    let mut next = |wide: bool| -> Result<u64> {
        let (value, size) = if compact {
            if wide {
                decode_varint_u64(&bytes[cursor..])
            } else {
                decode_varint(&bytes[cursor..]).map(|(v, n)| (u64::from(v), n))
            }
            .context("Malformed token dictionary metadata")?
        } else {
            let size = if wide { 8 } else { 4 };
            ensure!(
                size <= bytes.len() - cursor,
                "Truncated token dictionary metadata"
            );
            let slice = &bytes[cursor..cursor + size];
            let value = if wide {
                u64::from_le_bytes(slice.try_into().unwrap())
            } else {
                u64::from(u32::from_le_bytes(slice.try_into().unwrap()))
            };
            (value, size)
        };
        cursor += size;
        Ok(value)
    };
    let offset = next(true)?;
    let length = next(false)? as u32;
    let doc_freq = next(false)? as u32;
    let (pos_offset, pos_length) = if positions {
        (next(true)?, next(false)? as u32)
    } else {
        (0, 0)
    };
    Ok((
        Entry {
            token,
            offset,
            length,
            doc_freq,
            pos_offset,
            pos_length,
        },
        cursor,
    ))
}
pub(crate) fn write_header(writer: &mut impl Write, count: usize) -> Result<()> {
    writer.write_all(MAGIC)?;
    writer.write_all(
        &u32::try_from(count)
            .context("Too many token dictionary entries")?
            .to_le_bytes(),
    )?;
    Ok(())
}
pub(crate) fn write_entry(
    writer: &mut impl Write,
    entry: Entry<'_>,
    positions: bool,
) -> Result<()> {
    writer.write_all(
        &u16::try_from(entry.token.len())
            .context("Token exceeds format limit")?
            .to_le_bytes(),
    )?;
    writer.write_all(entry.token.as_bytes())?;
    let mut encoded = [0u8; 35];
    let mut length = 0;
    let fields = [
        entry.offset,
        u64::from(entry.length),
        u64::from(entry.doc_freq),
        entry.pos_offset,
        u64::from(entry.pos_length),
    ];
    for mut value in fields.into_iter().take(if positions { 5 } else { 3 }) {
        while value >= 128 {
            encoded[length] = (value as u8) | 128;
            length += 1;
            value >>= 7;
        }
        encoded[length] = value as u8;
        length += 1;
    }
    writer.write_all(&encoded[..length])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_and_legacy_entries_preserve_every_field() {
        for positions in [false, true] {
            for value in [0, 1, 127, 128, 16383, 16384, u32::MAX as u64, u64::MAX] {
                let original = Entry {
                    token: "café_token",
                    offset: value,
                    length: value as u32,
                    doc_freq: value as u32,
                    pos_offset: value,
                    pos_length: value as u32,
                };
                let mut bytes = Vec::new();
                write_header(&mut bytes, 1).unwrap();
                write_entry(&mut bytes, original, positions).unwrap();
                let h = header(&bytes, positions).unwrap();
                assert!(h.compact);
                assert_eq!(h.count, 1);
                let (decoded, consumed) = entry(&bytes[h.start..], true, positions).unwrap();
                assert_eq!(consumed + h.start, bytes.len());
                assert_eq!(
                    (
                        decoded.token,
                        decoded.offset,
                        decoded.length,
                        decoded.doc_freq
                    ),
                    (
                        original.token,
                        original.offset,
                        original.length,
                        original.doc_freq
                    )
                );
                assert_eq!(
                    (decoded.pos_offset, decoded.pos_length),
                    if positions {
                        (value, value as u32)
                    } else {
                        (0, 0)
                    }
                );
                for end in h.start..bytes.len() {
                    assert!(entry(&bytes[h.start..end], true, positions).is_err());
                }
                let mut legacy = 1u32.to_le_bytes().to_vec();
                legacy.extend_from_slice(&(original.token.len() as u16).to_le_bytes());
                legacy.extend_from_slice(original.token.as_bytes());
                legacy.extend_from_slice(&original.offset.to_le_bytes());
                legacy.extend_from_slice(&original.length.to_le_bytes());
                legacy.extend_from_slice(&original.doc_freq.to_le_bytes());
                if positions {
                    legacy.extend_from_slice(&original.pos_offset.to_le_bytes());
                    legacy.extend_from_slice(&original.pos_length.to_le_bytes());
                }
                assert!(!header(&legacy, positions).unwrap().compact);
                let (decoded, consumed) = entry(&legacy[4..], false, positions).unwrap();
                assert_eq!(consumed + 4, legacy.len());
                assert_eq!(decoded.offset, original.offset);
                assert_eq!(decoded.token, original.token);
            }
        }
    }
    #[test]
    fn malformed_compact_headers_and_metadata_are_rejected() {
        for bytes in [
            vec![],
            vec![255; 4],
            b"\xff\xff\xff\xffFXT9\0\0\0\0".to_vec(),
            b"\xff\xff\xff\xffFXT1\xff\xff\xff\xff".to_vec(),
        ] {
            assert!(header(&bytes, true).is_err());
        }
        let mut bytes = vec![1, 0, b'a'];
        bytes.extend_from_slice(&[255; 9]);
        bytes.push(2);
        bytes.extend_from_slice(&[1; 4]);
        assert!(entry(&bytes, true, true).is_err(), "u64 overflow");
        let bytes = [1, 0, 255, 0, 0, 0, 0, 0];
        assert!(entry(&bytes, true, true).is_err(), "invalid UTF8");
    }
}
