//! Segment compaction and merging.
//!
//! This module implements segment merging to prevent index fragmentation from
//! delta segment accumulation. Merging reuses stored postings; optional source
//! packs may still require source reads. Performance depends on corpus and layout.

use crate::index::reader::MappedBytes;
use crate::index::reader::{read_documents, read_paths};
use crate::index::segment_io;
use crate::index::types::*;
use crate::index::writer::{write_documents_atomic, write_meta_atomic, write_paths_atomic};
use crate::utils::{decode_position_postings, delta_decode, find_codebase_root};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Result of building doc_id remapping
struct DocIdRemapping {
    /// Maps old doc_id -> new contiguous doc_id, densely indexed by old id
    /// (doc ids start at 1; 0 = tombstoned/unknown). Probed once per posting
    /// element during merge, so this must be an O(1) array lookup, not a hash.
    old_to_new: DocRemap,
    /// Valid documents with remapped IDs
    valid_docs: Vec<Document>,
    /// Valid paths (deduplicated)
    valid_paths: Vec<PathBuf>,
    /// Maps old path_id -> new path_id (used during remapping)
    #[allow(dead_code)]
    path_id_remap: HashMap<PathId, PathId>,
}

enum DocRemap {
    Dense(Vec<DocId>),
    Sparse(HashMap<DocId, DocId>),
}
impl DocRemap {
    fn insert(&mut self, old: DocId, new: DocId) {
        match self {
            Self::Dense(ids) => ids[old as usize] = new,
            Self::Sparse(ids) => {
                ids.insert(old, new);
            }
        }
    }
}
impl DocIdRemapping {
    fn contains(&self, old_id: DocId) -> bool {
        match &self.old_to_new {
            DocRemap::Dense(ids) => ids.get(old_id as usize).is_some_and(|id| *id != u32::MAX),
            DocRemap::Sparse(ids) => ids.contains_key(&old_id),
        }
    }

    #[inline]
    fn remap(&self, old_id: DocId) -> Option<DocId> {
        let id = match &self.old_to_new {
            DocRemap::Dense(ids) => ids.get(old_id as usize).copied(),
            DocRemap::Sparse(ids) => ids.get(&old_id).copied(),
        };
        id.filter(|id| *id != 0 && *id != u32::MAX)
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
    // Pin and validate every required segment before mutation. In particular,
    // missing postings must never be interpreted as an empty segment.
    let validated = crate::index::reader::IndexReader::open(&root)?;
    let index_path = validated.generation_path().to_path_buf();

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

    // Keep every available posting. Only legacy/configured omissions must
    // remain marked: compaction cannot reconstruct previously omitted data.
    let stop_grams: HashSet<_> = meta.stop_grams.iter().copied().collect();

    // Step 4: Write merged segment atomically
    let mut generation = crate::index::generation::Generation::new(&root)?;
    let new_segment_id: SegmentId = 1;
    let segments_path = generation.path.join("segments");
    let new_segment_path = segments_path.join(format!("seg_{:04}", new_segment_id));

    // Create new segment directory
    fs::create_dir_all(&new_segment_path)?;

    // Write segment files. The merged segment drops stop-grams entirely:
    // they can't narrow and the executor never looks them up.
    segment_io::write_trigram_index(&new_segment_path, &trigram_postings, Some(&stop_grams))?;
    segment_io::write_token_index(
        &new_segment_path,
        &token_postings,
        if has_positions {
            Some(&token_positions)
        } else {
            None
        },
    )?;
    segment_io::write_line_maps(&new_segment_path, &line_maps)?;
    segment_io::build_and_write_bloom(&new_segment_path, trigram_postings.keys().copied(), 10000)?;
    eprintln!("  Wrote merged segment to seg_{:04}", new_segment_id);

    // Step 5: Write global files atomically
    // docs.bin.tmp -> docs.bin
    write_documents_atomic(&generation.path, &remapping.valid_docs)?;

    // paths.bin.tmp -> paths.bin
    write_paths_atomic(&generation.path, &remapping.valid_paths)?;

    // Step 6: Update and write meta.json (commits the transaction)
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let new_meta = IndexMeta {
        version: 2,
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
        // Compaction merges segments; the rejected-file scan cache is
        // unaffected and must survive the meta rewrite
        rejected_files: meta.rejected_files,
    };
    write_meta_atomic(&generation.path, &new_meta)?;
    crate::index::source_pack::write_missing(
        &generation.path,
        &root,
        &remapping.valid_docs,
        &remapping.valid_paths,
        crate::index::source_pack::requested() || crate::index::source_pack::present(&index_path),
    )?;
    // All input bytes have been consumed. Release our validation lease before
    // publication collects retired generations; genuine external readers keep
    // their own leases and remain protected.
    drop(validated);
    generation.publish()?;
    eprintln!("  Updated meta.json");

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
    let mut old_to_new = if u64::from(max_doc_id) <= (documents.len() as u64).saturating_mul(4) {
        DocRemap::Dense(vec![u32::MAX; max_doc_id as usize + 1])
    } else {
        DocRemap::Sparse(HashMap::with_capacity(documents.len()))
    };
    let mut valid_docs = Vec::new();
    let mut path_id_remap: HashMap<PathId, PathId> = HashMap::new();
    let mut valid_paths = Vec::new();
    let mut next_doc_id: DocId = 1;

    for doc in documents {
        old_to_new.insert(doc.doc_id, 0);
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

            old_to_new.insert(doc.doc_id, next_doc_id);

            let mut new_doc = doc.clone();
            new_doc.doc_id = next_doc_id;
            new_doc.path_id = new_path_id;
            new_doc.segment_id = 1; // All docs go to merged segment
            valid_docs.push(new_doc);

            next_doc_id = next_doc_id
                .checked_add(1)
                .context("Document ID capacity exhausted")?;
        }
    }

    Ok(DocIdRemapping {
        old_to_new,
        valid_docs,
        valid_paths,
        path_id_remap,
    })
}

/// Token -> doc -> positions postings, as merged during compaction
type PositionPostings = BTreeMap<String, BTreeMap<DocId, Vec<u32>>>;

/// All merged segment data: trigram postings, token postings, line maps,
/// position postings, and whether every segment had position data
type MergedSegments = (
    BTreeMap<Trigram, Vec<DocId>>,
    BTreeMap<String, Vec<DocId>>,
    HashMap<DocId, Vec<u32>>,
    PositionPostings,
    bool,
);

/// Merge postings from all segments, remapping doc_ids.
fn merge_all_segments(
    index_path: &Path,
    segment_ids: &[SegmentId],
    remapping: &DocIdRemapping,
) -> Result<MergedSegments> {
    let mut merged_trigrams: BTreeMap<Trigram, Vec<DocId>> = BTreeMap::new();
    let mut merged_tokens: BTreeMap<String, Vec<DocId>> = BTreeMap::new();
    let mut merged_line_maps: HashMap<DocId, Vec<u32>> = HashMap::new();
    let mut merged_positions: BTreeMap<String, BTreeMap<DocId, Vec<u32>>> = BTreeMap::new();
    let mut all_have_positions = true;

    let segments_path = index_path.join("segments");

    for &seg_id in segment_ids {
        let segment_path = segments_path.join(format!("seg_{:04}", seg_id));
        anyhow::ensure!(segment_path.is_dir(), "Missing required segment");

        // Merge trigram postings
        merge_trigram_segment(&segment_path, &mut merged_trigrams, remapping)?;

        // Merge token postings
        merge_token_segment(&segment_path, &mut merged_tokens, remapping)?;

        // Merge line maps
        merge_line_maps_segment(&segment_path, &mut merged_line_maps, remapping)?;

        // Merge position data
        let positions_path = segment_path.join("tokens.positions");
        if positions_path.exists() {
            merge_token_positions_segment(&segment_path, &mut merged_positions, remapping)?;
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

    anyhow::ensure!(
        dict_path.is_file() && postings_path.is_file(),
        "Missing required segment postings"
    );

    // Read dictionary
    let mut dict_file = BufReader::new(File::open(&dict_path)?);
    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];

    dict_file.read_exact(&mut buf4)?;
    let entry_count = u32::from_le_bytes(buf4) as usize;

    // mmap postings file
    let postings_mmap = MappedBytes::open(&postings_path)?;

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

        // Stored frequency must agree with a complete, valid posting list.
        dict_file.read_exact(&mut buf4)?;
        let doc_freq = u32::from_le_bytes(buf4) as usize;

        // Decode posting list
        let end = offset
            .checked_add(length)
            .context("Posting range overflow")?;
        anyhow::ensure!(
            end <= postings_mmap.len(),
            "Posting range exceeds file bounds"
        );
        crate::utils::encoding::validate_delta_stream(&postings_mmap[offset..end])?;
        {
            // Remap doc_ids, filtering out tombstoned docs
            let decoded = delta_decode(&postings_mmap[offset..end]);
            anyhow::ensure!(
                decoded.len() == doc_freq
                    && decoded.iter().all(|&id| id != 0 && remapping.contains(id))
                    && decoded.windows(2).all(|pair| pair[0] < pair[1]),
                "Invalid posting document IDs or frequency"
            );
            let remapped: Vec<DocId> = decoded
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

    anyhow::ensure!(
        dict_path.is_file() && postings_path.is_file(),
        "Missing required segment postings"
    );

    // Check if this segment has positions (affects dict entry size)
    let has_positions = segment_path.join("tokens.positions").exists();

    let dictionary = MappedBytes::open(&dict_path)?;
    let header = super::token_dictionary::header(&dictionary, has_positions)?;
    let postings_mmap = MappedBytes::open(&postings_path)?;
    let mut cursor = header.start;
    for _ in 0..header.count {
        let (entry, consumed) =
            super::token_dictionary::entry(&dictionary[cursor..], header.compact, has_positions)?;
        cursor += consumed;
        let token = entry.token.to_owned();
        let offset =
            usize::try_from(entry.offset).context("Posting offset exceeds platform bounds")?;
        let length = entry.length as usize;
        let doc_freq = entry.doc_freq as usize;
        // Decode posting list
        let end = offset
            .checked_add(length)
            .context("Posting range overflow")?;
        anyhow::ensure!(
            end <= postings_mmap.len(),
            "Posting range exceeds file bounds"
        );
        crate::utils::encoding::validate_delta_stream(&postings_mmap[offset..end])?;
        {
            // Remap doc_ids, filtering out tombstoned docs
            let decoded = delta_decode(&postings_mmap[offset..end]);
            anyhow::ensure!(
                decoded.len() == doc_freq
                    && decoded.iter().all(|&id| id != 0 && remapping.contains(id))
                    && decoded.windows(2).all(|pair| pair[0] < pair[1]),
                "Invalid posting document IDs or frequency"
            );
            let remapped: Vec<DocId> = decoded
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
    for (old_doc_id, offsets) in crate::index::reader::read_line_maps(segment_path)? {
        anyhow::ensure!(remapping.contains(old_doc_id), "Unknown line map document");
        if let Some(new_doc_id) = remapping.remap(old_doc_id) {
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

    let dictionary = MappedBytes::open(&dict_path)?;
    let header = super::token_dictionary::header(&dictionary, true)?;
    let positions_mmap = MappedBytes::open(&positions_path)?;
    let mut cursor = header.start;
    for _ in 0..header.count {
        let (entry, consumed) =
            super::token_dictionary::entry(&dictionary[cursor..], header.compact, true)?;
        cursor += consumed;
        let token = entry.token.to_owned();
        let pos_offset =
            usize::try_from(entry.pos_offset).context("Position offset exceeds platform bounds")?;
        let pos_length = entry.pos_length as usize;

        if pos_length == 0 {
            continue;
        }

        // Decode position postings
        let end = pos_offset
            .checked_add(pos_length)
            .context("Position range overflow")?;
        anyhow::ensure!(
            end <= positions_mmap.len(),
            "Position range exceeds file bounds"
        );
        crate::utils::encoding::validate_position_stream(&positions_mmap[pos_offset..end])?;

        let doc_positions = decode_position_postings(&positions_mmap[pos_offset..end]);

        // Remap doc_ids and merge
        let token_entry = merged.entry(token).or_default();
        for (old_doc_id, positions) in doc_positions {
            anyhow::ensure!(remapping.contains(old_doc_id), "Unknown position document");
            if let Some(new_doc_id) = remapping.remap(old_doc_id) {
                token_entry.entry(new_doc_id).or_default().extend(positions);
            }
        }
    }

    Ok(())
}

/// Legacy compact function - now calls merge_segments.
pub fn compact_segments(root_path: &Path) -> Result<()> {
    merge_segments(root_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for i in 0..6 {
            fs::write(root.join(format!("{i}.rs")), "auditneedle token phrase\n").unwrap();
        }
        crate::index::build::build_index_with_chunk_size(&root, true, Some(2)).unwrap();
        let generation = crate::utils::get_index_dir(&root).unwrap();
        (dir, root, generation)
    }

    #[test]
    fn malformed_compaction_inputs_never_publish_partial_results() {
        for damage in [
            "missing_grams",
            "missing_tokens",
            "missing_positions",
            "gram_range",
            "gram_varint",
            "token_utf8",
            "position_count",
            "line_count",
            "sparse_document",
        ] {
            let (_dir, root, generation) = fixture();
            let segment = generation.join("segments/seg_0001");
            match damage {
                "missing_grams" => fs::remove_file(segment.join("grams.dict")).unwrap(),
                "missing_tokens" => fs::remove_file(segment.join("tokens.postings")).unwrap(),
                "missing_positions" => fs::remove_file(segment.join("tokens.positions")).unwrap(),
                "gram_range" => {
                    let path = segment.join("grams.dict");
                    let mut bytes = fs::read(&path).unwrap();
                    bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
                    fs::write(path, bytes).unwrap();
                }
                "gram_varint" => {
                    let path = segment.join("grams.postings");
                    let mut bytes = fs::read(&path).unwrap();
                    bytes.fill(0x80);
                    fs::write(path, bytes).unwrap();
                }
                "token_utf8" => {
                    let path = segment.join("tokens.dict");
                    let mut bytes = fs::read(&path).unwrap();
                    bytes[6] = 0xff;
                    fs::write(path, bytes).unwrap();
                }
                "position_count" => {
                    let path = segment.join("tokens.positions");
                    let mut bytes = fs::read(&path).unwrap();
                    // A complete but impossible count, bounded by its token's byte range.
                    bytes[1] = 127;
                    fs::write(path, bytes).unwrap();
                }
                "line_count" => {
                    let path = segment.join("linemap.bin");
                    let mut bytes = fs::read(&path).unwrap();
                    bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
                    fs::write(path, bytes).unwrap();
                }
                "sparse_document" => {
                    let path = generation.join("docs.bin");
                    let mut bytes = fs::read(&path).unwrap();
                    // Duplicate IDs must be rejected before constructing any remapping.
                    let duplicate: [u8; 4] = bytes[4..8].try_into().unwrap();
                    bytes[34..38].copy_from_slice(&duplicate);
                    fs::write(path, bytes).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(merge_segments(&root).is_err(), "accepted {damage}");
            assert_eq!(
                crate::utils::get_index_dir(&root).unwrap(),
                generation,
                "published {damage}"
            );
            crate::utils::remove_index(&root).unwrap();
        }
    }

    #[test]
    fn sparse_maximum_document_id_uses_bounded_remapping() {
        let (_dir, root, generation) = fixture();
        let mut docs = read_documents(&generation).unwrap();
        docs[0].doc_id = u32::MAX;
        write_documents_atomic(&generation, &docs).unwrap();
        let remap = build_doc_id_remapping(&generation).unwrap();
        assert!(matches!(remap.old_to_new, DocRemap::Sparse(_)));
        assert_eq!(remap.remap(u32::MAX), Some(1));
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn compact_accepts_empty_posting_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for i in 0..4 {
            fs::write(root.join(format!("{i}.txt")), "a").unwrap();
        }
        crate::index::build::build_index_with_chunk_size(&root, true, Some(1)).unwrap();
        merge_segments(&root).unwrap();
        assert_eq!(
            crate::index::reader::IndexReader::open(&root)
                .unwrap()
                .valid_doc_ids()
                .len(),
            4
        );
        crate::utils::remove_index(&root).unwrap();
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
