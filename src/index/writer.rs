use crate::index::build::ProcessedFile;
use crate::index::types::*;
#[allow(unused_imports)]
use crate::utils::{
    BloomFilter, delta_encode, extract_tokens, extract_trigrams, get_index_dir, is_binary,
    is_minified,
};
use anyhow::Result;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// File data with pre-assigned IDs, ready for background processing
struct AssignedFile {
    doc_id: DocId,
    trigrams: Vec<u32>,
    tokens: crate::utils::PackedTokens,
    line_offsets: Vec<u32>,
    /// (index into `tokens`, word_position)
    token_positions: Vec<(u32, u32)>,
}

/// Token postings in dictionary order, then document/position order.
struct InvertedTokens {
    postings: Vec<(u32, DocId)>,
    positions: Vec<(u32, DocId, u32)>,
    symbols: Vec<String>,
}

/// Invert the document-ordered occurrence stream with counting and stable
/// scatter. Sorting every occurrence repeats information already supplied by
/// document order; only the unique dictionary strings need comparison sorting.
fn invert_token_postings(files: &mut [AssignedFile]) -> InvertedTokens {
    debug_assert!(files.windows(2).all(|f| f[0].doc_id < f[1].doc_id));
    let mut ids = ahash::AHashMap::<String, u32>::new();
    let mut counts = Vec::<usize>::new();
    let mut position_counts = Vec::<usize>::new();
    let mut local_ids = Vec::with_capacity(files.len());
    for file in files.iter_mut() {
        let mut local = Vec::with_capacity(file.tokens.len());
        for token in std::mem::take(&mut file.tokens).iter() {
            let next = ids.len() as u32;
            let id = if let Some(&id) = ids.get(token) {
                id
            } else {
                ids.insert(token.to_owned(), next);
                next
            };
            if id == next {
                counts.push(0);
                position_counts.push(0);
            }
            counts[id as usize] += 1;
            local.push(id);
        }
        for &(idx, _) in &file.token_positions {
            position_counts[local[idx as usize] as usize] += 1;
        }
        local_ids.push(local);
    }
    let mut symbols = vec![String::new(); ids.len()];
    for (token, id) in ids {
        symbols[id as usize] = token;
    }
    let mut order: Vec<usize> = (0..symbols.len()).collect();
    order.par_sort_unstable_by(|&a, &b| symbols[a].cmp(&symbols[b]));
    let mut ranks = vec![0u32; symbols.len()];
    let mut offsets = vec![0usize; symbols.len()];
    let mut position_offsets = vec![0usize; symbols.len()];
    let (mut total, mut position_total) = (0, 0);
    for (rank, &id) in order.iter().enumerate() {
        ranks[id] = rank as u32;
        offsets[id] = total;
        position_offsets[id] = position_total;
        total += counts[id];
        position_total += position_counts[id];
    }
    let mut postings = vec![(0, 0); total];
    let mut positions = vec![(0, 0, 0); position_total];
    for (file, local) in files.iter_mut().zip(local_ids) {
        for &id in &local {
            let id = id as usize;
            postings[offsets[id]] = (ranks[id], file.doc_id);
            offsets[id] += 1;
        }
        // The tokenizer already emits increasing positions. Preserve support
        // for callers constructing ProcessedFile with unordered occurrences.
        if !file.token_positions.is_sorted_by_key(|&(_, pos)| pos) {
            file.token_positions.sort_unstable_by_key(|&(_, pos)| pos);
        }
        for (idx, pos) in std::mem::take(&mut file.token_positions) {
            let id = local[idx as usize] as usize;
            positions[position_offsets[id]] = (ranks[id], file.doc_id, pos);
            position_offsets[id] += 1;
        }
    }
    InvertedTokens {
        postings,
        positions,
        symbols: order
            .into_iter()
            .map(|id| std::mem::take(&mut symbols[id]))
            .collect(),
    }
}

/// Data needed to write a segment to disk (sent to background thread)
struct SegmentWriteJob {
    segment_id: SegmentId,
    segment_path: PathBuf,
    files: Vec<AssignedFile>,
    trigram_frequencies: Arc<Mutex<ahash::AHashMap<Trigram, u32>>>,
}

/// Pick stop-grams from (trigram, doc-frequency) pairs: a trigram qualifies
/// only when it appears in more than half of all documents (it can no longer
/// narrow the candidate set), capped at the `cap` hottest ones. The fraction
/// threshold keeps small indexes from declaring every trigram a stop-gram.
pub(crate) fn select_stop_grams(
    freq: Vec<(Trigram, usize)>,
    doc_count: usize,
    cap: usize,
) -> HashSet<Trigram> {
    let min_docs = doc_count / 2 + 1;
    let mut hot: Vec<_> = freq.into_iter().filter(|&(_, c)| c >= min_docs).collect();
    hot.sort_by_key(|&(_, freq)| std::cmp::Reverse(freq));
    hot.into_iter().take(cap).map(|(t, _)| t).collect()
}

/// Chunked index writer for memory-bounded index building.
/// Processes files in chunks and writes each chunk as a separate segment.
/// Segment writes happen asynchronously in a background thread to overlap
/// I/O with processing of the next chunk.
pub struct ChunkedIndexWriter {
    generation: crate::index::generation::Generation,
    root_path: PathBuf,
    index_path: PathBuf,
    config: IndexConfig,
    // Global state (persists across chunks)
    all_documents: Vec<Document>,
    all_paths: Vec<PathBuf>,
    path_to_id: HashMap<PathBuf, PathId>,
    next_doc_id: DocId,
    segment_ids: Vec<SegmentId>,
    /// Files rejected after reading content (binary sniff etc.), recorded in
    /// meta so incremental scans skip them while unchanged
    rejected_files: Vec<(PathBuf, u64)>,
    // Accumulated trigram frequencies for stop-gram computation (shared with
    // background thread). ahash: 3-byte trigram keys don't need SipHash.
    trigram_frequencies: Arc<Mutex<ahash::AHashMap<Trigram, u32>>>,
    // Background writer thread
    write_sender: Option<SyncSender<SegmentWriteJob>>,
    write_thread: Option<JoinHandle<Vec<anyhow::Error>>>,
    // Channel for receiving segment completion notifications
    completion_receiver: Option<Receiver<SegmentId>>,
}

impl Drop for ChunkedIndexWriter {
    fn drop(&mut self) {
        // Finish queued writes before the unpublished generation is removed.
        self.write_sender.take();
        if let Some(thread) = self.write_thread.take() {
            let _ = thread.join();
        }
    }
}

impl ChunkedIndexWriter {
    /// Create a new chunked index writer
    pub fn new(root_path: &Path, config: IndexConfig) -> Result<Self> {
        let root_path = root_path.canonicalize()?;
        let generation = crate::index::generation::Generation::new(&root_path)?;
        let index_path = generation.path.clone();

        // Create index directory structure
        fs::create_dir_all(&index_path)?;
        let segments_path = index_path.join("segments");
        fs::create_dir_all(&segments_path)?;

        // Hand off one segment directly to the writer, then tokenize the
        // next concurrently. Queuing extra complete segments multiplies peak
        // memory without increasing steady-state pipeline throughput.
        let (tx, rx) = mpsc::sync_channel::<SegmentWriteJob>(0);

        // Create channel for completion notifications
        let (completion_tx, completion_rx) = mpsc::channel::<SegmentId>();

        // Spawn background writer thread
        let write_thread = thread::spawn(move || {
            let mut errors = Vec::new();
            while let Ok(job) = rx.recv() {
                let segment_id = job.segment_id;
                if let Err(e) = Self::process_and_write_segment(job) {
                    errors.push(e);
                } else {
                    // Notify that segment write completed successfully
                    let _ = completion_tx.send(segment_id);
                }
            }
            errors
        });

        Ok(Self {
            generation,
            root_path,
            index_path,
            config,
            all_documents: Vec::new(),
            all_paths: Vec::new(),
            path_to_id: HashMap::new(),
            next_doc_id: 1,
            segment_ids: Vec::new(),
            rejected_files: Vec::new(),
            trigram_frequencies: Arc::new(Mutex::new(ahash::AHashMap::new())),
            write_sender: Some(tx),
            write_thread: Some(write_thread),
            completion_receiver: Some(completion_rx),
        })
    }

    /// Get or create path ID
    fn add_path(&mut self, path: &Path) -> PathId {
        if let Some(&id) = self.path_to_id.get(path) {
            return id;
        }

        let id = self.all_paths.len() as PathId;
        self.all_paths.push(path.to_path_buf());
        self.path_to_id.insert(path.to_path_buf(), id);
        id
    }

    /// Get total number of segments queued for writing
    pub fn total_segments(&self) -> usize {
        self.segment_ids.len()
    }

    /// Try to receive a segment completion notification (non-blocking).
    /// Returns Some(segment_id) if a segment finished writing, None otherwise.
    pub fn try_recv_completion(&self) -> Option<SegmentId> {
        self.completion_receiver
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
    }

    /// Wait for the next segment completion with timeout.
    /// Returns Some(segment_id) if a segment finished, None on timeout.
    pub fn recv_completion_timeout(&self, timeout: Duration) -> Option<SegmentId> {
        self.completion_receiver
            .as_ref()
            .and_then(|rx| rx.recv_timeout(timeout).ok())
    }

    /// Write a chunk of processed files as a segment.
    /// This only assigns IDs and builds Document entries synchronously,
    /// then dispatches all heavy work to a background thread.
    #[allow(dead_code)] // Public Vec<String> ingestion API; bulk build uses packed tokens.
    pub fn write_chunk(
        &mut self,
        segment_id: SegmentId,
        processed_files: Vec<ProcessedFile>,
    ) -> Result<()> {
        self.write_chunk_impl(segment_id, processed_files)
    }

    pub(crate) fn write_packed_chunk(
        &mut self,
        segment_id: SegmentId,
        processed_files: Vec<ProcessedFile<crate::utils::PackedTokens>>,
    ) -> Result<()> {
        self.write_chunk_impl(segment_id, processed_files)
    }

    fn write_chunk_impl<T: Into<crate::utils::PackedTokens>>(
        &mut self,
        segment_id: SegmentId,
        processed_files: Vec<ProcessedFile<T>>,
    ) -> Result<()> {
        if processed_files.is_empty() {
            return Ok(());
        }

        self.segment_ids.push(segment_id);

        // Assign IDs and build documents synchronously (fast - just ID assignment)
        let mut assigned_files = Vec::with_capacity(processed_files.len());

        for processed in processed_files {
            let doc_id = self.next_doc_id;
            self.next_doc_id += 1;

            let path_id = self.add_path(&processed.rel_path);

            // Create document entry
            let doc = Document {
                doc_id,
                path_id,
                size: processed.size,
                mtime: processed.mtime,
                language: processed.language,
                flags: processed.flags,
                segment_id,
            };
            self.all_documents.push(doc);

            // Store file data with assigned ID for background processing
            assigned_files.push(AssignedFile {
                doc_id,
                trigrams: processed.trigrams,
                tokens: processed.tokens.into(),
                line_offsets: processed.line_offsets,
                token_positions: processed.token_positions,
            });
        }

        // Dispatch all heavy work to background thread
        if let Some(ref sender) = self.write_sender {
            let segment_name = format!("seg_{:04}", segment_id);
            let segment_path = self.index_path.join("segments").join(&segment_name);

            let job = SegmentWriteJob {
                segment_id,
                segment_path,
                files: assigned_files,
                trigram_frequencies: Arc::clone(&self.trigram_frequencies),
            };
            let _ = sender.send(job);
        }

        Ok(())
    }

    /// Process files and write segment to disk (called from background thread)
    fn process_and_write_segment(mut job: SegmentWriteJob) -> Result<()> {
        // Create segment directory
        fs::create_dir_all(&job.segment_path)?;

        let file_count = job.files.len();
        let t_start = std::time::Instant::now();

        let InvertedTokens {
            postings: token_pairs,
            positions: position_triples,
            symbols: symbols_sorted,
        } = invert_token_postings(&mut job.files);
        let trigram_count = job.files.iter().map(|f| f.trigrams.len()).sum();
        let mut trigram_pairs = Vec::with_capacity(trigram_count);
        let mut line_maps = Vec::with_capacity(file_count);
        for file in job.files {
            trigram_pairs.extend(file.trigrams.into_iter().map(|gram| (gram, file.doc_id)));
            line_maps.push((file.doc_id, file.line_offsets));
        }

        let t_collect = std::time::Instant::now();

        // Sort flat pairs — par_sort is very cache-friendly on contiguous data
        trigram_pairs.par_sort_unstable();

        let t_sort = std::time::Instant::now();

        // Build bloom filter from sorted unique trigrams only
        let estimated_trigrams = file_count * 500;
        let mut bloom_filter = BloomFilter::new(estimated_trigrams.max(10000), 0.01);
        {
            let mut prev: Option<u32> = None;
            for &(trigram, _) in &trigram_pairs {
                if prev != Some(trigram) {
                    bloom_filter.insert(trigram);
                    prev = Some(trigram);
                }
            }
        }

        let t_bloom = std::time::Instant::now();

        // Derive frequencies from sorted pairs and batch update shared map
        {
            let mut freq_map = job
                .trigram_frequencies
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !trigram_pairs.is_empty() {
                let mut current = trigram_pairs[0].0;
                let mut count: u32 = 1;
                for &(trigram, _) in &trigram_pairs[1..] {
                    if trigram == current {
                        count += 1;
                    } else {
                        *freq_map.entry(current).or_insert(0) += count;
                        current = trigram;
                        count = 1;
                    }
                }
                *freq_map.entry(current).or_insert(0) += count;
            }
        }

        let t_freq = std::time::Instant::now();

        // Write all segment files concurrently (5 threads)
        thread::scope(|s| {
            let trigram_handle =
                s.spawn(|| Self::write_trigram_index_flat(&job.segment_path, &trigram_pairs));
            let token_handle = s.spawn(|| {
                Self::write_token_index_flat(
                    &job.segment_path,
                    &token_pairs,
                    &position_triples,
                    &symbols_sorted,
                )
            });
            let linemap_handle =
                s.spawn(|| Self::write_line_maps_flat(&job.segment_path, &line_maps));
            let bloom_handle =
                s.spawn(|| Self::write_bloom_filter(&job.segment_path, &bloom_filter));

            trigram_handle.join().unwrap()?;
            token_handle.join().unwrap()?;
            linemap_handle.join().unwrap()?;
            bloom_handle.join().unwrap()?;
            Ok::<(), anyhow::Error>(())
        })?;

        let t_write = std::time::Instant::now();

        if std::env::var("FXI_DEBUG").is_ok() {
            eprintln!(
                "[seg{}] files={} pairs={} | collect={:?} sort={:?} bloom={:?} freq={:?} write={:?} TOTAL={:?}",
                job.segment_id,
                file_count,
                trigram_pairs.len(),
                t_collect - t_start,
                t_sort - t_collect,
                t_bloom - t_sort,
                t_freq - t_bloom,
                t_write - t_freq,
                t_write - t_start,
            );
        }

        Ok(())
    }

    /// Write trigram index from pre-sorted flat pairs
    fn write_trigram_index_flat(segment_path: &Path, pairs: &[(u32, u32)]) -> Result<()> {
        let dict_path = segment_path.join("grams.dict");
        let postings_path = segment_path.join("grams.postings");

        if pairs.is_empty() {
            let mut dict_file = BufWriter::new(File::create(&dict_path)?);
            let _ = File::create(&postings_path)?;
            dict_file.write_all(&0u32.to_le_bytes())?;
            dict_file.flush()?;
            return Ok(());
        }

        // Count unique trigrams and find group boundaries
        let mut group_starts: Vec<usize> = Vec::with_capacity(pairs.len() / 10);
        group_starts.push(0);
        for i in 1..pairs.len() {
            if pairs[i].0 != pairs[i - 1].0 {
                group_starts.push(i);
            }
        }
        let entry_count = group_starts.len();

        // Parallel encode each group
        let encoded: Vec<(u32, Vec<u8>, u32)> = group_starts
            .par_iter()
            .enumerate()
            .map(|(idx, &start)| {
                let end = group_starts.get(idx + 1).copied().unwrap_or(pairs.len());
                let trigram = pairs[start].0;

                // Collect unique doc_ids (already sorted by doc_id within group)
                let mut doc_ids: Vec<u32> = Vec::with_capacity(end - start);
                for &(_, doc_id) in &pairs[start..end] {
                    if doc_ids.last() != Some(&doc_id) {
                        doc_ids.push(doc_id);
                    }
                }

                let doc_freq = doc_ids.len() as u32;
                let mut enc = Vec::with_capacity(doc_ids.len() * 2);
                delta_encode(&doc_ids, &mut enc);

                (trigram, enc, doc_freq)
            })
            .collect();

        let mut dict_file = BufWriter::with_capacity(65536, File::create(&dict_path)?);
        let mut postings_file = BufWriter::with_capacity(65536, File::create(&postings_path)?);
        dict_file.write_all(&(entry_count as u32).to_le_bytes())?;
        let mut offset = 0u64;
        for (trigram, enc, doc_freq) in encoded {
            dict_file.write_all(&trigram.to_le_bytes())?;
            dict_file.write_all(&offset.to_le_bytes())?;
            dict_file.write_all(&(enc.len() as u32).to_le_bytes())?;
            dict_file.write_all(&doc_freq.to_le_bytes())?;
            postings_file.write_all(&enc)?;
            offset += enc.len() as u64;
        }
        dict_file.flush()?;
        postings_file.flush()?;

        Ok(())
    }

    /// Write token index from pre-sorted flat pairs, and token positions file.
    /// The token dict is extended with pos_offset and pos_length fields per entry.
    ///
    /// `pairs` and `position_triples` are keyed by lexicographic token rank
    /// (see process_and_write_segment); `symbols_sorted[rank]` is the token
    /// string. Both lists are sorted, so group order equals dict order.
    fn write_token_index_flat(
        segment_path: &Path,
        pairs: &[(u32, DocId)],
        position_triples: &[(u32, DocId, u32)],
        symbols_sorted: &[String],
    ) -> Result<()> {
        let dict_path = segment_path.join("tokens.dict");
        let postings_path = segment_path.join("tokens.postings");
        let positions_path = segment_path.join("tokens.positions");

        if pairs.is_empty() {
            let mut dict_file = BufWriter::new(File::create(&dict_path)?);
            let _ = File::create(&postings_path)?;
            let _ = File::create(&positions_path)?;
            dict_file.write_all(&0u32.to_le_bytes())?;
            dict_file.flush()?;
            return Ok(());
        }

        // Find group boundaries for token pairs
        let mut group_starts: Vec<usize> = Vec::with_capacity(pairs.len() / 10);
        group_starts.push(0);
        for i in 1..pairs.len() {
            if pairs[i].0 != pairs[i - 1].0 {
                group_starts.push(i);
            }
        }
        let entry_count = group_starts.len();

        // Parallel encode postings
        let encoded: Vec<(u32, Vec<u8>, u32)> = group_starts
            .par_iter()
            .enumerate()
            .map(|(idx, &start)| {
                let end = group_starts.get(idx + 1).copied().unwrap_or(pairs.len());
                let token_rank = pairs[start].0;

                let mut doc_ids: Vec<u32> = Vec::with_capacity(end - start);
                for (_, doc_id) in &pairs[start..end] {
                    if doc_ids.last() != Some(doc_id) {
                        doc_ids.push(*doc_id);
                    }
                }

                let doc_freq = doc_ids.len() as u32;
                let mut enc = Vec::with_capacity(doc_ids.len() * 2);
                delta_encode(&doc_ids, &mut enc);

                (token_rank, enc, doc_freq)
            })
            .collect();

        // Stream each encoded list once. Retaining a second segment-sized copy
        // of postings and positions needlessly doubles their peak memory.
        let mut dict_file = BufWriter::with_capacity(65536, File::create(&dict_path)?);
        let mut postings_file = BufWriter::with_capacity(65536, File::create(&postings_path)?);
        let mut positions_file = BufWriter::with_capacity(65536, File::create(&positions_path)?);
        dict_file.write_all(&(entry_count as u32).to_le_bytes())?;
        let mut offset = 0u64;
        let mut pos_offset = 0u64;
        let mut pos_idx = 0usize;
        let mut pos_buf = Vec::new();
        for (token_rank, enc, doc_freq) in encoded {
            let token_bytes = symbols_sorted[token_rank as usize].as_bytes();
            dict_file.write_all(&(token_bytes.len() as u16).to_le_bytes())?;
            dict_file.write_all(token_bytes)?;
            dict_file.write_all(&offset.to_le_bytes())?;
            dict_file.write_all(&(enc.len() as u32).to_le_bytes())?;
            dict_file.write_all(&doc_freq.to_le_bytes())?;

            while pos_idx < position_triples.len() && position_triples[pos_idx].0 < token_rank {
                pos_idx += 1;
            }
            pos_buf.clear();
            let mut prev_doc = 0;
            while pos_idx < position_triples.len() && position_triples[pos_idx].0 == token_rank {
                let doc = position_triples[pos_idx].1;
                let start = pos_idx;
                while pos_idx < position_triples.len()
                    && position_triples[pos_idx].0 == token_rank
                    && position_triples[pos_idx].1 == doc
                {
                    pos_idx += 1;
                }
                crate::utils::encode_varint(doc - prev_doc, &mut pos_buf);
                crate::utils::encode_varint((pos_idx - start) as u32, &mut pos_buf);
                let mut prev_pos = 0;
                for &(_, _, pos) in &position_triples[start..pos_idx] {
                    crate::utils::encode_varint(pos - prev_pos, &mut pos_buf);
                    prev_pos = pos;
                }
                prev_doc = doc;
            }
            let entry_pos_offset = if pos_buf.is_empty() { 0 } else { pos_offset };
            dict_file.write_all(&entry_pos_offset.to_le_bytes())?;
            dict_file.write_all(&(pos_buf.len() as u32).to_le_bytes())?;
            positions_file.write_all(&pos_buf)?;
            pos_offset += pos_buf.len() as u64;
            postings_file.write_all(&enc)?;
            offset += enc.len() as u64;
        }
        dict_file.flush()?;
        postings_file.flush()?;
        positions_file.flush()?;

        Ok(())
    }

    /// Write line maps from flat vec
    fn write_line_maps_flat(segment_path: &Path, line_maps: &[(DocId, Vec<u32>)]) -> Result<()> {
        let path = segment_path.join("linemap.bin");

        // Pre-allocate buffer
        let estimated_size = 4 + line_maps.len() * 20;
        let mut buf = Vec::with_capacity(estimated_size);
        let mut encoded = Vec::with_capacity(1024);

        buf.extend_from_slice(&(line_maps.len() as u32).to_le_bytes());

        for (doc_id, offsets) in line_maps {
            buf.extend_from_slice(&doc_id.to_le_bytes());
            buf.extend_from_slice(&(offsets.len() as u32).to_le_bytes());

            encoded.clear();
            delta_encode(offsets, &mut encoded);
            buf.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
            buf.extend_from_slice(&encoded);
        }

        let mut file = BufWriter::new(File::create(&path)?);
        file.write_all(&buf)?;

        Ok(())
    }

    /// Write bloom filter to segment for fast pre-filtering
    fn write_bloom_filter(segment_path: &Path, bloom_filter: &BloomFilter) -> Result<()> {
        let bloom_path = segment_path.join("bloom.bin");
        let mut file = BufWriter::with_capacity(65536, File::create(&bloom_path)?);

        // Write num_hashes (u8)
        file.write_all(&[bloom_filter.num_hashes()])?;

        // Write number of u64 words
        let bits = bloom_filter.bits();
        file.write_all(&(bits.len() as u32).to_le_bytes())?;

        // Write bit data in one buffer instead of one write per u64 word
        let mut bit_buf = Vec::with_capacity(bits.len() * 8);
        for &word in bits {
            bit_buf.extend_from_slice(&word.to_le_bytes());
        }
        file.write_all(&bit_buf)?;

        file.flush()?;
        Ok(())
    }

    /// Compute stop-grams from accumulated frequencies
    fn compute_stop_grams(&self) -> HashSet<Trigram> {
        let freq_map = self
            .trigram_frequencies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let freq: Vec<_> = freq_map
            .iter()
            .map(|(&t, &count)| (t, count as usize))
            .collect();

        select_stop_grams(freq, self.all_documents.len(), self.config.stop_gram_count)
    }

    /// Finalize the index - wait for pending writes, then write global data (docs, paths, meta)
    #[allow(dead_code)]
    pub fn finalize(&mut self) -> Result<()> {
        self.finalize_with_progress(|_, _| {})
    }

    /// Finalize with progress callback.
    /// The callback receives (completed_count, total_count) for each segment that finishes writing.
    pub fn finalize_with_progress<F>(&mut self, mut on_segment_complete: F) -> Result<()>
    where
        F: FnMut(usize, usize),
    {
        let total_segments = self.segment_ids.len();
        let mut completed = 0;

        // Drop the sender to signal the background thread to finish
        self.write_sender.take();

        // Poll for completions while waiting for thread to finish
        // This allows progress reporting as each segment completes
        if let Some(handle) = self.write_thread.take() {
            // Drain completion notifications while thread is running
            while !handle.is_finished() {
                // Check for completions with a short timeout
                if let Some(_segment_id) = self.recv_completion_timeout(Duration::from_millis(50)) {
                    completed += 1;
                    on_segment_complete(completed, total_segments);
                }
            }

            // Drain any remaining completions after thread finishes
            while let Some(_segment_id) = self.try_recv_completion() {
                completed += 1;
                on_segment_complete(completed, total_segments);
            }

            // Now join and check for errors
            let errors = handle
                .join()
                .map_err(|_| anyhow::anyhow!("Background write thread panicked"))?;
            if !errors.is_empty() {
                // Return the first error (could aggregate if needed)
                return Err(errors.into_iter().next().unwrap());
            }
        }

        // Write documents table
        self.write_documents()?;

        // Write path store
        self.write_paths()?;

        // Compute stop-grams from accumulated frequencies
        let stop_grams = self.compute_stop_grams();

        // Write metadata
        self.write_meta(&stop_grams)?;
        self.generation.publish()?;

        Ok(())
    }

    /// Write document table
    fn write_documents(&self) -> Result<()> {
        let docs_path = self.index_path.join("docs.bin");
        let mut file = BufWriter::with_capacity(65536, File::create(&docs_path)?);

        // Write document count
        file.write_all(&(self.all_documents.len() as u32).to_le_bytes())?;

        for doc in &self.all_documents {
            // One write per record instead of seven
            let mut rec = [0u8; 30];
            rec[0..4].copy_from_slice(&doc.doc_id.to_le_bytes());
            rec[4..8].copy_from_slice(&doc.path_id.to_le_bytes());
            rec[8..16].copy_from_slice(&doc.size.to_le_bytes());
            rec[16..24].copy_from_slice(&doc.mtime.to_le_bytes());
            rec[24..26].copy_from_slice(&(doc.language as u16).to_le_bytes());
            rec[26..28].copy_from_slice(&doc.flags.0.to_le_bytes());
            rec[28..30].copy_from_slice(&doc.segment_id.to_le_bytes());
            file.write_all(&rec)?;
        }

        file.flush()?;
        Ok(())
    }

    /// Write path store
    fn write_paths(&self) -> Result<()> {
        let paths_path = self.index_path.join("paths.bin");
        let mut file = BufWriter::with_capacity(65536, File::create(&paths_path)?);

        // Simple format: count, then [length, bytes]...
        file.write_all(&(self.all_paths.len() as u32).to_le_bytes())?;

        for path in &self.all_paths {
            let path_str = path.to_string_lossy();
            let bytes = path_str.as_bytes();
            file.write_all(&(bytes.len() as u32).to_le_bytes())?;
            file.write_all(bytes)?;
        }

        file.flush()?;
        Ok(())
    }

    /// Write metadata
    fn write_meta(&self, stop_grams: &HashSet<Trigram>) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Use first segment as base, rest as delta
        let (base_segment, delta_segments) = if self.segment_ids.is_empty() {
            (None, Vec::new())
        } else if self.segment_ids.len() == 1 {
            (Some(self.segment_ids[0]), Vec::new())
        } else {
            (Some(self.segment_ids[0]), self.segment_ids[1..].to_vec())
        };

        // Compute valid doc count (no tombstones in fresh index)
        let valid_doc_count = self.all_documents.len() as u32;

        // Set delta_baseline to current delta count so chunked indexes don't immediately trigger merge
        let delta_baseline = delta_segments.len();

        let meta = IndexMeta {
            version: 2,
            root_path: self.root_path.clone(),
            doc_count: self.all_documents.len() as u32,
            segment_count: self.segment_ids.len() as u16,
            base_segment,
            delta_segments,
            stop_grams: stop_grams.iter().copied().collect(),
            created_at: now,
            updated_at: now,
            tombstone_count: 0, // Fresh index has no tombstones
            valid_doc_count,
            delta_baseline,
            has_positions: true,
            rejected_files: self.rejected_files.clone(),
        };

        let meta_path = self.index_path.join("meta.json");
        let file = File::create(&meta_path)?;
        serde_json::to_writer_pretty(file, &meta)?;

        Ok(())
    }

    /// Record files rejected during processing so the incremental change
    /// scan can skip them while their mtime is unchanged
    pub fn set_rejected_files(&mut self, rejected: Vec<(PathBuf, u64)>) {
        self.rejected_files = rejected;
    }

    /// Get the index path
    #[allow(dead_code)]
    pub fn index_path(&self) -> &Path {
        &self.index_path
    }
}

// =============================================================================
// Delta Segment Writer - For incremental index updates
// =============================================================================

/// Delta segment writer for incremental index updates.
/// Loads existing index data and writes a new delta segment containing only changed files.
pub struct DeltaSegmentWriter {
    generation: crate::index::generation::Generation,
    #[allow(dead_code)]
    root_path: PathBuf,
    index_path: PathBuf,
    segment_id: SegmentId,

    // Loaded from existing index
    existing_documents: Vec<Document>,
    existing_paths: Vec<PathBuf>,
    path_to_id: HashMap<PathBuf, PathId>,

    // New data for this delta
    new_documents: Vec<Document>,
    new_paths: Vec<PathBuf>,
    next_doc_id: DocId,
    next_path_id: PathId,
    trigram_postings: BTreeMap<Trigram, Vec<DocId>>,
    token_postings: BTreeMap<String, Vec<DocId>>,
    /// Token -> doc_id -> positions (for positional phrase queries)
    token_position_postings: BTreeMap<String, BTreeMap<DocId, Vec<u32>>>,
    line_maps: HashMap<DocId, Vec<u32>>,

    // Docs to mark as tombstones
    tombstone_doc_ids: Vec<DocId>,
}

impl DeltaSegmentWriter {
    /// Create a new delta segment writer.
    /// Loads existing documents and paths from the index.
    pub fn new(root_path: &Path, segment_id: SegmentId) -> Result<Self> {
        let root_path = root_path.canonicalize()?;
        let index_path = get_index_dir(&root_path)?;

        // Load existing documents and paths
        let existing_documents = crate::index::reader::read_documents(&index_path)?;
        let existing_paths = crate::index::reader::read_paths(&index_path)?;

        let generation = crate::index::generation::Generation::new(&root_path)?;
        generation.inherit_segments(&index_path)?;
        let index_path = generation.path.clone();

        // Build path lookup map
        let mut path_to_id: HashMap<PathBuf, PathId> = HashMap::new();
        for (idx, path) in existing_paths.iter().enumerate() {
            path_to_id.insert(path.clone(), idx as PathId);
        }

        // Calculate next IDs
        let next_doc_id = existing_documents
            .iter()
            .map(|d| d.doc_id)
            .max()
            .unwrap_or(0)
            + 1;
        let next_path_id = existing_paths.len() as PathId;

        Ok(Self {
            generation,
            root_path,
            index_path,
            segment_id,
            existing_documents,
            existing_paths,
            path_to_id,
            new_documents: Vec::new(),
            new_paths: Vec::new(),
            next_doc_id,
            next_path_id,
            trigram_postings: BTreeMap::new(),
            token_postings: BTreeMap::new(),
            token_position_postings: BTreeMap::new(),
            line_maps: HashMap::new(),
            tombstone_doc_ids: Vec::new(),
        })
    }

    /// Mark a document as a tombstone by its relative path.
    /// The document will be marked as deleted in docs.bin but its segment data remains.
    pub fn mark_tombstone(&mut self, rel_path: &Path) {
        // Find the path_id for this path
        if let Some(&path_id) = self.path_to_id.get(rel_path) {
            // Find the doc_id for this path_id (most recent non-tombstone)
            for doc in self.existing_documents.iter().rev() {
                if doc.path_id == path_id && doc.is_valid() {
                    self.tombstone_doc_ids.push(doc.doc_id);
                    break;
                }
            }
        }
    }

    /// Get or create a path ID for the given relative path
    fn get_or_create_path_id(&mut self, rel_path: &Path) -> PathId {
        if let Some(&path_id) = self.path_to_id.get(rel_path) {
            return path_id;
        }

        // New path - assign next ID
        let path_id = self.next_path_id;
        self.next_path_id += 1;
        self.new_paths.push(rel_path.to_path_buf());
        self.path_to_id.insert(rel_path.to_path_buf(), path_id);
        path_id
    }

    /// Add a processed file to the delta segment
    pub fn add_file(&mut self, processed: ProcessedFile) {
        let doc_id = self.next_doc_id;
        self.next_doc_id += 1;

        let path_id = self.get_or_create_path_id(&processed.rel_path);

        // Create document entry
        let doc = Document {
            doc_id,
            path_id,
            size: processed.size,
            mtime: processed.mtime,
            language: processed.language,
            flags: processed.flags,
            segment_id: self.segment_id,
        };
        self.new_documents.push(doc);

        // Add trigrams to postings
        for trigram in processed.trigrams {
            self.trigram_postings
                .entry(trigram)
                .or_default()
                .push(doc_id);
        }

        // Add token positions first (they index into `tokens`, which is
        // moved into the postings map below)
        for (idx, pos) in processed.token_positions {
            let token = &processed.tokens[idx as usize];
            if let Some(doc_map) = self.token_position_postings.get_mut(token) {
                doc_map.entry(doc_id).or_default().push(pos);
            } else {
                let mut doc_map = std::collections::BTreeMap::new();
                doc_map.insert(doc_id, vec![pos]);
                self.token_position_postings.insert(token.clone(), doc_map);
            }
        }

        // Add tokens to postings
        for token in processed.tokens {
            self.token_postings.entry(token).or_default().push(doc_id);
        }

        // Store line map
        self.line_maps.insert(doc_id, processed.line_offsets);
    }

    /// Check if there are any changes to write
    pub fn has_changes(&self) -> bool {
        !self.new_documents.is_empty() || !self.tombstone_doc_ids.is_empty()
    }

    /// Finalize the delta segment - write all data atomically.
    /// Returns the updated IndexMeta.
    pub fn finalize(mut self, meta: &mut IndexMeta) -> Result<()> {
        // Create segment directory if we have new documents
        let has_new_documents = !self.new_documents.is_empty();
        if has_new_documents {
            let segment_path = self
                .index_path
                .join("segments")
                .join(format!("seg_{:04}", self.segment_id));
            fs::create_dir_all(segment_path.parent().unwrap())?;
            fs::create_dir(&segment_path)?;

            // Write segment files
            self.write_segment_files(&segment_path)?;
        }

        // Merge existing and new documents, applying tombstones
        let tombstone_set: HashSet<DocId> = self.tombstone_doc_ids.iter().copied().collect();
        let mut all_documents: Vec<Document> = self
            .existing_documents
            .into_iter()
            .map(|mut doc| {
                if tombstone_set.contains(&doc.doc_id) {
                    doc.flags.set_tombstone();
                }
                doc
            })
            .collect();
        all_documents.extend(self.new_documents);

        // Merge paths
        let mut all_paths = self.existing_paths;
        all_paths.extend(self.new_paths);

        // Write atomically: segment → docs.bin → paths.bin → meta.json
        // (Segment already written above)

        // Update docs.bin atomically
        write_documents_atomic(&self.index_path, &all_documents)?;

        // Update paths.bin atomically
        write_paths_atomic(&self.index_path, &all_paths)?;

        // Update meta
        meta.version = 2;
        meta.doc_count = all_documents.len() as u32;
        if has_new_documents {
            meta.delta_segments.push(self.segment_id);
        }
        meta.segment_count =
            u16::from(meta.base_segment.is_some()) + meta.delta_segments.len() as u16;
        meta.updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Track fragmentation metrics
        meta.tombstone_count = all_documents
            .iter()
            .filter(|d| d.flags.is_tombstone())
            .count() as u32;
        meta.valid_doc_count = all_documents.iter().filter(|d| d.is_valid()).count() as u32;

        // Write meta.json atomically (commits the transaction)
        write_meta_atomic(&self.index_path, meta)?;
        self.generation.publish()?;

        Ok(())
    }

    /// Write segment files (trigrams, tokens, line maps, bloom filter)
    fn write_segment_files(&self, segment_path: &Path) -> Result<()> {
        use crate::index::segment_io;

        // Delta segments keep all trigrams: stop-grams are a global
        // (meta-level) judgement made at full build or compaction
        segment_io::write_trigram_index(segment_path, &self.trigram_postings, None)?;
        segment_io::write_token_index(
            segment_path,
            &self.token_postings,
            Some(&self.token_position_postings),
        )?;
        segment_io::write_line_maps(segment_path, &self.line_maps)?;
        segment_io::build_and_write_bloom(
            segment_path,
            self.trigram_postings.keys().copied(),
            1000,
        )?;

        Ok(())
    }
}

// =============================================================================
// Atomic Write Helpers
// =============================================================================

/// Write documents to docs.bin atomically using temp file + rename
pub fn write_documents_atomic(index_path: &Path, documents: &[Document]) -> Result<()> {
    let docs_path = index_path.join("docs.bin");
    let tmp_path = index_path.join("docs.bin.tmp");

    {
        let mut file = BufWriter::with_capacity(65536, File::create(&tmp_path)?);

        // Write document count
        file.write_all(&(documents.len() as u32).to_le_bytes())?;

        for doc in documents {
            file.write_all(&doc.doc_id.to_le_bytes())?;
            file.write_all(&doc.path_id.to_le_bytes())?;
            file.write_all(&doc.size.to_le_bytes())?;
            file.write_all(&doc.mtime.to_le_bytes())?;
            file.write_all(&(doc.language as u16).to_le_bytes())?;
            file.write_all(&doc.flags.0.to_le_bytes())?;
            file.write_all(&doc.segment_id.to_le_bytes())?;
        }

        file.flush()?;
    }

    // Atomic rename
    fs::rename(&tmp_path, &docs_path)?;
    Ok(())
}

/// Write paths to paths.bin atomically using temp file + rename
pub fn write_paths_atomic(index_path: &Path, paths: &[PathBuf]) -> Result<()> {
    let paths_path = index_path.join("paths.bin");
    let tmp_path = index_path.join("paths.bin.tmp");

    {
        let mut file = BufWriter::with_capacity(65536, File::create(&tmp_path)?);

        // Write count
        file.write_all(&(paths.len() as u32).to_le_bytes())?;

        for path in paths {
            let path_str = path.to_string_lossy();
            let bytes = path_str.as_bytes();
            file.write_all(&(bytes.len() as u32).to_le_bytes())?;
            file.write_all(bytes)?;
        }

        file.flush()?;
    }

    // Atomic rename
    fs::rename(&tmp_path, &paths_path)?;
    Ok(())
}

/// Write meta.json atomically using temp file + rename
pub fn write_meta_atomic(index_path: &Path, meta: &IndexMeta) -> Result<()> {
    let meta_path = index_path.join("meta.json");
    let tmp_path = index_path.join("meta.json.tmp");

    {
        let file = File::create(&tmp_path)?;
        serde_json::to_writer_pretty(file, meta)?;
    }

    // Atomic rename - this commits the transaction
    fs::rename(&tmp_path, &meta_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn stable_token_inversion_matches_full_sort() {
        for file_count in [0, 1, 7, 35] {
            let mut files = Vec::new();
            for doc in 0..file_count {
                let tokens: Vec<String> = (0..(doc % 11 + 1))
                    .map(|i| format!("symbol{:03}", (i * 7 + doc) % 19))
                    .collect();
                // Deliberately reversed positions, repeated occurrences, shared
                // tokens, and document IDs spanning varint boundaries.
                let token_positions = (0..doc * 31)
                    .rev()
                    .map(|i| ((i as usize % tokens.len()) as u32, i / 2))
                    .collect();
                files.push(AssignedFile {
                    doc_id: doc * 131,
                    trigrams: vec![],
                    tokens: tokens.into(),
                    line_offsets: vec![],
                    token_positions,
                });
            }
            let mut symbols: Vec<String> = files
                .iter()
                .flat_map(|f| f.tokens.iter().map(str::to_owned))
                .collect();
            symbols.sort();
            symbols.dedup();
            let mut expected_pairs = Vec::new();
            let mut expected_positions = Vec::new();
            for file in &files {
                for token in file.tokens.iter() {
                    expected_pairs.push((
                        symbols.binary_search_by(|s| s.as_str().cmp(token)).unwrap() as u32,
                        file.doc_id,
                    ));
                }
                for &(idx, pos) in &file.token_positions {
                    expected_positions.push((
                        symbols
                            .binary_search_by(|s| s.as_str().cmp(&file.tokens[idx as usize]))
                            .unwrap() as u32,
                        file.doc_id,
                        pos,
                    ));
                }
            }
            expected_pairs.sort_unstable();
            expected_positions.sort_unstable();
            let inverted = invert_token_postings(&mut files);
            assert_eq!(inverted.symbols, symbols);
            assert_eq!(inverted.postings, expected_pairs);
            assert_eq!(inverted.positions, expected_positions);
        }
    }

    #[test]
    fn streamed_indexes_match_reference_encoding() {
        // Exercise empty lists, duplicate documents, absent positions, and
        // multi-byte deltas against the existing public encoders.
        for count in [0, 1, 17, 130] {
            let temp = TempDir::new().unwrap();
            let symbols: Vec<String> = (0..count).map(|i| format!("token{i:04}")).collect();
            let mut pairs = Vec::new();
            let mut triples = Vec::new();
            let mut grams_dict = (count as u32).to_le_bytes().to_vec();
            let mut tokens_dict = grams_dict.clone();
            let mut postings = Vec::new();
            let mut positions = Vec::new();
            for rank in 0..count as u32 {
                let docs = [0, 127, 128, 65536 + rank];
                let offset = postings.len() as u64;
                crate::utils::delta_encode(&docs, &mut postings);
                let length = postings.len() as u32 - offset as u32;
                for doc in docs {
                    pairs.extend([(rank, doc), (rank, doc)]);
                }
                grams_dict.extend_from_slice(&rank.to_le_bytes());
                grams_dict.extend_from_slice(&offset.to_le_bytes());
                grams_dict.extend_from_slice(&length.to_le_bytes());
                grams_dict.extend_from_slice(&4u32.to_le_bytes());
                let token = symbols[rank as usize].as_bytes();
                tokens_dict.extend_from_slice(&(token.len() as u16).to_le_bytes());
                tokens_dict.extend_from_slice(token);
                tokens_dict.extend_from_slice(&offset.to_le_bytes());
                tokens_dict.extend_from_slice(&length.to_le_bytes());
                tokens_dict.extend_from_slice(&4u32.to_le_bytes());
                let pos_offset = positions.len() as u64;
                if rank % 3 != 0 {
                    let occurrences = [0, 0, 128, 999999];
                    for doc in docs {
                        for pos in occurrences {
                            triples.push((rank, doc, pos));
                        }
                    }
                    let refs: Vec<_> = docs
                        .iter()
                        .map(|&doc| (doc, occurrences.as_slice()))
                        .collect();
                    crate::utils::encode_position_postings(&refs, &mut positions);
                    tokens_dict.extend_from_slice(&pos_offset.to_le_bytes());
                    tokens_dict.extend_from_slice(
                        &(positions.len() as u32 - pos_offset as u32).to_le_bytes(),
                    );
                } else {
                    tokens_dict.extend_from_slice(&0u64.to_le_bytes());
                    tokens_dict.extend_from_slice(&0u32.to_le_bytes());
                }
            }
            ChunkedIndexWriter::write_trigram_index_flat(temp.path(), &pairs).unwrap();
            ChunkedIndexWriter::write_token_index_flat(temp.path(), &pairs, &triples, &symbols)
                .unwrap();
            for (name, expected) in [
                ("grams.dict", &grams_dict),
                ("grams.postings", &postings),
                ("tokens.dict", &tokens_dict),
                ("tokens.postings", &postings),
                ("tokens.positions", &positions),
            ] {
                assert_eq!(
                    fs::read(temp.path().join(name)).unwrap(),
                    *expected,
                    "{name}, count={count}"
                );
            }
        }
    }

    fn create_test_processed_file(rel_path: &str, content: &str) -> ProcessedFile {
        let trigrams: Vec<u32> = content
            .as_bytes()
            .windows(3)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], 0]))
            .collect();
        // Positions index into the unique-token list, so both must come
        // from the same extraction; whitespace tokens the tokenizer misses
        // are appended after (position indices stay valid)
        let (mut tokens, token_positions) = crate::utils::extract_tokens_and_positions(content);
        for t in content.split_whitespace().map(|s| s.to_lowercase()) {
            if !tokens.contains(&t) {
                tokens.push(t);
            }
        }
        let line_offsets: Vec<u32> = std::iter::once(0)
            .chain(
                content
                    .bytes()
                    .enumerate()
                    .filter(|(_, b)| *b == b'\n')
                    .map(|(i, _)| (i + 1) as u32),
            )
            .collect();

        ProcessedFile {
            rel_path: PathBuf::from(rel_path),
            mtime: 1234567890,
            size: content.len() as u64,
            language: Language::Rust,
            flags: DocFlags::new(),
            trigrams,
            tokens,
            line_offsets,
            token_positions,
        }
    }

    #[test]
    fn test_chunked_writer_single_chunk() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create a dummy file so the root exists
        fs::write(root.join("test.rs"), "fn main() {}").unwrap();

        let config = IndexConfig::default();
        let mut writer = ChunkedIndexWriter::new(root, config).unwrap();

        let files = vec![
            create_test_processed_file("src/main.rs", "fn main() {\n    println!(\"hello\");\n}"),
            create_test_processed_file(
                "src/lib.rs",
                "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}",
            ),
        ];

        writer.write_chunk(1, files).unwrap();
        writer.finalize().unwrap();

        // Verify index was created
        let index_path = crate::utils::get_index_dir(root).unwrap();
        assert!(index_path.join("meta.json").exists());
        assert!(index_path.join("docs.bin").exists());
        assert!(index_path.join("paths.bin").exists());
        assert!(index_path.join("segments").join("seg_0001").exists());
    }

    #[test]
    fn test_chunked_writer_multiple_chunks() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::write(root.join("test.rs"), "fn main() {}").unwrap();

        let config = IndexConfig::default();
        let mut writer = ChunkedIndexWriter::new(root, config).unwrap();

        // Write first chunk
        let files1 = vec![
            create_test_processed_file("src/main.rs", "fn main() {}"),
            create_test_processed_file("src/lib.rs", "pub fn lib() {}"),
        ];
        writer.write_chunk(1, files1).unwrap();

        // Write second chunk
        let files2 = vec![
            create_test_processed_file("src/utils.rs", "pub fn util() {}"),
            create_test_processed_file("src/config.rs", "pub struct Config {}"),
        ];
        writer.write_chunk(2, files2).unwrap();

        writer.finalize().unwrap();

        // Verify both segments were created
        let index_path = crate::utils::get_index_dir(root).unwrap();
        assert!(index_path.join("segments").join("seg_0001").exists());
        assert!(index_path.join("segments").join("seg_0002").exists());

        // Verify meta.json has correct segment count
        let meta: IndexMeta =
            serde_json::from_reader(File::open(index_path.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.segment_count, 2);
        assert_eq!(meta.doc_count, 4);
    }

    #[test]
    fn test_chunked_writer_empty_chunk() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::write(root.join("test.rs"), "fn main() {}").unwrap();

        let config = IndexConfig::default();
        let mut writer = ChunkedIndexWriter::new(root, config).unwrap();

        // Write empty chunk (should be skipped)
        writer.write_chunk(1, vec![]).unwrap();

        // Write actual chunk
        let files = vec![create_test_processed_file("src/main.rs", "fn main() {}")];
        writer.write_chunk(2, files).unwrap();

        writer.finalize().unwrap();

        // Verify only one segment exists
        let index_path = crate::utils::get_index_dir(root).unwrap();
        assert!(!index_path.join("segments").join("seg_0001").exists());
        assert!(index_path.join("segments").join("seg_0002").exists());
    }

    #[test]
    fn test_trigram_frequencies_accumulated() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::write(root.join("test.rs"), "fn main() {}").unwrap();

        let config = IndexConfig::default();
        let mut writer = ChunkedIndexWriter::new(root, config).unwrap();

        // Create files with overlapping trigrams
        let files1 = vec![create_test_processed_file("a.rs", "hello world")];
        writer.write_chunk(1, files1).unwrap();

        let files2 = vec![create_test_processed_file("b.rs", "hello there")];
        writer.write_chunk(2, files2).unwrap();

        writer.finalize().unwrap();

        // Verify stop-grams were computed (meta.json should have stop_grams)
        let index_path = crate::utils::get_index_dir(root).unwrap();
        let meta: IndexMeta =
            serde_json::from_reader(File::open(index_path.join("meta.json")).unwrap()).unwrap();

        // Should have computed some stop-grams from the accumulated frequencies
        // The actual count depends on config.stop_gram_count and file content
        assert!(meta.stop_grams.len() <= meta.doc_count as usize * 100);
    }

    #[test]
    fn test_async_write_completes_before_finalize() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::write(root.join("test.rs"), "fn main() {}").unwrap();

        let config = IndexConfig::default();
        let mut writer = ChunkedIndexWriter::new(root, config).unwrap();

        // Write multiple chunks rapidly
        for i in 1..=5 {
            let files = vec![create_test_processed_file(
                &format!("src/file{}.rs", i),
                &format!("pub fn func{}() {{}}", i),
            )];
            writer.write_chunk(i as u16, files).unwrap();
        }

        // Finalize should wait for all async writes
        writer.finalize().unwrap();

        // Verify all segments exist and have required files
        let index_path = crate::utils::get_index_dir(root).unwrap();
        for i in 1..=5 {
            let seg_path = index_path.join("segments").join(format!("seg_{:04}", i));
            assert!(
                seg_path.join("grams.dict").exists(),
                "seg_{:04} missing grams.dict",
                i
            );
            assert!(
                seg_path.join("grams.postings").exists(),
                "seg_{:04} missing grams.postings",
                i
            );
            assert!(
                seg_path.join("tokens.dict").exists(),
                "seg_{:04} missing tokens.dict",
                i
            );
            assert!(
                seg_path.join("tokens.postings").exists(),
                "seg_{:04} missing tokens.postings",
                i
            );
            assert!(
                seg_path.join("linemap.bin").exists(),
                "seg_{:04} missing linemap.bin",
                i
            );
            assert!(
                seg_path.join("bloom.bin").exists(),
                "seg_{:04} missing bloom.bin",
                i
            );
        }
    }
}
