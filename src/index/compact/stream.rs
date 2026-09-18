//! Bounded term-at-a-time compaction. Input readers have already established
//! complete segment validity. Recheck decoded ranges before remapping; publish
//! nothing until every output component has finished successfully.
use super::*;
use crate::index::token_dictionary;
use crate::utils::{delta_encode, encode_position_postings};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{BufWriter, Seek, SeekFrom, Write};

const BUFFER: usize = 128 * 1024;
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn range(bytes: &[u8], offset: u64, length: u32) -> Result<&[u8]> {
    let start = usize::try_from(offset).context("Posting offset exceeds platform bounds")?;
    let end = start
        .checked_add(length as usize)
        .context("Posting range overflow")?;
    bytes
        .get(start..end)
        .context("Posting range exceeds file bounds")
}
fn append_docs(
    bytes: &[u8],
    frequency: u32,
    remapping: &DocIdRemapping,
    out: &mut Vec<DocId>,
) -> Result<()> {
    crate::utils::encoding::validate_delta_stream(bytes)?;
    let decoded = delta_decode(bytes);
    anyhow::ensure!(
        decoded.len() == frequency as usize
            && decoded.iter().all(|&id| id != 0 && remapping.contains(id))
            && decoded.windows(2).all(|w| w[0] < w[1]),
        "Invalid posting document IDs or frequency"
    );
    out.extend(decoded.into_iter().filter_map(|id| remapping.remap(id)));
    Ok(())
}
fn output(path: &Path, name: &str) -> Result<BufWriter<File>> {
    Ok(BufWriter::with_capacity(
        BUFFER,
        File::create(path.join(name))?,
    ))
}

struct Grams {
    dict: MappedBytes,
    postings: MappedBytes,
    cursor: usize,
    count: usize,
}
impl Grams {
    fn key(&self) -> Option<Trigram> {
        (self.cursor < self.count).then(|| u32_at(&self.dict, 4 + self.cursor * 20))
    }
}

pub(super) fn grams(
    paths: &[PathBuf],
    destination: &Path,
    remapping: &DocIdRemapping,
    stops: &HashSet<Trigram>,
) -> Result<usize> {
    let mut inputs = Vec::new();
    let mut heap = BinaryHeap::new();
    for path in paths {
        let dict = MappedBytes::open(&path.join("grams.dict"))?;
        anyhow::ensure!(dict.len() >= 4, "Truncated gram dictionary");
        let count = u32_at(&dict, 0) as usize;
        anyhow::ensure!(
            count == (dict.len() - 4) / 20 && (dict.len() - 4) % 20 == 0,
            "Invalid gram dictionary length"
        );
        let input = Grams {
            dict,
            postings: MappedBytes::open(&path.join("grams.postings"))?,
            cursor: 0,
            count,
        };
        if let Some(key) = input.key() {
            heap.push(Reverse((key, inputs.len())));
        }
        inputs.push(input);
    }
    let mut dictionary = output(destination, "grams.dict")?;
    let mut postings = output(destination, "grams.postings")?;
    dictionary.write_all(&0u32.to_le_bytes())?;
    let mut keys = Vec::new();
    let mut ids = Vec::new();
    let mut encoded = Vec::new();
    let mut offset = 0u64;
    while let Some(&Reverse((key, _))) = heap.peek() {
        ids.clear();
        while heap.peek().is_some_and(|Reverse((next, _))| *next == key) {
            let Reverse((_, index)) = heap.pop().unwrap();
            let input = &mut inputs[index];
            let record = &input.dict[4 + input.cursor * 20..];
            append_docs(
                range(&input.postings, u64_at(record, 4), u32_at(record, 12))?,
                u32_at(record, 16),
                remapping,
                &mut ids,
            )?;
            input.cursor += 1;
            if let Some(next) = input.key() {
                anyhow::ensure!(next > key, "Unsorted gram dictionary");
                heap.push(Reverse((next, index)));
            }
        }
        if ids.is_empty() || stops.contains(&key) {
            continue;
        }
        ids.sort_unstable();
        ids.dedup();
        encoded.clear();
        delta_encode(&ids, &mut encoded);
        dictionary.write_all(&key.to_le_bytes())?;
        dictionary.write_all(&offset.to_le_bytes())?;
        dictionary.write_all(&u32::try_from(encoded.len())?.to_le_bytes())?;
        dictionary.write_all(&u32::try_from(ids.len())?.to_le_bytes())?;
        postings.write_all(&encoded)?;
        offset += encoded.len() as u64;
        keys.push(key);
    }
    dictionary.seek(SeekFrom::Start(0))?;
    dictionary.write_all(&u32::try_from(keys.len())?.to_le_bytes())?;
    dictionary.flush()?;
    postings.flush()?;
    segment_io::build_and_write_bloom(destination, keys.iter().copied(), 10000)?;
    Ok(keys.len())
}

struct Tokens {
    dict: MappedBytes,
    postings: MappedBytes,
    positions: Option<MappedBytes>,
    header: token_dictionary::Header,
    cursor: usize,
    remaining: usize,
}
impl Tokens {
    fn entry(&self) -> Result<(token_dictionary::Entry<'_>, usize)> {
        token_dictionary::entry(
            &self.dict[self.cursor..],
            self.header.compact,
            self.positions.is_some(),
        )
    }
    fn key(&self) -> Result<Option<String>> {
        if self.remaining == 0 {
            anyhow::ensure!(
                self.cursor == self.dict.len(),
                "Trailing token dictionary data"
            );
            Ok(None)
        } else {
            Ok(Some(self.entry()?.0.token.to_owned()))
        }
    }
}

pub(super) fn tokens(
    paths: &[PathBuf],
    destination: &Path,
    remapping: &DocIdRemapping,
) -> Result<(usize, bool)> {
    let mut inputs = Vec::new();
    let mut heap = BinaryHeap::new();
    for path in paths {
        let positions = if path.join("tokens.positions").exists() {
            Some(MappedBytes::open(&path.join("tokens.positions"))?)
        } else {
            None
        };
        let dict = MappedBytes::open(&path.join("tokens.dict"))?;
        let header = token_dictionary::header(&dict, positions.is_some())?;
        let input = Tokens {
            dict,
            postings: MappedBytes::open(&path.join("tokens.postings"))?,
            positions,
            header,
            cursor: header.start,
            remaining: header.count,
        };
        if let Some(key) = input.key()? {
            heap.push(Reverse((key, inputs.len())));
        }
        inputs.push(input);
    }
    let has_positions = inputs.iter().all(|input| input.positions.is_some());
    let mut dictionary = output(destination, "tokens.dict")?;
    let mut postings = output(destination, "tokens.postings")?;
    let mut positions = if has_positions {
        Some(output(destination, "tokens.positions")?)
    } else {
        None
    };
    token_dictionary::write_header(&mut dictionary, 0)?;
    let mut count = 0usize;
    let mut offset = 0u64;
    let mut pos_offset = 0u64;
    let mut ids = Vec::new();
    let mut doc_positions: BTreeMap<DocId, Vec<u32>> = BTreeMap::new();
    let mut encoded = Vec::new();
    let mut pos_encoded = Vec::new();
    while let Some(Reverse((key, first))) = heap.pop() {
        ids.clear();
        doc_positions.clear();
        let mut next_input = Some(first);
        while let Some(index) = next_input {
            let input = &mut inputs[index];
            let (entry, consumed) = input.entry()?;
            append_docs(
                range(&input.postings, entry.offset, entry.length)?,
                entry.doc_freq,
                remapping,
                &mut ids,
            )?;
            if let Some(bytes) = &input.positions {
                let bytes = range(bytes, entry.pos_offset, entry.pos_length)?;
                crate::utils::encoding::validate_position_stream(bytes)?;
                // Validate even when one legacy segment lacks positions and
                // the merged index must omit this optional capability.
                for (old, values) in decode_position_postings(bytes) {
                    anyhow::ensure!(remapping.contains(old), "Unknown position document");
                    if has_positions && let Some(new) = remapping.remap(old) {
                        doc_positions.entry(new).or_default().extend(values);
                    }
                }
            }
            input.cursor += consumed;
            input.remaining -= 1;
            if let Some(next) = input.key()? {
                anyhow::ensure!(next > key, "Unsorted token dictionary");
                heap.push(Reverse((next, index)));
            }
            next_input = if heap.peek().is_some_and(|Reverse((next, _))| next == &key) {
                Some(heap.pop().unwrap().0.1)
            } else {
                None
            };
        }
        if ids.is_empty() {
            continue;
        }
        ids.sort_unstable();
        ids.dedup();
        encoded.clear();
        delta_encode(&ids, &mut encoded);
        pos_encoded.clear();
        if let Some(writer) = &mut positions {
            let refs: Vec<_> = doc_positions
                .iter()
                .map(|(&id, values)| (id, values.as_slice()))
                .collect();
            encode_position_postings(&refs, &mut pos_encoded);
            writer.write_all(&pos_encoded)?;
        }
        token_dictionary::write_entry(
            &mut dictionary,
            token_dictionary::Entry {
                token: &key,
                offset,
                length: u32::try_from(encoded.len())?,
                doc_freq: u32::try_from(ids.len())?,
                pos_offset,
                pos_length: u32::try_from(pos_encoded.len())?,
            },
            has_positions,
        )?;
        postings.write_all(&encoded)?;
        offset += encoded.len() as u64;
        pos_offset += pos_encoded.len() as u64;
        count += 1;
    }
    dictionary.seek(SeekFrom::Start(0))?;
    token_dictionary::write_header(&mut dictionary, count)?;
    dictionary.flush()?;
    postings.flush()?;
    if let Some(writer) = &mut positions {
        writer.flush()?;
    }
    Ok((count, has_positions))
}

pub(super) fn lines<'a>(
    inputs: impl IntoIterator<Item = (SegmentId, &'a Path)>,
    destination: &Path,
    remapping: &DocIdRemapping,
    belongs_to_segment: impl Fn(DocId, SegmentId) -> bool,
) -> Result<()> {
    let mut writer = output(destination, "linemap.bin")?;
    writer.write_all(&0u32.to_le_bytes())?;
    let mut written = 0u32;
    let mut seen = HashSet::new();
    for (segment_id, path) in inputs {
        // Legacy segments may have no stored line-map capability.
        if !path.join("linemap.bin").try_exists()? {
            continue;
        }
        let bytes = MappedBytes::open(&path.join("linemap.bin"))?;
        anyhow::ensure!(bytes.len() >= 4, "Truncated line maps");
        let count = u32_at(&bytes, 0) as usize;
        anyhow::ensure!(
            count <= (bytes.len() - 4) / 12,
            "Line map count exceeds file bounds"
        );
        let mut cursor = 4;
        for _ in 0..count {
            let record = bytes
                .get(cursor..cursor + 12)
                .context("Truncated line map record")?;
            let old = u32_at(record, 0);
            let line_count = u32_at(record, 4);
            let len = u32_at(record, 8);
            cursor += 12;
            let encoded = range(&bytes, cursor as u64, len)?;
            let actual = crate::utils::encoding::validate_delta_stream(encoded)?;
            let offsets = delta_decode(encoded);
            anyhow::ensure!(
                actual == line_count as usize
                    && offsets.first() == Some(&0)
                    && offsets.windows(2).all(|w| w[0] < w[1]),
                "Invalid line offsets"
            );
            anyhow::ensure!(
                remapping.contains(old) && belongs_to_segment(old, segment_id) && seen.insert(old),
                "Unknown or duplicate line map document"
            );
            if let Some(new) = remapping.remap(old) {
                writer.write_all(&new.to_le_bytes())?;
                writer.write_all(&record[4..])?;
                writer.write_all(encoded)?;
                written = written.checked_add(1).context("Too many line maps")?;
            }
            cursor += len as usize;
        }
        anyhow::ensure!(cursor == bytes.len(), "Trailing line map data");
    }
    writer.seek(SeekFrom::Start(0))?;
    writer.write_all(&written.to_le_bytes())?;
    writer.flush()?;
    Ok(())
}
