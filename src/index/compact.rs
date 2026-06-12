//! Segment compaction and merging.
//!
//! This module implements segment merging to prevent index fragmentation from
//! delta segment accumulation. Segment merging is 60-100x faster than full
//! rebuild because it only reads/merges existing index data, avoiding expensive
//! source file I/O.

use crate::index::reader::{read_documents, read_paths};
use crate::index::types::*;
use crate::index::writer::{write_documents_atomic, write_meta_atomic, write_paths_atomic};
use crate::utils::{
    decode_position_postings, delta_decode, delta_encode, encode_position_postings,
    find_codebase_root, get_index_dir, BloomFilter,
};
use anyhow::{Context, Result};
use memmap2::Mmap;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Result of building doc_id remapping
struct DocIdRemapping {
    /// Maps old doc_id -> new contiguous doc_id, densely indexed by old id
    /// (doc ids start at 1; 0 = tombstoned/unknown). Probed once per posting
    /// element during merge, so this must be an O(1) array lookup, not a hash.
    old_to_new: Vec<DocId>,
    /// Valid documents with remapped IDs
    valid_docs: Vec<Document>,
    /// Valid paths (deduplicated)
    valid_paths: Vec<PathBuf>,
    /// Maps old path_id -> new path_id (used during remapping)
    #[allow(dead_code)]
    path_id_remap: HashMap<PathId, PathId>,
}

impl DocIdRemapping {
    /// Remap an old doc_id; None if tombstoned or unknown.
    #[inline]
    fn remap(&self, old_id: DocId) -> Option<DocId> {
        match self.old_to_new.get(old_id as usize) {
            Some(&new_id) if new_id != 0 => Some(new_id),
            _ => None,
        }
    }
}

/// Merge all segments into a single compacted segment.
///
/// This function:
/// 1. Builds doc_id remapping (old -> new contiguous IDs, skip tombstones)
/// 2. Reads and merges trigram/token postings from all segments
/// 3. Computes stop-grams from merged frequencies
/// 4. Writes merged segment atomically
/// 5. Updates docs.bin, paths.bin, meta.json
/// 6. Deletes old segments after meta commit
pub fn merge_segments(root_path: &Path) -> Result<()> {
    let root = find_codebase_root(root_path)?;
    let index_path = get_index_dir(&root)?;

    if !index_path.exists() {
        anyhow::bail!("No index found. Run 'fxi index' first.");
    }

    // Read current metadata
    let meta_path = index_path.join("meta.json");
    let meta_file = File::open(&meta_path).context("Failed to open meta.json")?;
    let meta: IndexMeta = serde_json::from_reader(meta_file)?;

    // Collect all segment IDs
    let mut segment_ids: Vec<SegmentId> = Vec::new();
    if let Some(base_id) = meta.base_segment {
        segment_ids.push(base_id);
    }
    segment_ids.extend(&meta.delta_segments);

    if segment_ids.is_empty() {
        eprintln!("No segments to merge.");
        return Ok(());
    }

    if segment_ids.len() == 1 && meta.tombstone_count == 0 {
        eprintln!("Only one segment with no tombstones, nothing to merge.");
        return Ok(());
    }

    eprintln!(
        "Merging {} segments ({} docs, {} tombstones)...",
        segment_ids.len(),
        meta.doc_count,
        meta.tombstone_count
    );

    // Step 1: Build doc_id remapping
    let remapping = build_doc_id_remapping(&index_path)?;
    eprintln!(
        "  Remapped {} valid docs (skipped {} tombstones)",
        remapping.valid_docs.len(),
        meta.doc_count as usize - remapping.valid_docs.len()
    );

    // Step 2: Merge all segment postings
    let (trigram_postings, token_postings, line_maps, token_positions, has_positions) =
        merge_all_segments(&index_path, &segment_ids, &remapping)?;
    eprintln!(
        "  Merged {} trigrams, {} tokens{}",
        trigram_postings.len(),
        token_postings.len(),
        if has_positions {
            format!(", {} tokens with positions", token_positions.len())
        } else {
            String::new()
        }
    );

    // Step 3: Compute stop-grams from merged frequencies
    let stop_grams = compute_stop_grams(&trigram_postings, remapping.valid_docs.len(), 512);
    eprintln!("  Computed {} stop-grams", stop_grams.len());

    // Step 4: Write merged segment atomically
    let new_segment_id: SegmentId = 1;
    let segments_path = index_path.join("segments");
    let new_segment_path = segments_path.join(format!("seg_{:04}", new_segment_id));

    // Create new segment directory
    fs::create_dir_all(&new_segment_path)?;

    // Write segment files
    write_trigram_index(&new_segment_path, &trigram_postings, &stop_grams)?;
    write_token_index_with_positions(
        &new_segment_path,
        &token_postings,
        if has_positions {
            Some(&token_positions)
        } else {
            None
        },
    )?;
    write_line_maps(&new_segment_path, &line_maps)?;
    write_bloom_filter(&new_segment_path, &trigram_postings)?;
    eprintln!("  Wrote merged segment to seg_{:04}", new_segment_id);

    // Step 5: Write global files atomically
    // docs.bin.tmp -> docs.bin
    write_documents_atomic(&index_path, &remapping.valid_docs)?;

    // paths.bin.tmp -> paths.bin
    write_paths_atomic(&index_path, &remapping.valid_paths)?;

    // Step 6: Update and write meta.json (commits the transaction)
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let new_meta = IndexMeta {
        version: meta.version,
        root_path: meta.root_path,
        doc_count: remapping.valid_docs.len() as u32,
        segment_count: 1,
        base_segment: Some(new_segment_id),
        delta_segments: Vec::new(),
        stop_grams: stop_grams.iter().copied().collect(),
        created_at: meta.created_at,
        updated_at: now,
        tombstone_count: 0,
        valid_doc_count: remapping.valid_docs.len() as u32,
        delta_baseline: 0, // Reset after merge - all segments consolidated
        has_positions,
    };
    write_meta_atomic(&index_path, &new_meta)?;
    eprintln!("  Updated meta.json");

    // Step 7: Delete old segments (safe now that meta is committed)
    for seg_id in &segment_ids {
        if *seg_id == new_segment_id {
            continue; // Don't delete our new segment
        }
        let old_seg_path = segments_path.join(format!("seg_{:04}", seg_id));
        if old_seg_path.exists() {
            if let Err(e) = fs::remove_dir_all(&old_seg_path) {
                eprintln!("  Warning: failed to remove old segment {}: {}", seg_id, e);
            }
        }
    }

    eprintln!(
        "Merge complete: {} docs in 1 segment (was {} segments)",
        remapping.valid_docs.len(),
        segment_ids.len()
    );

    Ok(())
}

/// Build doc_id remapping from old IDs to new contiguous IDs.
/// Skips tombstoned and stale documents.
fn build_doc_id_remapping(index_path: &Path) -> Result<DocIdRemapping> {
    let documents = read_documents(index_path)?;
    let paths = read_paths(index_path)?;

    let max_doc_id = documents.iter().map(|d| d.doc_id).max().unwrap_or(0);
    let mut old_to_new: Vec<DocId> = vec![0; max_doc_id as usize + 1];
    let mut valid_docs = Vec::new();
    let mut path_id_remap: HashMap<PathId, PathId> = HashMap::new();
    let mut valid_paths = Vec::new();
    let mut next_doc_id: DocId = 1;

    for doc in documents {
        if doc.is_valid() {
            // Get or create new path_id
            let new_path_id = if let Some(&existing) = path_id_remap.get(&doc.path_id) {
                existing
            } else {
                let new_id = valid_paths.len() as PathId;
                if let Some(path) = paths.get(doc.path_id as usize) {
                    valid_paths.push(path.clone());
                    path_id_remap.insert(doc.path_id, new_id);
                    new_id
                } else {
                    continue; // Skip docs with invalid path_id
                }
            };

            old_to_new[doc.doc_id as usize] = next_doc_id;

            let mut new_doc = doc.clone();
            new_doc.doc_id = next_doc_id;
            new_doc.path_id = new_path_id;
            new_doc.segment_id = 1; // All docs go to merged segment
            valid_docs.push(new_doc);

            next_doc_id += 1;
        }
    }

    Ok(DocIdRemapping {
        old_to_new,
        valid_docs,
        valid_paths,
        path_id_remap,
    })
}

/// Merge postings from all segments, remapping doc_ids.
fn merge_all_segments(
    index_path: &Path,
    segment_ids: &[SegmentId],
    remapping: &DocIdRemapping,
) -> Result<(
    BTreeMap<Trigram, Vec<DocId>>,
    BTreeMap<String, Vec<DocId>>,
    HashMap<DocId, Vec<u32>>,
    BTreeMap<String, BTreeMap<DocId, Vec<u32>>>,
    bool,
)> {
    let mut merged_trigrams: BTreeMap<Trigram, Vec<DocId>> = BTreeMap::new();
    let mut merged_tokens: BTreeMap<String, Vec<DocId>> = BTreeMap::new();
    let mut merged_line_maps: HashMap<DocId, Vec<u32>> = HashMap::new();
    let mut merged_positions: BTreeMap<String, BTreeMap<DocId, Vec<u32>>> = BTreeMap::new();
    let mut all_have_positions = true;

    let segments_path = index_path.join("segments");

    for &seg_id in segment_ids {
        let segment_path = segments_path.join(format!("seg_{:04}", seg_id));
        if !segment_path.exists() {
            continue;
        }

        // Merge trigram postings
        merge_trigram_segment(&segment_path, &mut merged_trigrams, remapping)?;

        // Merge token postings
        merge_token_segment(&segment_path, &mut merged_tokens, remapping)?;

        // Merge line maps
        merge_line_maps_segment(&segment_path, &mut merged_line_maps, remapping)?;

        // Merge position data
        let positions_path = segment_path.join("tokens.positions");
        if positions_path.exists() {
            merge_token_positions_segment(
                &segment_path,
                &mut merged_positions,
                remapping,
            )?;
        } else {
            all_have_positions = false;
        }
    }

    // Sort and deduplicate all posting lists
    for postings in merged_trigrams.values_mut() {
        postings.sort_unstable();
        postings.dedup();
    }

    for postings in merged_tokens.values_mut() {
        postings.sort_unstable();
        postings.dedup();
    }

    Ok((
        merged_trigrams,
        merged_tokens,
        merged_line_maps,
        merged_positions,
        all_have_positions,
    ))
}

/// Merge trigram postings from a single segment.
fn merge_trigram_segment(
    segment_path: &Path,
    merged: &mut BTreeMap<Trigram, Vec<DocId>>,
    remapping: &DocIdRemapping,
) -> Result<()> {
    let dict_path = segment_path.join("grams.dict");
    let postings_path = segment_path.join("grams.postings");

    if !dict_path.exists() || !postings_path.exists() {
        return Ok(());
    }

    // Read dictionary
    let mut dict_file = BufReader::new(File::open(&dict_path)?);
    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];

    dict_file.read_exact(&mut buf4)?;
    let entry_count = u32::from_le_bytes(buf4) as usize;

    // mmap postings file
    let postings_file = File::open(&postings_path)?;
    let postings_mmap = unsafe { Mmap::map(&postings_file)? };

    for _ in 0..entry_count {
        // Read trigram
        dict_file.read_exact(&mut buf4)?;
        let trigram = u32::from_le_bytes(buf4);

        // Read offset
        dict_file.read_exact(&mut buf8)?;
        let offset = u64::from_le_bytes(buf8) as usize;

        // Read length
        dict_file.read_exact(&mut buf4)?;
        let length = u32::from_le_bytes(buf4) as usize;

        // Read doc_freq (skip)
        dict_file.read_exact(&mut buf4)?;

        // Decode posting list
        if offset + length <= postings_mmap.len() {
            // Remap doc_ids, filtering out tombstoned docs
            let remapped: Vec<DocId> = delta_decode(&postings_mmap[offset..offset + length])
                .into_iter()
                .filter_map(|old_id| remapping.remap(old_id))
                .collect();

            if !remapped.is_empty() {
                // One map lookup per posting list, not per element; the first
                // segment to contribute a trigram moves its list in wholesale
                match merged.entry(trigram) {
                    std::collections::btree_map::Entry::Vacant(e) => {
                        e.insert(remapped);
                    }
                    std::collections::btree_map::Entry::Occupied(mut e) => {
                        e.get_mut().extend_from_slice(&remapped);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Merge token postings from a single segment.
fn merge_token_segment(
    segment_path: &Path,
    merged: &mut BTreeMap<String, Vec<DocId>>,
    remapping: &DocIdRemapping,
) -> Result<()> {
    let dict_path = segment_path.join("tokens.dict");
    let postings_path = segment_path.join("tokens.postings");

    if !dict_path.exists() || !postings_path.exists() {
        return Ok(());
    }

    // Check if this segment has positions (affects dict entry size)
    let has_positions = segment_path.join("tokens.positions").exists();

    // Read dictionary
    let mut dict_file = BufReader::new(File::open(&dict_path)?);
    let mut buf2 = [0u8; 2];
    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];

    dict_file.read_exact(&mut buf4)?;
    let entry_count = u32::from_le_bytes(buf4) as usize;

    // mmap postings file
    let postings_file = File::open(&postings_path)?;
    let postings_mmap = unsafe { Mmap::map(&postings_file)? };

    for _ in 0..entry_count {
        // Read token length
        dict_file.read_exact(&mut buf2)?;
        let token_len = u16::from_le_bytes(buf2) as usize;

        // Read token (moves the byte buffer when valid UTF-8 — no copy)
        let mut token_bytes = vec![0u8; token_len];
        dict_file.read_exact(&mut token_bytes)?;
        let token = String::from_utf8(token_bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());

        // Read offset
        dict_file.read_exact(&mut buf8)?;
        let offset = u64::from_le_bytes(buf8) as usize;

        // Read length
        dict_file.read_exact(&mut buf4)?;
        let length = u32::from_le_bytes(buf4) as usize;

        // Read doc_freq (skip)
        dict_file.read_exact(&mut buf4)?;

        // Skip position offset/length if present
        if has_positions {
            dict_file.read_exact(&mut buf8)?; // pos_offset
            dict_file.read_exact(&mut buf4)?; // pos_length
        }

        // Decode posting list
        if offset + length <= postings_mmap.len() {
            // Remap doc_ids, filtering out tombstoned docs
            let remapped: Vec<DocId> = delta_decode(&postings_mmap[offset..offset + length])
                .into_iter()
                .filter_map(|old_id| remapping.remap(old_id))
                .collect();

            if !remapped.is_empty() {
                // The token String is moved into the map once instead of
                // cloned per posting element
                match merged.entry(token) {
                    std::collections::btree_map::Entry::Vacant(e) => {
                        e.insert(remapped);
                    }
                    std::collections::btree_map::Entry::Occupied(mut e) => {
                        e.get_mut().extend_from_slice(&remapped);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Merge line maps from a single segment.
fn merge_line_maps_segment(
    segment_path: &Path,
    merged: &mut HashMap<DocId, Vec<u32>>,
    remapping: &DocIdRemapping,
) -> Result<()> {
    let linemap_path = segment_path.join("linemap.bin");

    if !linemap_path.exists() {
        return Ok(());
    }

    let mut file = BufReader::new(File::open(&linemap_path)?);
    let mut buf4 = [0u8; 4];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    for _ in 0..count {
        // Read doc_id
        file.read_exact(&mut buf4)?;
        let old_doc_id = u32::from_le_bytes(buf4);

        // Read line count (skip)
        file.read_exact(&mut buf4)?;

        // Read encoded length
        file.read_exact(&mut buf4)?;
        let encoded_len = u32::from_le_bytes(buf4) as usize;

        // Read encoded data
        let mut encoded = vec![0u8; encoded_len];
        file.read_exact(&mut encoded)?;

        // Only keep if doc is still valid
        if let Some(new_doc_id) = remapping.remap(old_doc_id) {
            let offsets = delta_decode(&encoded);
            merged.insert(new_doc_id, offsets);
        }
    }

    Ok(())
}

/// Merge token position data from a single segment.
/// Reads the token dict (with position offsets) and the tokens.positions file,
/// then remaps doc_ids and merges into the accumulator.
fn merge_token_positions_segment(
    segment_path: &Path,
    merged: &mut BTreeMap<String, BTreeMap<DocId, Vec<u32>>>,
    remapping: &DocIdRemapping,
) -> Result<()> {
    let dict_path = segment_path.join("tokens.dict");
    let positions_path = segment_path.join("tokens.positions");

    if !dict_path.exists() || !positions_path.exists() {
        return Ok(());
    }

    // Read dictionary
    let mut dict_file = BufReader::new(File::open(&dict_path)?);
    let mut buf2 = [0u8; 2];
    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];

    dict_file.read_exact(&mut buf4)?;
    let entry_count = u32::from_le_bytes(buf4) as usize;

    // mmap positions file
    let positions_file = File::open(&positions_path)?;
    let positions_mmap = unsafe { Mmap::map(&positions_file)? };

    for _ in 0..entry_count {
        // Read token length
        dict_file.read_exact(&mut buf2)?;
        let token_len = u16::from_le_bytes(buf2) as usize;

        // Read token
        let mut token_bytes = vec![0u8; token_len];
        dict_file.read_exact(&mut token_bytes)?;
        let token = String::from_utf8_lossy(&token_bytes).to_string();

        // Read postings offset, length, doc_freq (skip for position merging)
        dict_file.read_exact(&mut buf8)?; // offset
        dict_file.read_exact(&mut buf4)?; // length
        dict_file.read_exact(&mut buf4)?; // doc_freq

        // Read position offset and length
        dict_file.read_exact(&mut buf8)?;
        let pos_offset = u64::from_le_bytes(buf8) as usize;
        dict_file.read_exact(&mut buf4)?;
        let pos_length = u32::from_le_bytes(buf4) as usize;

        if pos_length == 0 {
            continue;
        }

        // Decode position postings
        let end = pos_offset + pos_length;
        if end > positions_mmap.len() {
            continue;
        }

        let doc_positions = decode_position_postings(&positions_mmap[pos_offset..end]);

        // Remap doc_ids and merge
        let token_entry = merged.entry(token).or_default();
        for (old_doc_id, positions) in doc_positions {
            if let Some(new_doc_id) = remapping.remap(old_doc_id) {
                token_entry
                    .entry(new_doc_id)
                    .or_default()
                    .extend(positions);
            }
        }
    }

    Ok(())
}

/// Compute stop-grams from merged trigram frequencies.
fn compute_stop_grams(
    trigram_postings: &BTreeMap<Trigram, Vec<DocId>>,
    doc_count: usize,
    count: usize,
) -> HashSet<Trigram> {
    let freq: Vec<_> = trigram_postings
        .iter()
        .map(|(&t, v)| (t, v.len()))
        .collect();

    crate::index::writer::select_stop_grams(freq, doc_count, count)
}

/// Write trigram index files.
fn write_trigram_index(
    segment_path: &Path,
    postings: &BTreeMap<Trigram, Vec<DocId>>,
    stop_grams: &HashSet<Trigram>,
) -> Result<()> {
    let dict_path = segment_path.join("grams.dict");
    let postings_path = segment_path.join("grams.postings");

    let mut dict_file = BufWriter::with_capacity(131072, File::create(&dict_path)?);
    let mut postings_file = BufWriter::with_capacity(131072, File::create(&postings_path)?);

    // Filter out stop-grams
    let filtered: Vec<_> = postings
        .iter()
        .filter(|(t, _)| !stop_grams.contains(t))
        .collect();

    // Write entry count
    dict_file.write_all(&(filtered.len() as u32).to_le_bytes())?;

    let mut postings_offset: u64 = 0;

    for (&trigram, doc_ids) in filtered {
        // Delta-encode
        let mut encoded = Vec::with_capacity(doc_ids.len() * 2);
        delta_encode(doc_ids, &mut encoded);

        // Write dictionary entry
        dict_file.write_all(&trigram.to_le_bytes())?;
        dict_file.write_all(&postings_offset.to_le_bytes())?;
        dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
        dict_file.write_all(&(doc_ids.len() as u32).to_le_bytes())?;

        // Write postings
        postings_file.write_all(&encoded)?;
        postings_offset += encoded.len() as u64;
    }

    dict_file.flush()?;
    postings_file.flush()?;

    Ok(())
}

/// Write token index files with optional position data.
fn write_token_index_with_positions(
    segment_path: &Path,
    postings: &BTreeMap<String, Vec<DocId>>,
    token_positions: Option<&BTreeMap<String, BTreeMap<DocId, Vec<u32>>>>,
) -> Result<()> {
    let dict_path = segment_path.join("tokens.dict");
    let postings_path = segment_path.join("tokens.postings");

    let mut dict_file = BufWriter::with_capacity(131072, File::create(&dict_path)?);
    let mut postings_file = BufWriter::with_capacity(131072, File::create(&postings_path)?);

    // Write positions file if we have position data
    let mut positions_file = if token_positions.is_some() {
        let positions_path = segment_path.join("tokens.positions");
        Some(BufWriter::with_capacity(
            131072,
            File::create(&positions_path)?,
        ))
    } else {
        None
    };

    // Write entry count
    dict_file.write_all(&(postings.len() as u32).to_le_bytes())?;

    let mut postings_offset: u64 = 0;
    let mut pos_offset: u64 = 0;

    for (token, doc_ids) in postings {
        let token_bytes = token.as_bytes();

        // Delta-encode postings
        let mut encoded = Vec::with_capacity(doc_ids.len() * 2);
        delta_encode(doc_ids, &mut encoded);

        // Write token (length-prefixed)
        dict_file.write_all(&(token_bytes.len() as u16).to_le_bytes())?;
        dict_file.write_all(token_bytes)?;

        // Write offset, length, freq
        dict_file.write_all(&postings_offset.to_le_bytes())?;
        dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
        dict_file.write_all(&(doc_ids.len() as u32).to_le_bytes())?;

        // Write position data if available
        if let Some(ref mut pf) = positions_file {
            let mut pos_encoded = Vec::new();
            if let Some(positions) = token_positions.and_then(|tp| tp.get(token)) {
                // Sort by doc_id and encode
                let mut sorted: Vec<_> = positions
                    .iter()
                    .map(|(&doc_id, pos)| (doc_id, pos.as_slice()))
                    .collect();
                sorted.sort_by_key(|&(d, _)| d);
                encode_position_postings(&sorted, &mut pos_encoded);
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

/// Write line maps file.
fn write_line_maps(segment_path: &Path, line_maps: &HashMap<DocId, Vec<u32>>) -> Result<()> {
    let linemap_path = segment_path.join("linemap.bin");
    let mut file = BufWriter::with_capacity(65536, File::create(&linemap_path)?);

    // Write count
    file.write_all(&(line_maps.len() as u32).to_le_bytes())?;

    // Sort by doc_id for consistent ordering
    let mut sorted: Vec<_> = line_maps.iter().collect();
    sorted.sort_by_key(|(id, _)| *id);

    for (&doc_id, offsets) in sorted {
        file.write_all(&doc_id.to_le_bytes())?;
        file.write_all(&(offsets.len() as u32).to_le_bytes())?;

        let mut encoded = Vec::new();
        delta_encode(offsets, &mut encoded);
        file.write_all(&(encoded.len() as u32).to_le_bytes())?;
        file.write_all(&encoded)?;
    }

    file.flush()?;
    Ok(())
}

/// Write bloom filter for the merged segment.
fn write_bloom_filter(
    segment_path: &Path,
    trigram_postings: &BTreeMap<Trigram, Vec<DocId>>,
) -> Result<()> {
    let bloom_path = segment_path.join("bloom.bin");

    // Create bloom filter with all trigrams
    let estimated_trigrams = trigram_postings.len();
    let mut bloom_filter = BloomFilter::new(estimated_trigrams.max(10000), 0.01);

    for &trigram in trigram_postings.keys() {
        bloom_filter.insert(trigram);
    }

    // Write bloom filter
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

/// Legacy compact function - now calls merge_segments.
pub fn compact_segments(root_path: &Path) -> Result<()> {
    merge_segments(root_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_stop_grams() {
        let mut postings = BTreeMap::new();
        postings.insert(1u32, vec![1, 2, 3, 4, 5]); // in 5/6 docs
        postings.insert(2u32, vec![1, 2]); // in 2/6 docs
        postings.insert(3u32, vec![1, 2, 3, 4]); // in 4/6 docs
        postings.insert(4u32, vec![1]); // in 1/6 docs

        // Only trigrams present in more than half of the 6 documents qualify
        let stop_grams = compute_stop_grams(&postings, 6, 2);
        assert_eq!(stop_grams.len(), 2);
        assert!(stop_grams.contains(&1u32)); // 5/6 docs
        assert!(stop_grams.contains(&3u32)); // 4/6 docs

        // The cap still applies among qualifying trigrams
        let capped = compute_stop_grams(&postings, 6, 1);
        assert_eq!(capped.len(), 1);
        assert!(capped.contains(&1u32));

        // In a tiny index nothing is hot enough to stop
        let none = compute_stop_grams(&postings, 100, 2);
        assert!(none.is_empty());
    }

    #[test]
    fn test_merge_sorted_lists() {
        // Test that merged lists are sorted and deduplicated
        let mut merged: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        merged.entry(1).or_default().extend([1, 3, 5]);
        merged.entry(1).or_default().extend([2, 3, 4]);

        for postings in merged.values_mut() {
            postings.sort_unstable();
            postings.dedup();
        }

        assert_eq!(merged.get(&1).unwrap(), &vec![1, 2, 3, 4, 5]);
    }
}
