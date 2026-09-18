//! Optional immutable source copies. These are accelerators: absent, invalid,
//! stale or unsupported evidence always falls back to reading the live source.
use crate::index::types::{DocId, Document};
use anyhow::{Result, ensure};
use memmap2::Mmap;
use std::collections::BTreeMap;
use std::fs::{self, File, Metadata};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

mod compressed;

pub(crate) enum SourcePack {
    Raw(RawSourcePack),
    Compressed(compressed::CompressedPack),
}
impl SourcePack {
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let mut header = [0; 8];
        File::open(directory.join(TABLE))?.read_exact(&mut header)?;
        if &header == compressed::MAGIC {
            compressed::CompressedPack::open(directory).map(Self::Compressed)
        } else {
            RawSourcePack::open(directory).map(Self::Raw)
        }
    }
    pub(crate) fn read(
        &self,
        id: DocId,
        relative: &Path,
        path: &Path,
    ) -> Option<std::borrow::Cow<'_, str>> {
        match self {
            Self::Raw(p) => p.read(id, relative, path).map(std::borrow::Cow::Borrowed),
            Self::Compressed(p) => p.read(id, relative, path).map(std::borrow::Cow::Owned),
        }
    }
    pub(crate) fn contains_literal(
        &self,
        id: DocId,
        relative: &Path,
        path: &Path,
        finder: &memchr::memmem::Finder<'_>,
    ) -> Option<bool> {
        match self {
            Self::Raw(p) => p.contains_literal(id, relative, path, finder),
            Self::Compressed(p) => p.contains_literal(id, relative, path, finder),
        }
    }
}

const MAGIC: &[u8; 8] = b"FXISRC02";
const WORDS: usize = 13;
const BLOCK_BYTES: usize = 4096;
const RECORD_BYTES: usize = WORDS * 8;
const TABLE: &str = "source.table";
const DATA: &str = "source.data";

fn stamp(_metadata: &Metadata) -> Option<[u64; 7]> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some([
            _metadata.len(),
            _metadata.dev(),
            _metadata.ino(),
            _metadata.mtime() as u64,
            _metadata.mtime_nsec() as u64,
            _metadata.ctime() as u64,
            _metadata.ctime_nsec() as u64,
        ])
    }
    #[cfg(not(unix))]
    None
}

fn path_hash(path: &Path) -> u64 {
    xxh3_64(path.as_os_str().as_encoded_bytes())
}

pub(crate) fn requested() -> bool {
    cfg!(unix) && std::env::var_os("FXI_SOURCE_PACK").is_some_and(|v| v == "1")
}

pub(crate) fn present(index: &Path) -> bool {
    fs::read_dir(index.join("segments")).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().join(TABLE).is_file())
    })
}

/// Write only new segments, never mutate inherited hard-linked files. Generation
/// publication syncs both files before making the new generation visible.
pub(crate) fn write_missing(
    index: &Path,
    root: &Path,
    docs: &[Document],
    paths: &[PathBuf],
    enabled: bool,
) -> Result<()> {
    if !enabled || !cfg!(unix) {
        return Ok(());
    }
    let mut segments = BTreeMap::<_, Vec<&Document>>::new();
    for doc in docs.iter().filter(|doc| doc.is_valid()) {
        segments.entry(doc.segment_id).or_default().push(doc);
    }
    for (id, mut documents) in segments {
        let directory = index.join("segments").join(format!("seg_{id:04}"));
        if directory.join(TABLE).exists() {
            continue;
        }
        documents.sort_unstable_by_key(|doc| doc.doc_id);
        // An inherited orphan may be hard-linked into a pinned old generation.
        // Unlink our name first; never truncate the inherited inode.
        match fs::remove_file(directory.join(DATA)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if std::env::var_os("FXI_SOURCE_PACK_COMPRESSION").is_some_and(|v| v == "1") {
            compressed::write_segment(&directory, root, &documents, paths)?;
            continue;
        }
        let mut output = BufWriter::new(File::create_new(directory.join(DATA))?);
        let mut block_hashes = Vec::new();
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
                || file.metadata().ok().and_then(|meta| stamp(&meta)) != Some(before)
            {
                continue;
            }
            output.write_all(text.as_bytes())?;
            let record = [
                u64::from(doc.doc_id),
                path_hash(relative),
                offset,
                text.len() as u64,
                xxh3_64(text.as_bytes()),
                before[0],
                before[1],
                before[2],
                before[3],
                before[4],
                before[5],
                before[6],
                block_hashes.len() as u64 / 8,
            ];
            for block in text.as_bytes().chunks(BLOCK_BYTES) {
                block_hashes.extend_from_slice(&xxh3_64(block).to_le_bytes());
            }
            for word in record {
                records.extend_from_slice(&word.to_le_bytes());
            }
            offset += text.len() as u64;
        }
        output.flush()?;
        let mut table = BufWriter::new(File::create(directory.join(TABLE))?);
        table.write_all(MAGIC)?;
        table.write_all(&(records.len() as u64 / RECORD_BYTES as u64).to_le_bytes())?;
        records.extend_from_slice(&block_hashes);
        table.write_all(&xxh3_64(&records).to_le_bytes())?;
        table.write_all(&records)?;
        table.flush()?;
    }
    Ok(())
}

pub(crate) struct RawSourcePack {
    records: Vec<[u64; WORDS]>,
    block_hashes: Vec<u64>,
    data: Option<Mmap>,
}
impl RawSourcePack {
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        ensure!(cfg!(unix), "source pack metadata validation requires Unix");
        let table = fs::read(directory.join(TABLE))?;
        ensure!(
            table.len() >= 24 && &table[..8] == MAGIC,
            "invalid source table header"
        );
        let word = |bytes: &[u8]| u64::from_le_bytes(bytes.try_into().unwrap());
        let count = word(&table[8..16]);
        let payload = &table[24..];
        ensure!(
            count <= (payload.len() / RECORD_BYTES) as u64,
            "invalid source table count"
        );
        let record_bytes = count as usize * RECORD_BYTES;
        let (entries, hashes) = payload.split_at(record_bytes);
        ensure!(hashes.len() % 8 == 0, "invalid source checksum table");
        let block_hashes: Vec<u64> = hashes.as_chunks::<8>().0.iter().map(|b| word(b)).collect();
        ensure!(
            word(&table[16..24]) == xxh3_64(payload),
            "source table checksum mismatch"
        );
        let file = File::open(directory.join(DATA))?;
        let length = file.metadata()?.len();
        let mut records = Vec::with_capacity(count as usize);
        let mut previous = None;
        for bytes in entries.as_chunks::<RECORD_BYTES>().0 {
            let record: [u64; WORDS] = std::array::from_fn(|i| word(&bytes[i * 8..i * 8 + 8]));
            ensure!(
                record[0] <= u64::from(u32::MAX) && previous.is_none_or(|id| id < record[0]),
                "invalid source document order"
            );
            ensure!(
                record[3] == record[5]
                    && record[2]
                        .checked_add(record[3])
                        .is_some_and(|end| end <= length),
                "invalid source byte range"
            );
            ensure!(
                record[12]
                    .checked_add(record[3].div_ceil(BLOCK_BYTES as u64))
                    .is_some_and(|end| end <= block_hashes.len() as u64),
                "invalid source block range"
            );
            previous = Some(record[0]);
            records.push(record);
        }
        // Published index generations are immutable and pinned by the reader.
        // Editable source files are never mapped. Empty files cannot be mapped.
        let data = if length == 0 {
            None
        } else {
            Some(unsafe { Mmap::map(&file)? })
        };
        Ok(Self {
            records,
            block_hashes,
            data,
        })
    }

    fn source(
        &self,
        id: DocId,
        relative: &Path,
        full_path: &Path,
    ) -> Option<(&[u64; WORDS], &[u8])> {
        let position = self
            .records
            .binary_search_by_key(&u64::from(id), |r| r[0])
            .ok()?;
        let record = &self.records[position];
        if record[1] != path_hash(relative) {
            return None;
        }
        let current = stamp(&fs::metadata(full_path).ok()?)?;
        if current.as_slice() != &record[5..12] {
            return None;
        }
        let start = usize::try_from(record[2]).ok()?;
        let end = start.checked_add(usize::try_from(record[3]).ok()?)?;
        let bytes = self.data.as_deref().unwrap_or(&[]).get(start..end)?;
        Some((record, bytes))
    }

    pub(crate) fn read(&self, id: DocId, relative: &Path, full_path: &Path) -> Option<&str> {
        let (record, bytes) = self.source(id, relative, full_path)?;
        // Check the entire file, including bytes beyond the first possible hit.
        // XXH3 detects accidental corruption; it is not authentication.
        if xxh3_64(bytes) != record[4] {
            return None;
        }
        std::str::from_utf8(bytes).ok()
    }

    /// An unchanged source stamp proves the captured source is still complete
    /// UTF-8. A positive literal witness needs only the blocks covering its
    /// bytes; a negative must validate every block so corruption cannot hide it.
    pub(crate) fn contains_literal(
        &self,
        id: DocId,
        relative: &Path,
        full_path: &Path,
        finder: &memchr::memmem::Finder<'_>,
    ) -> Option<bool> {
        let (record, bytes) = self.source(id, relative, full_path)?;
        let needle_len = finder.needle().len();
        if needle_len == 0 || needle_len > BLOCK_BYTES {
            return self
                .read(id, relative, full_path)
                .map(|text| finder.find(text.as_bytes()).is_some());
        }
        let base = usize::try_from(record[12]).ok()?;
        for (number, block) in bytes.chunks(BLOCK_BYTES).enumerate() {
            if xxh3_64(block) != *self.block_hashes.get(base.checked_add(number)?)? {
                return None;
            }
            let start = number * BLOCK_BYTES;
            // The preceding block has already passed its checksum. Include
            // enough preceding bytes to cover every cross-boundary match.
            let overlap_start = start.saturating_sub(needle_len - 1);
            if finder
                .find(&bytes[overlap_start..start + block.len()])
                .is_some()
            {
                return Some(true);
            }
        }
        Some(false)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::index::types::{DocFlags, Language};

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let segment = temp.path().join("segments/seg_0001");
        fs::create_dir_all(&segment).unwrap();
        let path = temp.path().join("a.txt");
        fs::write(&path, "needle\n").unwrap();
        let doc = Document {
            doc_id: 1,
            path_id: 0,
            size: 7,
            mtime: 0,
            language: Language::Unknown,
            flags: DocFlags::new(),
            segment_id: 1,
        };
        write_missing(
            temp.path(),
            temp.path(),
            &[doc],
            &[PathBuf::from("a.txt")],
            true,
        )
        .unwrap();
        (temp, segment, path)
    }

    #[test]
    fn literal_blocks_cover_boundaries_long_needles_and_corrupt_negative_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let segment = temp.path().join("segments/seg_0001");
        fs::create_dir_all(&segment).unwrap();
        let path = temp.path().join("a.txt");
        let relative = Path::new("a.txt");
        for boundary in [
            BLOCK_BYTES - 3,
            BLOCK_BYTES - 1,
            BLOCK_BYTES,
            BLOCK_BYTES + 1,
        ] {
            let text = format!(
                "{}needle{}éclair{}",
                "x".repeat(boundary),
                "y".repeat(BLOCK_BYTES),
                "z".repeat(BLOCK_BYTES)
            );
            fs::write(&path, &text).unwrap();
            let doc = Document {
                doc_id: 1,
                path_id: 0,
                size: text.len() as u64,
                mtime: 0,
                language: Language::Unknown,
                flags: DocFlags::new(),
                segment_id: 1,
            };
            let _ = fs::remove_file(segment.join(TABLE));
            write_missing(
                temp.path(),
                temp.path(),
                &[doc],
                &[relative.to_path_buf()],
                true,
            )
            .unwrap();
            {
                let pack = SourcePack::open(&segment).unwrap();
                for needle in [
                    "needle",
                    "éclair",
                    "never-present",
                    &"y".repeat(BLOCK_BYTES + 1),
                ] {
                    let finder = memchr::memmem::Finder::new(needle.as_bytes());
                    assert_eq!(
                        pack.contains_literal(1, relative, &path, &finder),
                        Some(text.contains(needle))
                    );
                }
            }
            let original = fs::read(segment.join(DATA)).unwrap();
            let mut corrupt = original.clone();
            corrupt[boundary] = b'q';
            fs::write(segment.join(DATA), &corrupt).unwrap();
            {
                let pack = SourcePack::open(&segment).unwrap();
                assert_eq!(
                    pack.contains_literal(
                        1,
                        relative,
                        &path,
                        &memchr::memmem::Finder::new(b"needle")
                    ),
                    None
                );
            }
            // A later damaged block cannot invalidate an earlier verified
            // positive witness, but must never prove a negative result.
            let mut tail = original;
            *tail.last_mut().unwrap() = 0xff;
            fs::write(segment.join(DATA), tail).unwrap();
            {
                let pack = SourcePack::open(&segment).unwrap();
                assert_eq!(
                    pack.contains_literal(
                        1,
                        relative,
                        &path,
                        &memchr::memmem::Finder::new(b"needle")
                    ),
                    Some(true)
                );
                assert_eq!(
                    pack.contains_literal(
                        1,
                        relative,
                        &path,
                        &memchr::memmem::Finder::new(b"never-present")
                    ),
                    None
                );
            }
            // Invalid UTF-8 in the live source is different: its changed stamp
            // forces fallback even if the match precedes the invalid byte.
            let mut live = text.into_bytes();
            live.push(0xff);
            fs::write(&path, live).unwrap();
            let pack = SourcePack::open(&segment).unwrap();
            assert_eq!(
                pack.contains_literal(1, relative, &path, &memchr::memmem::Finder::new(b"needle")),
                None
            );
        }
    }

    #[test]
    fn snapshots_reject_edits_restored_mtime_replacement_deletion_and_wrong_path() {
        let (_temp, segment, path) = fixture();
        let pack = SourcePack::open(&segment).unwrap();
        let relative = Path::new("a.txt");
        assert_eq!(pack.read(1, relative, &path).as_deref(), Some("needle\n"));
        assert_eq!(pack.read(2, relative, &path), None);
        assert_eq!(pack.read(1, Path::new("b.txt"), &path), None);
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "absent\n").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(pack.read(1, relative, &path), None);
        fs::remove_file(&path).unwrap();
        fs::write(&path, "needle\n").unwrap();
        assert_eq!(pack.read(1, relative, &path), None);
        fs::remove_file(&path).unwrap();
        assert_eq!(pack.read(1, relative, &path), None);
    }

    #[test]
    fn table_truncation_corruption_and_data_corruption_are_not_evidence() {
        let (_temp, segment, path) = fixture();
        let original = fs::read(segment.join(TABLE)).unwrap();
        for length in 0..original.len() {
            fs::write(segment.join(TABLE), &original[..length]).unwrap();
            assert!(SourcePack::open(&segment).is_err());
        }
        let mut corrupt = original.clone();
        corrupt[32] ^= 1;
        fs::write(segment.join(TABLE), corrupt).unwrap();
        assert!(SourcePack::open(&segment).is_err());
        fs::write(segment.join(TABLE), original).unwrap();
        fs::write(segment.join(DATA), "absent\n").unwrap();
        let pack = SourcePack::open(&segment).unwrap();
        assert_eq!(pack.read(1, Path::new("a.txt"), &path), None);
        drop(pack);
        fs::write(segment.join(DATA), "short").unwrap();
        assert!(SourcePack::open(&segment).is_err());
    }
}
