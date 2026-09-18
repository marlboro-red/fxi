//! Experimental independently compressed source blocks. Filters describe the
//! same captured source bytes, not the earlier tokenization snapshot.
use super::{BLOCK_BYTES, DATA, TABLE, path_hash, stamp};
use crate::index::types::{DocId, Document};
use anyhow::{Result, ensure};
use memmap2::Mmap;
use std::{
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
};
use xxhash_rust::xxh3::xxh3_64;

pub(super) const MAGIC: &[u8; 8] = b"FXISRC03";
const WORDS: usize = 14;
const RECORD: usize = WORDS * 8;
// offset, stored length, raw length, raw checksum, then 2048 filter bits.
const DESCRIPTOR: usize = 32 + 256;
const MAX_LITERAL: usize = 256;
fn word(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?))
}
fn filter(bytes: &[u8]) -> [u64; 32] {
    let mut bits = [0u64; 32];
    for gram in bytes.windows(3) {
        let n = u64::from(gram[0]) | (u64::from(gram[1]) << 8) | (u64::from(gram[2]) << 16);
        let hash = n.wrapping_mul(0x9e3779b185ebca87);
        for shift in [0, 21, 42] {
            let bit = ((hash >> shift) & 2047) as usize;
            bits[bit / 64] |= 1 << (bit % 64);
        }
    }
    bits
}
fn push_word(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}

pub(super) fn write_segment(
    directory: &Path,
    root: &Path,
    documents: &[&Document],
    paths: &[PathBuf],
) -> Result<()> {
    let mut output = BufWriter::new(File::create_new(directory.join(DATA))?);
    let mut records = Vec::new();
    let mut offset = 0u64;
    for doc in documents {
        let Some(relative) = paths.get(doc.path_id as usize) else {
            continue;
        };
        let Ok(mut file) = File::open(root.join(relative)) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        let Some(before) = stamp(&metadata) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() != doc.size {
            continue;
        }
        let mut text = String::new();
        if (&mut file)
            .take(doc.size.saturating_add(1))
            .read_to_string(&mut text)
            .is_err()
            || text.len() as u64 != doc.size
            || file.metadata().ok().and_then(|m| stamp(&m)) != Some(before)
        {
            continue;
        }
        let bytes = text.as_bytes();
        let mut descriptors = Vec::new();
        let mut payload = Vec::new();
        for (number, block) in bytes.chunks(BLOCK_BYTES).enumerate() {
            let compressed = if number == 0 {
                Vec::new()
            } else {
                lz4_flex::block::compress(block)
            };
            let raw = number == 0 || compressed.len() >= block.len();
            let stored = if raw { block } else { &compressed };
            push_word(&mut descriptors, payload.len() as u64);
            // Equal stored/raw lengths encode the uncompressed case. A compressed
            // representation is retained only if strictly smaller.
            push_word(&mut descriptors, stored.len() as u64);
            push_word(&mut descriptors, block.len() as u64);
            push_word(&mut descriptors, xxh3_64(block));
            let start = number * BLOCK_BYTES;
            for value in
                filter(&bytes[start..(start + block.len() + MAX_LITERAL - 1).min(bytes.len())])
            {
                push_word(&mut descriptors, value);
            }
            payload.extend_from_slice(stored);
        }
        let length = (descriptors.len() + payload.len()) as u64;
        for value in [
            u64::from(doc.doc_id),
            path_hash(relative),
            offset,
            text.len() as u64,
            xxh3_64(bytes),
            before[0],
            before[1],
            before[2],
            before[3],
            before[4],
            before[5],
            before[6],
            length,
            xxh3_64(&descriptors),
        ] {
            push_word(&mut records, value);
        }
        output.write_all(&descriptors)?;
        output.write_all(&payload)?;
        offset = offset
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("source pack overflow"))?;
    }
    output.flush()?;
    let mut table = BufWriter::new(File::create(directory.join(TABLE))?);
    table.write_all(MAGIC)?;
    table.write_all(&(records.len() as u64 / RECORD as u64).to_le_bytes())?;
    table.write_all(&xxh3_64(&records).to_le_bytes())?;
    table.write_all(&records)?;
    table.flush()?;
    Ok(())
}
pub(crate) struct CompressedPack {
    records: Vec<[u64; WORDS]>,
    data: Option<Mmap>,
}
struct Source<'a> {
    record: &'a [u64; WORDS],
    descriptors: &'a [u8],
    payload: &'a [u8],
}
impl Source<'_> {
    fn blocks(&self) -> usize {
        self.descriptors.len() / DESCRIPTOR
    }
    fn descriptor(&self, n: usize) -> Option<&[u8]> {
        self.descriptors
            .get(n.checked_mul(DESCRIPTOR)?..n.checked_add(1)?.checked_mul(DESCRIPTOR)?)
    }
    fn decode<'a>(&'a self, n: usize, scratch: &'a mut Vec<u8>) -> Option<&'a [u8]> {
        let d = self.descriptor(n)?;
        let offset = usize::try_from(word(d)?).ok()?;
        let stored = usize::try_from(word(&d[8..])?).ok()?;
        let len = usize::try_from(word(&d[16..])?).ok()?;
        // Bound allocation independently of untrusted on-disk lengths.
        let expected = if n + 1 == self.blocks() {
            usize::try_from(self.record[3])
                .ok()?
                .checked_sub(n.checked_mul(BLOCK_BYTES)?)?
        } else {
            BLOCK_BYTES
        };
        if len != expected || len == 0 || len > BLOCK_BYTES || stored == 0 || stored > len {
            return None;
        }
        let bytes = self.payload.get(offset..offset.checked_add(stored)?)?;
        let decoded = if stored == len {
            bytes
        } else {
            scratch.resize(len, 0);
            if lz4_flex::block::decompress_into(bytes, scratch).ok()? != len {
                return None;
            }
            scratch.as_slice()
        };
        (xxh3_64(decoded) == word(&d[24..])?).then_some(decoded)
    }
    fn possible(&self, n: usize, query: &[u64; 32]) -> Option<bool> {
        let d = self.descriptor(n)?;
        for (i, q) in query.iter().enumerate() {
            if word(&d[32 + i * 8..])? & q != *q {
                return Some(false);
            }
        }
        Some(true)
    }
}
impl CompressedPack {
    pub(super) fn open(directory: &Path) -> Result<Self> {
        ensure!(cfg!(unix), "source pack metadata validation requires Unix");
        let table = fs::read(directory.join(TABLE))?;
        ensure!(
            table.len() >= 24 && &table[..8] == MAGIC,
            "invalid compressed source header"
        );
        let count = word(&table[8..]).unwrap();
        let payload = &table[24..];
        ensure!(
            payload.len() % RECORD == 0 && count == (payload.len() / RECORD) as u64,
            "invalid compressed source count"
        );
        ensure!(
            word(&table[16..]) == Some(xxh3_64(payload)),
            "compressed source table checksum"
        );
        let file = File::open(directory.join(DATA))?;
        let length = file.metadata()?.len();
        let mut records = Vec::with_capacity(count as usize);
        let mut previous = None;
        for bytes in payload.chunks_exact(RECORD) {
            let r: [u64; WORDS] = std::array::from_fn(|i| word(&bytes[i * 8..]).unwrap());
            ensure!(
                r[0] <= u64::from(u32::MAX) && previous.is_none_or(|id| id < r[0]),
                "compressed source document order"
            );
            ensure!(
                r[3] == r[5] && r[2].checked_add(r[12]).is_some_and(|end| end <= length),
                "compressed source range"
            );
            ensure!(
                r[3].div_ceil(BLOCK_BYTES as u64)
                    .checked_mul(DESCRIPTOR as u64)
                    .is_some_and(|n| n <= r[12]),
                "compressed descriptor range"
            );
            previous = Some(r[0]);
            records.push(r);
        }
        // Immutable generation-owned pack only, never editable source files.
        let data = if length == 0 {
            None
        } else {
            Some(unsafe { Mmap::map(&file)? })
        };
        Ok(Self { records, data })
    }
    fn source(&self, id: DocId, relative: &Path, path: &Path) -> Option<Source<'_>> {
        let record = &self.records[self
            .records
            .binary_search_by_key(&u64::from(id), |r| r[0])
            .ok()?];
        if record[1] != path_hash(relative)
            || stamp(&fs::metadata(path).ok()?)?.as_slice() != &record[5..12]
        {
            return None;
        }
        let start = usize::try_from(record[2]).ok()?;
        let len = usize::try_from(record[12]).ok()?;
        let bytes = self
            .data
            .as_deref()
            .unwrap_or(&[])
            .get(start..start.checked_add(len)?)?;
        let metadata = usize::try_from(record[3].div_ceil(BLOCK_BYTES as u64))
            .ok()?
            .checked_mul(DESCRIPTOR)?;
        let descriptors = bytes.get(..metadata)?;
        // A filter can prove absence only after its per-file metadata checksum.
        // Do not eagerly read every file's filter table when opening the index.
        if xxh3_64(descriptors) != record[13] {
            return None;
        }
        Some(Source {
            record,
            descriptors,
            payload: bytes.get(metadata..)?,
        })
    }
    pub(super) fn read(&self, id: DocId, relative: &Path, path: &Path) -> Option<String> {
        let source = self.source(id, relative, path)?;
        let mut result = Vec::new();
        let mut scratch = Vec::new();
        for n in 0..source.blocks() {
            result.extend_from_slice(source.decode(n, &mut scratch)?);
        }
        if result.len() as u64 != source.record[3] || xxh3_64(&result) != source.record[4] {
            return None;
        }
        String::from_utf8(result).ok()
    }
    pub(super) fn contains_literal(
        &self,
        id: DocId,
        relative: &Path,
        path: &Path,
        finder: &memchr::memmem::Finder<'_>,
    ) -> Option<bool> {
        let needle = finder.needle();
        if !(3..=MAX_LITERAL).contains(&needle.len()) {
            return self
                .read(id, relative, path)
                .map(|s| finder.find(s.as_bytes()).is_some());
        }
        let source = self.source(id, relative, path)?;
        let query = filter(needle);
        let mut scratch = Vec::new();
        let mut window = Vec::new();
        for n in 0..source.blocks() {
            if !source.possible(n, &query)? {
                continue;
            }
            let bytes = source.decode(n, &mut scratch)?;
            if finder.find(bytes).is_some() {
                return Some(true);
            }
            if n + 1 < source.blocks() {
                window.clear();
                window.extend_from_slice(&bytes[bytes.len().saturating_sub(needle.len() - 1)..]);
                let next = source.decode(n + 1, &mut scratch)?;
                window.extend_from_slice(&next[..next.len().min(needle.len() - 1)]);
                if finder.find(&window).is_some() {
                    return Some(true);
                }
            }
        }
        Some(false)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::index::types::{DocFlags, Language};
    fn fixture(text: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("pack");
        fs::create_dir(&directory).unwrap();
        let path = temp.path().join("a.txt");
        fs::write(&path, text).unwrap();
        let doc = Document {
            doc_id: 1,
            path_id: 0,
            size: text.len() as u64,
            mtime: 0,
            language: Language::Unknown,
            flags: DocFlags::new(),
            segment_id: 1,
        };
        write_segment(&directory, temp.path(), &[&doc], &[PathBuf::from("a.txt")]).unwrap();
        (temp, directory, path)
    }
    #[test]
    fn roundtrip_boundaries_empty_long_literals_and_changed_sources() {
        let mut seed = 37u64;
        let random: String = (0..18000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (b'a' + (seed % 26) as u8) as char
            })
            .collect();
        for text in [
            String::new(),
            "éclair".into(),
            format!(
                "{}éclair{}needle{}",
                "a".repeat(4094),
                "b".repeat(8190),
                "c".repeat(6000)
            ),
            random,
        ] {
            let (_temp, directory, path) = fixture(&text);
            let pack = CompressedPack::open(&directory).unwrap();
            let relative = Path::new("a.txt");
            assert_eq!(
                pack.read(1, relative, &path).as_deref(),
                Some(text.as_str())
            );
            for start in (0..text.len())
                .step_by(67)
                .filter(|n| text.is_char_boundary(*n))
            {
                for length in [1, 2, 3, 7, 255, 256, 257, 4097] {
                    let end = (start + length).min(text.len());
                    if !text.is_char_boundary(end) {
                        continue;
                    }
                    let needle = &text.as_bytes()[start..end];
                    assert_eq!(
                        pack.contains_literal(
                            1,
                            relative,
                            &path,
                            &memchr::memmem::Finder::new(needle)
                        ),
                        Some(true),
                        "start {start}, len {length}"
                    );
                }
            }
            for needle in ["", "éclair", "needle", "absent123456"] {
                assert_eq!(
                    pack.contains_literal(
                        1,
                        relative,
                        &path,
                        &memchr::memmem::Finder::new(needle.as_bytes())
                    ),
                    Some(text.contains(needle))
                );
            }
            assert!(pack.read(1, Path::new("wrong"), &path).is_none());
            assert!(pack.read(2, relative, &path).is_none());
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            fs::write(&path, "x".repeat(text.len())).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(modified))
                .unwrap();
            assert!(pack.read(1, relative, &path).is_none());
            fs::remove_file(&path).unwrap();
            assert!(pack.read(1, relative, &path).is_none());
        }
    }
    #[test]
    fn truncation_filter_corruption_and_decoder_bounds_fall_back() {
        let text = format!("{}needle{}", "a".repeat(5000), "b".repeat(10000));
        let (_temp, directory, path) = fixture(&text);
        let relative = Path::new("a.txt");
        let table = fs::read(directory.join(TABLE)).unwrap();
        let data = fs::read(directory.join(DATA)).unwrap();
        for len in 0..table.len() {
            fs::write(directory.join(TABLE), &table[..len]).unwrap();
            assert!(CompressedPack::open(&directory).is_err());
        }
        fs::write(directory.join(TABLE), &table).unwrap();
        let mut damaged = data.clone();
        damaged[32] ^= 1;
        fs::write(directory.join(DATA), damaged).unwrap();
        {
            let p = CompressedPack::open(&directory).unwrap();
            assert!(
                p.contains_literal(1, relative, &path, &memchr::memmem::Finder::new(b"absent"))
                    .is_none()
            );
        }
        // Corrupt the compressed block containing the only witness.
        let metadata = text.len().div_ceil(BLOCK_BYTES) * DESCRIPTOR;
        let offset = word(&data[DESCRIPTOR..]).unwrap() as usize;
        let mut damaged = data.clone();
        damaged[metadata + offset] ^= 0xff;
        fs::write(directory.join(DATA), damaged).unwrap();
        {
            let p = CompressedPack::open(&directory).unwrap();
            assert!(p.read(1, relative, &path).is_none());
            assert!(
                p.contains_literal(1, relative, &path, &memchr::memmem::Finder::new(b"needle"))
                    .is_none()
            );
        }
        // Even a correctly checksummed malformed descriptor cannot request an
        // unbounded decompression allocation or an out-of-range payload slice.
        for slot in [0, 8, 16] {
            let mut damaged = data.clone();
            damaged[slot..slot + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            let mut adjusted = table.clone();
            adjusted[24 + 13 * 8..24 + 14 * 8]
                .copy_from_slice(&xxh3_64(&damaged[..metadata]).to_le_bytes());
            let hash = xxh3_64(&adjusted[24..]);
            adjusted[16..24].copy_from_slice(&hash.to_le_bytes());
            fs::write(directory.join(DATA), damaged).unwrap();
            fs::write(directory.join(TABLE), adjusted).unwrap();
            let p = CompressedPack::open(&directory).unwrap();
            assert!(p.read(1, relative, &path).is_none());
        }
        fs::write(directory.join(TABLE), table).unwrap();
        fs::write(directory.join(DATA), &data[..data.len() - 1]).unwrap();
        assert!(CompressedPack::open(&directory).is_err());
    }
}
