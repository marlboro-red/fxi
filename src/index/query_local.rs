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
const PAGED_MAGIC: &[u8; 8] = b"FXIGRAM2";
const PAGE_ENTRIES: usize = 512;
const ROOT_HEADER: usize = 32;
const PAGE_RECORD: usize = 24;

struct Page {
    first: u32,
    last: u32,
    dictionary_hash: u64,
    postings_hash: u64,
    validated: AtomicU8,
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
const HEADER: usize = 40;

pub(crate) fn requested() -> bool {
    std::env::var_os("FXI_QUERY_LOCAL").is_some_and(|v| v == "1")
}

pub(crate) struct PostingChecks {
    bytes: MappedBytes,
    hashes_offset: usize,
    pages: Option<Vec<Page>>,
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
        if bytes.len() >= 8 && &bytes[..8] == PAGED_MAGIC {
            return Self::open_paged(bytes, posting_len, count).map(Some);
        }
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
            hashes_offset: HEADER,
            pages: None,
            validated: (0..count).map(|_| AtomicU8::new(0)).collect(),
        }))
    }

    fn open_paged(bytes: MappedBytes, posting_len: usize, count: usize) -> Result<Self> {
        let page_count = count.div_ceil(PAGE_ENTRIES);
        let root_end = page_count
            .checked_mul(PAGE_RECORD)
            .and_then(|n| n.checked_add(ROOT_HEADER))
            .context("Gram root size overflow")?;
        let expected_len = count
            .checked_mul(8)
            .and_then(|n| n.checked_add(root_end))
            .context("Gram checks size overflow")?;
        anyhow::ensure!(bytes.len() == expected_len, "Gram checks size mismatch");
        anyhow::ensure!(
            u64_at(&bytes, 8) == xxh3_64(&bytes[16..root_end]),
            "Gram root checksum mismatch"
        );
        anyhow::ensure!(
            u64_at(&bytes, 16) == posting_len as u64 && u64_at(&bytes, 24) == count as u64,
            "Gram checks length/count mismatch"
        );
        let mut pages: Vec<Page> = Vec::with_capacity(page_count);
        for entry in bytes[ROOT_HEADER..root_end].as_chunks::<PAGE_RECORD>().0 {
            let first = u32_at(entry, 0);
            let last = u32_at(entry, 4);
            anyhow::ensure!(
                first <= last && last <= 0x00ff_ffff && pages.last().is_none_or(|p| p.last < first),
                "Invalid gram page directory"
            );
            pages.push(Page {
                first,
                last,
                dictionary_hash: u64_at(entry, 8),
                postings_hash: u64_at(entry, 16),
                validated: AtomicU8::new(0),
            });
        }
        Ok(Self {
            bytes,
            hashes_offset: root_end,
            pages: Some(pages),
            validated: (0..count).map(|_| AtomicU8::new(0)).collect(),
        })
    }

    pub(crate) fn is_paged(&self) -> bool {
        self.pages.is_some()
    }

    fn validate_page(
        &self,
        page_index: usize,
        dictionary: &[u8],
        posting_len: usize,
    ) -> Result<std::ops::Range<usize>> {
        let page = &self.pages.as_ref().expect("paged checks")[page_index];
        let start = page_index * PAGE_ENTRIES;
        let end = (start + PAGE_ENTRIES).min(self.validated.len());
        if page.validated.load(Ordering::Acquire) != 0 {
            return Ok(start..end);
        }
        let records = &dictionary[4 + start * 20..4 + end * 20];
        anyhow::ensure!(
            xxh3_64(records) == page.dictionary_hash,
            "Gram dictionary page checksum mismatch"
        );
        anyhow::ensure!(
            xxh3_64(&self.bytes[self.hashes_offset + start * 8..self.hashes_offset + end * 8])
                == page.postings_hash,
            "Gram posting hash page checksum mismatch"
        );
        let mut previous = None;
        for entry in records.as_chunks::<20>().0 {
            let gram = u32_at(entry, 0);
            anyhow::ensure!(
                gram >= page.first && gram <= page.last && previous.is_none_or(|g| g < gram),
                "Invalid gram page ordering"
            );
            let offset = u64_at(entry, 4);
            let length = u32_at(entry, 12);
            anyhow::ensure!(
                offset
                    .checked_add(u64::from(length))
                    .is_some_and(|end| end <= posting_len as u64),
                "Invalid gram page posting range"
            );
            previous = Some(gram);
        }
        anyhow::ensure!(
            u32_at(records, 0) == page.first && previous == Some(page.last),
            "Gram page boundaries mismatch"
        );
        page.validated.store(1, Ordering::Release);
        Ok(start..end)
    }

    pub(crate) fn lookup_range(
        &self,
        gram: u32,
        dictionary: &[u8],
        posting_len: usize,
    ) -> Result<std::ops::Range<usize>> {
        let Some(pages) = &self.pages else {
            return Ok(0..self.validated.len());
        };
        let index = pages.partition_point(|page| page.last < gram);
        if index == pages.len() || gram < pages[index].first {
            return Ok(0..0);
        }
        self.validate_page(index, dictionary, posting_len)
    }

    pub(crate) fn validate_directory(&self, dictionary: &[u8], posting_len: usize) -> Result<()> {
        if let Some(pages) = &self.pages {
            for index in 0..pages.len() {
                self.validate_page(index, dictionary, posting_len)?;
            }
        }
        Ok(())
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
        let at = self.hashes_offset + index * 8;
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
    let page_count = count.div_ceil(PAGE_ENTRIES);
    let root_end = ROOT_HEADER + page_count * PAGE_RECORD;
    let mut bytes = vec![0u8; root_end];
    bytes[..8].copy_from_slice(PAGED_MAGIC);
    bytes[16..24].copy_from_slice(&(postings.len() as u64).to_le_bytes());
    bytes[24..32].copy_from_slice(&(count as u64).to_le_bytes());
    for record in dictionary[4..].as_chunks::<20>().0 {
        let start = usize::try_from(u64_at(record, 4))?;
        let length = u32_at(record, 12) as usize;
        let end = start.checked_add(length).context("Gram range overflow")?;
        let payload = postings.get(start..end).context("Invalid gram range")?;
        bytes.extend_from_slice(&xxh3_64(payload).to_le_bytes());
    }
    for page in 0..page_count {
        let start = page * PAGE_ENTRIES;
        let end = (start + PAGE_ENTRIES).min(count);
        let records = &dictionary[4 + start * 20..4 + end * 20];
        let hashes = &bytes[root_end + start * 8..root_end + end * 8];
        let dictionary_hash = xxh3_64(records);
        let postings_hash = xxh3_64(hashes);
        let at = ROOT_HEADER + page * PAGE_RECORD;
        bytes[at..at + 4].copy_from_slice(&records[..4]);
        bytes[at + 4..at + 8].copy_from_slice(&records[records.len() - 20..records.len() - 16]);
        bytes[at + 8..at + 16].copy_from_slice(&dictionary_hash.to_le_bytes());
        bytes[at + 16..at + 24].copy_from_slice(&postings_hash.to_le_bytes());
    }
    let checksum = xxh3_64(&bytes[16..root_end]);
    bytes[8..16].copy_from_slice(&checksum.to_le_bytes());
    std::fs::write(path.join(NAME), bytes)?;
    Ok(())
}
