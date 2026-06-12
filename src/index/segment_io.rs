//! Shared on-disk segment format writers.
//!
//! Used by the delta writer (incremental updates) and compaction (segment
//! merging), which both hold postings as BTreeMaps. The chunked full-build
//! writer in `writer.rs` keeps its own flat-array encoders: same file
//! format, different in-memory shape, tuned for build throughput.

use crate::index::types::{DocId, Trigram};
use crate::utils::{BloomFilter, delta_encode, encode_position_postings};
use anyhow::Result;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// Token -> doc -> positions postings
pub type PositionPostings = BTreeMap<String, BTreeMap<DocId, Vec<u32>>>;

const BUF_CAPACITY: usize = 131072;

/// Delta-encode a postings list, sorting and deduplicating first if needed
/// (delta segments append doc ids in insertion order, which is already
/// ascending in practice; this keeps the format safe if that ever changes)
fn encode_postings(doc_ids: &[DocId], encoded: &mut Vec<u8>) -> usize {
    if doc_ids.is_sorted() {
        delta_encode(doc_ids, encoded);
        doc_ids.len()
    } else {
        let mut sorted = doc_ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        delta_encode(&sorted, encoded);
        sorted.len()
    }
}

/// Write grams.dict + grams.postings. Trigrams in `stop_grams` (if given)
/// are omitted from the segment entirely: they match too many documents to
/// narrow and the executor never looks them up.
pub fn write_trigram_index(
    segment_path: &Path,
    postings: &BTreeMap<Trigram, Vec<DocId>>,
    stop_grams: Option<&HashSet<Trigram>>,
) -> Result<()> {
    let dict_path = segment_path.join("grams.dict");
    let postings_path = segment_path.join("grams.postings");

    let mut dict_file = BufWriter::with_capacity(BUF_CAPACITY, File::create(&dict_path)?);
    let mut postings_file = BufWriter::with_capacity(BUF_CAPACITY, File::create(&postings_path)?);

    let filtered: Vec<_> = postings
        .iter()
        .filter(|(t, _)| !stop_grams.is_some_and(|s| s.contains(t)))
        .collect();

    // Write entry count
    dict_file.write_all(&(filtered.len() as u32).to_le_bytes())?;

    let mut postings_offset: u64 = 0;
    let mut encoded = Vec::new();

    for (&trigram, doc_ids) in filtered {
        encoded.clear();
        let doc_freq = encode_postings(doc_ids, &mut encoded);

        // Write dictionary entry
        dict_file.write_all(&trigram.to_le_bytes())?;
        dict_file.write_all(&postings_offset.to_le_bytes())?;
        dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
        dict_file.write_all(&(doc_freq as u32).to_le_bytes())?;

        // Write postings
        postings_file.write_all(&encoded)?;
        postings_offset += encoded.len() as u64;
    }

    dict_file.flush()?;
    postings_file.flush()?;
    Ok(())
}

/// Write tokens.dict + tokens.postings + tokens.positions.
pub fn write_token_index(
    segment_path: &Path,
    postings: &BTreeMap<String, Vec<DocId>>,
    positions: Option<&PositionPostings>,
) -> Result<()> {
    let dict_path = segment_path.join("tokens.dict");
    let postings_path = segment_path.join("tokens.postings");

    let mut dict_file = BufWriter::with_capacity(BUF_CAPACITY, File::create(&dict_path)?);
    let mut postings_file = BufWriter::with_capacity(BUF_CAPACITY, File::create(&postings_path)?);

    let mut positions_file = if positions.is_some() {
        let positions_path = segment_path.join("tokens.positions");
        Some(BufWriter::with_capacity(
            BUF_CAPACITY,
            File::create(&positions_path)?,
        ))
    } else {
        None
    };

    // Write entry count
    dict_file.write_all(&(postings.len() as u32).to_le_bytes())?;

    let mut postings_offset: u64 = 0;
    let mut pos_offset: u64 = 0;
    let mut encoded = Vec::new();

    for (token, doc_ids) in postings {
        encoded.clear();
        let doc_freq = encode_postings(doc_ids, &mut encoded);

        // Write token (length-prefixed)
        let token_bytes = token.as_bytes();
        dict_file.write_all(&(token_bytes.len() as u16).to_le_bytes())?;
        dict_file.write_all(token_bytes)?;

        // Write offset, length, freq
        dict_file.write_all(&postings_offset.to_le_bytes())?;
        dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
        dict_file.write_all(&(doc_freq as u32).to_le_bytes())?;

        // Write position offset and length (a zero length means "no
        // position data for this token"; readers ignore the offset then)
        if let Some(ref mut pf) = positions_file {
            let mut pos_encoded = Vec::new();
            if let Some(doc_pos_map) = positions.and_then(|p| p.get(token)) {
                let refs: Vec<(u32, &[u32])> = doc_pos_map
                    .iter()
                    .map(|(&d, p)| (d, p.as_slice()))
                    .collect();
                encode_position_postings(&refs, &mut pos_encoded);
            }

            dict_file.write_all(&pos_offset.to_le_bytes())?;
            dict_file.write_all(&(pos_encoded.len() as u32).to_le_bytes())?;

            if !pos_encoded.is_empty() {
                pf.write_all(&pos_encoded)?;
                pos_offset += pos_encoded.len() as u64;
            }
        }

        // Write postings
        postings_file.write_all(&encoded)?;
        postings_offset += encoded.len() as u64;
    }

    dict_file.flush()?;
    postings_file.flush()?;
    if let Some(ref mut pf) = positions_file {
        pf.flush()?;
    }
    Ok(())
}

/// Write linemap.bin.
pub fn write_line_maps(
    segment_path: &Path,
    line_maps: &std::collections::HashMap<DocId, Vec<u32>>,
) -> Result<()> {
    let linemap_path = segment_path.join("linemap.bin");
    let mut file = BufWriter::with_capacity(BUF_CAPACITY, File::create(&linemap_path)?);

    // Write count
    file.write_all(&(line_maps.len() as u32).to_le_bytes())?;

    // Sort by doc_id for consistent ordering
    let mut sorted: Vec<_> = line_maps.iter().collect();
    sorted.sort_by_key(|(id, _)| *id);

    let mut encoded = Vec::new();
    for (&doc_id, offsets) in sorted {
        file.write_all(&doc_id.to_le_bytes())?;
        file.write_all(&(offsets.len() as u32).to_le_bytes())?;

        encoded.clear();
        delta_encode(offsets, &mut encoded);
        file.write_all(&(encoded.len() as u32).to_le_bytes())?;
        file.write_all(&encoded)?;
    }

    file.flush()?;
    Ok(())
}

/// Write bloom.bin from an already-built filter.
pub fn write_bloom_file(segment_path: &Path, bloom_filter: &BloomFilter) -> Result<()> {
    let bloom_path = segment_path.join("bloom.bin");
    let mut file = BufWriter::with_capacity(65536, File::create(&bloom_path)?);

    file.write_all(&[bloom_filter.num_hashes()])?;
    let bits = bloom_filter.bits();
    file.write_all(&(bits.len() as u32).to_le_bytes())?;
    for &word in bits {
        file.write_all(&word.to_le_bytes())?;
    }

    file.flush()?;
    Ok(())
}

/// Build a bloom filter over a segment's trigrams and write bloom.bin.
pub fn build_and_write_bloom(
    segment_path: &Path,
    trigrams: impl ExactSizeIterator<Item = Trigram>,
    min_capacity: usize,
) -> Result<()> {
    let mut bloom_filter = BloomFilter::new(trigrams.len().max(min_capacity), 0.01);
    for trigram in trigrams {
        bloom_filter.insert(trigram);
    }
    write_bloom_file(segment_path, &bloom_filter)
}
