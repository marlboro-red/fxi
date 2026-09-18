//! Experimental checked dictionary / query-local posting validation.
//!
//! Checksums detect accidental damage, not coherent rewriting by an untrusted
//! issuer. Metadata keeps its existing structural checks. Published mappings
//! must remain immutable for the reader's lifetime, as with the ordinary reader.
use super::reader::MappedBytes;
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use xxhash_rust::xxh3::xxh3_64;

const NAME: &str = "grams.checks";
const MAGIC: &[u8; 8] = b"FXIGRAM1";
const HEADER: usize = 40;

pub(crate) fn requested() -> bool {
    std::env::var_os("FXI_QUERY_LOCAL").is_some_and(|v| v == "1")
}

pub(crate) struct PostingChecks {
    bytes: MappedBytes,
    // Only successful checks are cached. Invalid payloads stay errors; concurrent
    // first readers may duplicate validation rather than synchronizing a lock.
    validated: Vec<AtomicU8>,
}

impl PostingChecks {
    pub(crate) fn open(
        path: &Path,
        dictionary: &[u8],
        posting_len: usize,
        count: usize,
    ) -> Result<Option<Self>> {
        let name = path.join(NAME);
        let bytes = match MappedBytes::open(&name) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        anyhow::ensure!(
            bytes.len() >= HEADER && &bytes[..8] == MAGIC,
            "Invalid gram checks header"
        );
        let read = |at| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        anyhow::ensure!(
            read(8) == xxh3_64(&bytes[16..]),
            "Gram checks checksum mismatch"
        );
        anyhow::ensure!(
            read(16) == xxh3_64(dictionary),
            "Gram dictionary checksum mismatch"
        );
        anyhow::ensure!(
            read(24) == posting_len as u64,
            "Gram posting length mismatch"
        );
        anyhow::ensure!(
            read(32) == count as u64
                && (bytes.len() - HEADER) / 8 == count
                && (bytes.len() - HEADER).is_multiple_of(8),
            "Gram checks count mismatch"
        );
        Ok(Some(Self {
            bytes,
            validated: (0..count).map(|_| AtomicU8::new(0)).collect(),
        }))
    }

    pub(crate) fn validate(
        &self,
        index: usize,
        bytes: &[u8],
        frequency: u32,
        validator: &crate::utils::encoding::DocumentPostingsValidator<'_>,
    ) -> Result<()> {
        if self.validated[index].load(Ordering::Acquire) != 0 {
            return Ok(());
        }
        let at = HEADER + index * 8;
        let expected = u64::from_le_bytes(self.bytes[at..at + 8].try_into().unwrap());
        anyhow::ensure!(xxh3_64(bytes) == expected, "Gram posting checksum mismatch");
        validator.validate(bytes, frequency)?;
        self.validated[index].store(1, Ordering::Release);
        Ok(())
    }
}

/// Called only on a fully validated staging segment. Never rewrite inherited
/// evidence: a malformed existing sidecar must fail validation, not be blessed.
pub(crate) fn write(path: &Path, dictionary: &[u8], postings: &[u8]) -> Result<()> {
    if path.join(NAME).try_exists()? {
        return Ok(());
    }
    let count = u32::from_le_bytes(dictionary[..4].try_into().unwrap()) as usize;
    let mut bytes = Vec::with_capacity(HEADER + count * 8);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&xxh3_64(dictionary).to_le_bytes());
    bytes.extend_from_slice(&(postings.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&(count as u64).to_le_bytes());
    for record in dictionary[4..].as_chunks::<20>().0 {
        let start = usize::try_from(u64::from_le_bytes(record[4..12].try_into().unwrap()))?;
        let length = u32::from_le_bytes(record[12..16].try_into().unwrap()) as usize;
        let end = start.checked_add(length).context("Gram range overflow")?;
        let payload = postings.get(start..end).context("Invalid gram range")?;
        bytes.extend_from_slice(&xxh3_64(payload).to_le_bytes());
    }
    let checksum = xxh3_64(&bytes[16..]);
    bytes[8..16].copy_from_slice(&checksum.to_le_bytes());
    std::fs::write(path.join(NAME), bytes)?;
    Ok(())
}
