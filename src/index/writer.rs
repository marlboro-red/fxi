use crate::index::build::ProcessedFile;
use crate::index::types::*;
#[allow(unused_imports)]
use crate::utils::{delta_encode, extract_tokens, extract_trigrams, get_index_dir, is_binary, is_minified, BloomFilter};
use anyhow::Result;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// File data with pre-assigned IDs, ready for background processing
struct AssignedFile {
    doc_id: DocId,
    trigrams: Vec<u32>,
    tokens: Vec<String>,
    line_offsets: Vec<u32>,
}

/// Data needed to write a segment to disk (sent to background thread)
struct SegmentWriteJob {
    segment_id: SegmentId,
    segment_path: PathBuf,
    files: Vec<AssignedFile>,
    trigram_frequencies: Arc<Mutex<HashMap<Trigram, u32>>>,
}

/// Chunked index writer for memory-bounded index building.
/// Processes files in chunks and writes each chunk as a separate segment.
/// Segment writes happen asynchronously in a background thread to overlap
/// I/O with processing of the next chunk.
pub struct ChunkedIndexWriter {
    root_path: PathBuf,
    index_path: PathBuf,
    config: IndexConfig,
    // Global state (persists across chunks)
    all_documents: Vec<Document>,
    all_paths: Vec<PathBuf>,
    path_to_id: HashMap<PathBuf, PathId>,
    next_doc_id: DocId,
    segment_ids: Vec<SegmentId>,
    // Accumulated trigram frequencies for stop-gram computation (shared with background thread)
    trigram_frequencies: Arc<Mutex<HashMap<Trigram, u32>>>,
    // Background writer thread
    write_sender: Option<Sender<SegmentWriteJob>>,
    write_thread: Option<JoinHandle<Vec<anyhow::Error>>>,
    // Channel for receiving segment completion notifications
    completion_receiver: Option<Receiver<SegmentId>>,
}

impl ChunkedIndexWriter {
    /// Create a new chunked index writer
    pub fn new(root_path: &Path, config: IndexConfig) -> Result<Self> {
        let root_path = root_path.canonicalize()?;
        let index_path = get_index_dir(&root_path)?;

        // Create index directory structure
        fs::create_dir_all(&index_path)?;
        let segments_path = index_path.join("segments");
        fs::create_dir_all(&segments_path)?;

        // Create channel for async segment writes
        let (tx, rx) = mpsc::channel::<SegmentWriteJob>();

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
            root_path,
            index_path,
            config,
            all_documents: Vec::new(),
            all_paths: Vec::new(),
            path_to_id: HashMap::new(),
            next_doc_id: 1,
            segment_ids: Vec::new(),
            trigram_frequencies: Arc::new(Mutex::new(HashMap::new())),
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
    pub fn write_chunk(&mut self, segment_id: SegmentId, processed_files: Vec<ProcessedFile>) -> Result<()> {
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
                tokens: processed.tokens,
                line_offsets: processed.line_offsets,
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
    fn process_and_write_segment(job: SegmentWriteJob) -> Result<()> {
        // Create segment directory
        fs::create_dir_all(&job.segment_path)?;

        let file_count = job.files.len();
        let t_start = std::time::Instant::now();

        // Flat vectors instead of HashMaps — just collect pairs, sort later
        let mut trigram_pairs: Vec<(u32, u32)> = Vec::with_capacity(file_count * 500);
        let mut token_pairs: Vec<(String, DocId)> = Vec::with_capacity(file_count * 50);
        let mut line_maps: Vec<(DocId, Vec<u32>)> = Vec::with_capacity(file_count);

        // Process each file - just append to flat vectors (no hashing)
        for file in job.files {
            let doc_id = file.doc_id;

            // Add trigram pairs
            for trigram in file.trigrams {
                trigram_pairs.push((trigram, doc_id));
            }

            // Add token pairs
            for token in file.tokens {
                token_pairs.push((token, doc_id));
            }

            // Store line map
            line_maps.push((doc_id, file.line_offsets));
        }

        let t_collect = std::time::Instant::now();

        // Sort flat pairs — par_sort is very cache-friendly on contiguous data
        trigram_pairs.par_sort_unstable();
        token_pairs.par_sort_unstable();

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
            let mut freq_map = job.trigram_frequencies
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

        // Write all segment files concurrently
        thread::scope(|s| {
            let trigram_handle = s.spawn(|| {
                Self::write_trigram_index_flat(&job.segment_path, &trigram_pairs)
            });
            let token_handle = s.spawn(|| {
                Self::write_token_index_flat(&job.segment_path, &token_pairs)
            });
            let linemap_handle = s.spawn(|| {
                Self::write_line_maps_flat(&job.segment_path, &line_maps)
            });
            let bloom_handle = s.spawn(|| {
                Self::write_bloom_filter(&job.segment_path, &bloom_filter)
            });

            trigram_handle.join().unwrap()?;
            token_handle.join().unwrap()?;
            linemap_handle.join().unwrap()?;
            bloom_handle.join().unwrap()?;
            Ok::<(), anyhow::Error>(())
        })?;

        let t_write = std::time::Instant::now();

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

        // Pre-allocate buffers for single write
        let total_postings: usize = encoded.iter().map(|(_, e, _)| e.len()).sum();
        let mut dict_buf = Vec::with_capacity(4 + entry_count * 20);
        let mut postings_buf = Vec::with_capacity(total_postings);

        dict_buf.extend_from_slice(&(entry_count as u32).to_le_bytes());
        let mut offset: u64 = 0;

        for (trigram, enc, doc_freq) in &encoded {
            dict_buf.extend_from_slice(&trigram.to_le_bytes());
            dict_buf.extend_from_slice(&offset.to_le_bytes());
            dict_buf.extend_from_slice(&(enc.len() as u32).to_le_bytes());
            dict_buf.extend_from_slice(&doc_freq.to_le_bytes());
            postings_buf.extend_from_slice(enc);
            offset += enc.len() as u64;
        }

        // Single write per file
        let mut dict_file = BufWriter::new(File::create(&dict_path)?);
        let mut postings_file = BufWriter::new(File::create(&postings_path)?);
        dict_file.write_all(&dict_buf)?;
        postings_file.write_all(&postings_buf)?;

        Ok(())
    }

    /// Write token index from pre-sorted flat pairs
    fn write_token_index_flat(segment_path: &Path, pairs: &[(String, DocId)]) -> Result<()> {
        let dict_path = segment_path.join("tokens.dict");
        let postings_path = segment_path.join("tokens.postings");

        if pairs.is_empty() {
            let mut dict_file = BufWriter::new(File::create(&dict_path)?);
            let _ = File::create(&postings_path)?;
            dict_file.write_all(&0u32.to_le_bytes())?;
            return Ok(());
        }

        // Find group boundaries
        let mut group_starts: Vec<usize> = Vec::with_capacity(pairs.len() / 10);
        group_starts.push(0);
        for i in 1..pairs.len() {
            if pairs[i].0 != pairs[i - 1].0 {
                group_starts.push(i);
            }
        }
        let entry_count = group_starts.len();

        // Parallel encode
        let encoded: Vec<(&str, Vec<u8>, u32)> = group_starts
            .par_iter()
            .enumerate()
            .map(|(idx, &start)| {
                let end = group_starts.get(idx + 1).copied().unwrap_or(pairs.len());
                let token = pairs[start].0.as_str();

                let mut doc_ids: Vec<u32> = Vec::with_capacity(end - start);
                for (_, doc_id) in &pairs[start..end] {
                    if doc_ids.last() != Some(doc_id) {
                        doc_ids.push(*doc_id);
                    }
                }

                let doc_freq = doc_ids.len() as u32;
                let mut enc = Vec::with_capacity(doc_ids.len() * 2);
                delta_encode(&doc_ids, &mut enc);

                (token, enc, doc_freq)
            })
            .collect();

        // Pre-allocate and build buffers
        let total_postings: usize = encoded.iter().map(|(_, e, _)| e.len()).sum();
        let total_dict: usize = 4 + encoded.iter().map(|(t, _, _)| 2 + t.len() + 16).sum::<usize>();
        let mut dict_buf = Vec::with_capacity(total_dict);
        let mut postings_buf = Vec::with_capacity(total_postings);

        dict_buf.extend_from_slice(&(entry_count as u32).to_le_bytes());
        let mut offset: u64 = 0;

        for (token, enc, doc_freq) in &encoded {
            let token_bytes = token.as_bytes();
            dict_buf.extend_from_slice(&(token_bytes.len() as u16).to_le_bytes());
            dict_buf.extend_from_slice(token_bytes);
            dict_buf.extend_from_slice(&offset.to_le_bytes());
            dict_buf.extend_from_slice(&(enc.len() as u32).to_le_bytes());
            dict_buf.extend_from_slice(&doc_freq.to_le_bytes());
            postings_buf.extend_from_slice(enc);
            offset += enc.len() as u64;
        }

        let mut dict_file = BufWriter::new(File::create(&dict_path)?);
        let mut postings_file = BufWriter::new(File::create(&postings_path)?);
        dict_file.write_all(&dict_buf)?;
        postings_file.write_all(&postings_buf)?;

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

    /// Write trigram index with parallel sort/encode
    fn write_trigram_index_parallel(segment_path: &Path, trigram_postings: HashMap<Trigram, Vec<DocId>>) -> Result<()> {
        let dict_path = segment_path.join("grams.dict");
        let postings_path = segment_path.join("grams.postings");

        // Convert to vec for parallel processing and sort by trigram for consistent output
        let mut entries: Vec<_> = trigram_postings.into_iter().collect();
        // Use parallel sort - faster for large entry counts
        entries.par_sort_unstable_by_key(|(trigram, _)| *trigram);

        let entry_count = entries.len();

        // Parallel sort, dedup, and encode - this is the expensive part
        let encoded_entries: Vec<(Trigram, Vec<u8>, u32)> = entries
            .into_par_iter()
            .map(|(trigram, mut doc_ids)| {
                // Sort and deduplicate in place
                doc_ids.sort_unstable();
                doc_ids.dedup();
                let doc_freq = doc_ids.len() as u32;

                // Delta-encode
                let mut encoded = Vec::with_capacity(doc_ids.len() * 2);
                delta_encode(&doc_ids, &mut encoded);

                (trigram, encoded, doc_freq)
            })
            .collect();

        // Sequential I/O write (must be sequential for correct offsets)
        let mut dict_file = BufWriter::with_capacity(131072, File::create(&dict_path)?);
        let mut postings_file = BufWriter::with_capacity(131072, File::create(&postings_path)?);

        dict_file.write_all(&(entry_count as u32).to_le_bytes())?;

        let expected_dict_size: u64 = 4 + (entry_count as u64 * 20);
        let mut postings_offset: u64 = 0;

        for (trigram, encoded, doc_freq) in encoded_entries {
            // Write dictionary entry: trigram, offset, length, doc_freq
            dict_file.write_all(&trigram.to_le_bytes())?;
            dict_file.write_all(&postings_offset.to_le_bytes())?;
            dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            dict_file.write_all(&doc_freq.to_le_bytes())?;

            postings_file.write_all(&encoded)?;
            postings_offset += encoded.len() as u64;
        }

        dict_file.flush()?;
        postings_file.flush()?;

        // Validate file sizes
        let actual_dict_size = fs::metadata(&dict_path)?.len();
        if actual_dict_size != expected_dict_size {
            anyhow::bail!(
                "Trigram dictionary size mismatch: expected {} bytes, got {} bytes",
                expected_dict_size, actual_dict_size
            );
        }

        let actual_postings_size = fs::metadata(&postings_path)?.len();
        if actual_postings_size != postings_offset {
            anyhow::bail!(
                "Trigram postings size mismatch: expected {} bytes, got {} bytes",
                postings_offset, actual_postings_size
            );
        }

        Ok(())
    }

    /// Write token index with parallel sort/encode
    fn write_token_index_parallel(segment_path: &Path, token_postings: HashMap<String, Vec<DocId>>) -> Result<()> {
        let dict_path = segment_path.join("tokens.dict");
        let postings_path = segment_path.join("tokens.postings");

        // Convert to vec for parallel processing and sort by token for consistent output
        let mut entries: Vec<_> = token_postings.into_iter().collect();
        // Use parallel sort - string comparison is expensive
        entries.par_sort_unstable_by(|(a, _), (b, _)| a.cmp(b));

        let entry_count = entries.len();

        // Parallel sort, dedup, and encode
        let encoded_entries: Vec<(String, Vec<u8>, u32)> = entries
            .into_par_iter()
            .map(|(token, mut doc_ids)| {
                doc_ids.sort_unstable();
                doc_ids.dedup();
                let doc_freq = doc_ids.len() as u32;

                let mut encoded = Vec::with_capacity(doc_ids.len() * 2);
                delta_encode(&doc_ids, &mut encoded);

                (token, encoded, doc_freq)
            })
            .collect();

        // Sequential I/O write
        let mut dict_file = BufWriter::with_capacity(131072, File::create(&dict_path)?);
        let mut postings_file = BufWriter::with_capacity(131072, File::create(&postings_path)?);

        dict_file.write_all(&(entry_count as u32).to_le_bytes())?;

        let mut expected_dict_size: u64 = 4;
        let mut postings_offset: u64 = 0;

        for (token, encoded, doc_freq) in encoded_entries {
            let token_bytes = token.as_bytes();

            // Write token (length-prefixed)
            dict_file.write_all(&(token_bytes.len() as u16).to_le_bytes())?;
            dict_file.write_all(token_bytes)?;

            // Write offset, length, freq
            dict_file.write_all(&postings_offset.to_le_bytes())?;
            dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            dict_file.write_all(&doc_freq.to_le_bytes())?;

            expected_dict_size += 18 + token_bytes.len() as u64;

            postings_file.write_all(&encoded)?;
            postings_offset += encoded.len() as u64;
        }

        dict_file.flush()?;
        postings_file.flush()?;

        // Validate file sizes
        let actual_dict_size = fs::metadata(&dict_path)?.len();
        if actual_dict_size != expected_dict_size {
            anyhow::bail!(
                "Token dictionary size mismatch: expected {} bytes, got {} bytes",
                expected_dict_size, actual_dict_size
            );
        }

        let actual_postings_size = fs::metadata(&postings_path)?.len();
        if actual_postings_size != postings_offset {
            anyhow::bail!(
                "Token postings size mismatch: expected {} bytes, got {} bytes",
                postings_offset, actual_postings_size
            );
        }

        Ok(())
    }

    /// Write line maps for a segment (static version for background thread)
    fn write_line_maps_static(segment_path: &Path, line_maps: &HashMap<DocId, Vec<u32>>) -> Result<()> {
        let linemap_path = segment_path.join("linemap.bin");
        let mut file = BufWriter::with_capacity(65536, File::create(&linemap_path)?);

        // Write count
        file.write_all(&(line_maps.len() as u32).to_le_bytes())?;

        // Sort by doc_id for consistent ordering
        let mut sorted: Vec<_> = line_maps.iter().collect();
        sorted.sort_by_key(|(id, _)| *id);

        for (&doc_id, offsets) in sorted {
            // Write doc_id, line count
            file.write_all(&doc_id.to_le_bytes())?;
            file.write_all(&(offsets.len() as u32).to_le_bytes())?;

            // Delta-encode line offsets
            let mut encoded = Vec::new();
            delta_encode(offsets, &mut encoded);
            file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            file.write_all(&encoded)?;
        }

        file.flush()?;
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

        // Write bit data
        for &word in bits {
            file.write_all(&word.to_le_bytes())?;
        }

        file.flush()?;
        Ok(())
    }

    /// Compute stop-grams from accumulated frequencies
    fn compute_stop_grams(&self) -> HashSet<Trigram> {
        let freq_map = self.trigram_frequencies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut freq: Vec<_> = freq_map.iter()
            .map(|(&t, &count)| (t, count as usize))
            .collect();

        freq.sort_by(|a, b| b.1.cmp(&a.1));

        freq.into_iter()
            .take(self.config.stop_gram_count)
            .map(|(t, _)| t)
            .collect()
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
            let errors = handle.join().map_err(|_| anyhow::anyhow!("Background write thread panicked"))?;
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

        Ok(())
    }

    /// Write document table
    fn write_documents(&self) -> Result<()> {
        let docs_path = self.index_path.join("docs.bin");
        let mut file = BufWriter::with_capacity(65536, File::create(&docs_path)?);

        // Write document count
        file.write_all(&(self.all_documents.len() as u32).to_le_bytes())?;

        for doc in &self.all_documents {
            file.write_all(&doc.doc_id.to_le_bytes())?;
            file.write_all(&doc.path_id.to_le_bytes())?;
            file.write_all(&doc.size.to_le_bytes())?;
            file.write_all(&doc.mtime.to_le_bytes())?;
            file.write_all(&(doc.language as u16).to_le_bytes())?;
            file.write_all(&doc.flags.0.to_le_bytes())?;
            file.write_all(&doc.segment_id.to_le_bytes())?;
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
            version: 1,
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
        };

        let meta_path = self.index_path.join("meta.json");
        let file = File::create(&meta_path)?;
        serde_json::to_writer_pretty(file, &meta)?;

        Ok(())
    }

    /// Get the index path
    #[allow(dead_code)]
    pub fn index_path(&self) -> &Path {
        &self.index_path
    }
}

/// Index writer for building and updating the search index (used for incremental updates)
#[allow(dead_code)]
pub struct IndexWriter {
    root_path: PathBuf,
    index_path: PathBuf,
    config: IndexConfig,
    documents: Vec<Document>,
    path_to_id: HashMap<PathBuf, PathId>,
    paths: Vec<PathBuf>,
    segment_id: SegmentId,
    /// Trigram -> list of doc_ids (accumulated during build)
    trigram_postings: BTreeMap<Trigram, Vec<DocId>>,
    /// Token -> list of doc_ids
    token_postings: BTreeMap<String, Vec<DocId>>,
    /// Line offsets per document
    line_maps: HashMap<DocId, Vec<u32>>,
    /// Doc IDs from existing index that should be marked as stale (for incremental updates)
    external_stale_ids: Vec<DocId>,
}

#[allow(dead_code)]
impl IndexWriter {
    /// Create a new index writer
    pub fn new(root_path: &Path, config: IndexConfig) -> Result<Self> {
        let root_path = root_path.canonicalize()?;
        let index_path = get_index_dir(&root_path)?;

        Ok(Self {
            root_path,
            index_path,
            config,
            documents: Vec::new(),
            path_to_id: HashMap::new(),
            paths: Vec::new(),
            segment_id: 1,
            trigram_postings: BTreeMap::new(),
            token_postings: BTreeMap::new(),
            line_maps: HashMap::new(),
            external_stale_ids: Vec::new(),
        })
    }

    /// Create a delta segment writer
    #[allow(dead_code)]
    pub fn new_delta(root_path: &Path, config: IndexConfig, segment_id: SegmentId) -> Result<Self> {
        let mut writer = Self::new(root_path, config)?;
        writer.segment_id = segment_id;
        Ok(writer)
    }

    /// Add a file to the index (used for incremental updates)
    #[allow(dead_code)]
    pub fn add_file(&mut self, rel_path: &Path, content: &[u8], mtime: u64) -> Result<DocId> {
        // Check if binary
        if is_binary(content) {
            return Ok(0); // Skip binary files
        }

        // Check size limit
        if content.len() as u64 > self.config.max_file_size {
            return Ok(0);
        }

        let doc_id = self.documents.len() as DocId + 1;
        let path_id = self.add_path(rel_path);

        // Detect language
        let ext = rel_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let language = Language::from_extension(ext);

        // Check for minified
        let mut flags = DocFlags::new();
        if is_minified(content) {
            flags.0 |= DocFlags::MINIFIED;
        }

        // Create document entry
        let doc = Document {
            doc_id,
            path_id,
            size: content.len() as u64,
            mtime,
            language,
            flags,
            segment_id: self.segment_id,
        };
        self.documents.push(doc);

        // Extract and index trigrams
        let trigrams = extract_trigrams(content);
        for trigram in trigrams {
            self.trigram_postings
                .entry(trigram)
                .or_default()
                .push(doc_id);
        }

        // Extract and index tokens
        if let Ok(text) = std::str::from_utf8(content) {
            let tokens = extract_tokens(text);
            for token in tokens {
                self.token_postings.entry(token).or_default().push(doc_id);
            }
        }

        // Build line map
        let line_offsets = build_line_map(content);
        self.line_maps.insert(doc_id, line_offsets);

        Ok(doc_id)
    }

    /// Add a pre-processed file to the index (for parallel indexing)
    pub fn add_processed_file(&mut self, processed: ProcessedFile) -> DocId {
        let doc_id = self.documents.len() as DocId + 1;
        let path_id = self.add_path(&processed.rel_path);

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
        self.documents.push(doc);

        // Add trigrams to postings
        for trigram in processed.trigrams {
            self.trigram_postings
                .entry(trigram)
                .or_default()
                .push(doc_id);
        }

        // Add tokens to postings
        for token in processed.tokens {
            self.token_postings.entry(token).or_default().push(doc_id);
        }

        // Store line map
        self.line_maps.insert(doc_id, processed.line_offsets);

        doc_id
    }

    /// Get or create path ID
    fn add_path(&mut self, path: &Path) -> PathId {
        if let Some(&id) = self.path_to_id.get(path) {
            return id;
        }

        let id = self.paths.len() as PathId;
        self.paths.push(path.to_path_buf());
        self.path_to_id.insert(path.to_path_buf(), id);
        id
    }

    /// Write the index to disk
    pub fn write(&self) -> Result<()> {
        // Create index directory structure
        fs::create_dir_all(&self.index_path)?;
        let segments_path = self.index_path.join("segments");
        fs::create_dir_all(&segments_path)?;

        let segment_name = format!("seg_{:04}", self.segment_id);
        let segment_path = segments_path.join(&segment_name);
        fs::create_dir_all(&segment_path)?;

        // Write documents table
        self.write_documents()?;

        // Write path store (simple for now, can upgrade to FST later)
        self.write_paths()?;

        // Compute stop-grams
        let stop_grams = self.compute_stop_grams();

        // Write trigram index
        self.write_trigram_index(&segment_path, &stop_grams)?;

        // Write token index
        self.write_token_index(&segment_path)?;

        // Write line maps
        self.write_line_maps(&segment_path)?;

        // Write metadata
        self.write_meta(&stop_grams)?;

        Ok(())
    }

    /// Write document table
    fn write_documents(&self) -> Result<()> {
        let docs_path = self.index_path.join("docs.bin");
        let mut file = BufWriter::new(File::create(&docs_path)?);

        // Write document count
        file.write_all(&(self.documents.len() as u32).to_le_bytes())?;

        for doc in &self.documents {
            file.write_all(&doc.doc_id.to_le_bytes())?;
            file.write_all(&doc.path_id.to_le_bytes())?;
            file.write_all(&doc.size.to_le_bytes())?;
            file.write_all(&doc.mtime.to_le_bytes())?;
            file.write_all(&(doc.language as u16).to_le_bytes())?;
            file.write_all(&doc.flags.0.to_le_bytes())?;
            file.write_all(&doc.segment_id.to_le_bytes())?;
        }

        file.flush()?;
        Ok(())
    }

    /// Write path store
    fn write_paths(&self) -> Result<()> {
        let paths_path = self.index_path.join("paths.bin");
        let mut file = BufWriter::new(File::create(&paths_path)?);

        // Simple format: count, then [length, bytes]...
        file.write_all(&(self.paths.len() as u32).to_le_bytes())?;

        for path in &self.paths {
            let path_str = path.to_string_lossy();
            let bytes = path_str.as_bytes();
            file.write_all(&(bytes.len() as u32).to_le_bytes())?;
            file.write_all(bytes)?;
        }

        file.flush()?;
        Ok(())
    }

    /// Compute stop-grams (most frequent trigrams)
    fn compute_stop_grams(&self) -> HashSet<Trigram> {
        let mut freq: Vec<_> = self
            .trigram_postings
            .iter()
            .map(|(&t, v)| (t, v.len()))
            .collect();

        freq.sort_by(|a, b| b.1.cmp(&a.1));

        freq.into_iter()
            .take(self.config.stop_gram_count)
            .map(|(t, _)| t)
            .collect()
    }

    /// Write trigram index (dictionary + postings)
    fn write_trigram_index(&self, segment_path: &Path, stop_grams: &HashSet<Trigram>) -> Result<()> {
        let dict_path = segment_path.join("grams.dict");
        let postings_path = segment_path.join("grams.postings");

        let mut dict_file = BufWriter::new(File::create(&dict_path)?);
        let mut postings_file = BufWriter::new(File::create(&postings_path)?);

        // Write entry count to dictionary
        let filtered_count = self
            .trigram_postings
            .keys()
            .filter(|t| !stop_grams.contains(t))
            .count();
        dict_file.write_all(&(filtered_count as u32).to_le_bytes())?;

        let mut postings_offset: u64 = 0;

        for (&trigram, doc_ids) in &self.trigram_postings {
            // Skip stop-grams
            if stop_grams.contains(&trigram) {
                continue;
            }

            // Sort and deduplicate doc_ids
            let mut sorted_ids = doc_ids.to_vec();
            sorted_ids.sort_unstable();
            sorted_ids.dedup();

            // Delta-encode postings
            let mut encoded = Vec::new();
            delta_encode(&sorted_ids, &mut encoded);

            // Write dictionary entry: trigram, offset, length, doc_freq
            dict_file.write_all(&trigram.to_le_bytes())?;
            dict_file.write_all(&postings_offset.to_le_bytes())?;
            dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            dict_file.write_all(&(sorted_ids.len() as u32).to_le_bytes())?;

            // Write postings
            postings_file.write_all(&encoded)?;
            postings_offset += encoded.len() as u64;
        }

        dict_file.flush()?;
        postings_file.flush()?;
        Ok(())
    }

    /// Write token index
    fn write_token_index(&self, segment_path: &Path) -> Result<()> {
        let dict_path = segment_path.join("tokens.dict");
        let postings_path = segment_path.join("tokens.postings");

        let mut dict_file = BufWriter::new(File::create(&dict_path)?);
        let mut postings_file = BufWriter::new(File::create(&postings_path)?);

        // Write entry count
        dict_file.write_all(&(self.token_postings.len() as u32).to_le_bytes())?;

        let mut postings_offset: u64 = 0;

        for (token, doc_ids) in &self.token_postings {
            // Sort and deduplicate
            let mut sorted_ids = doc_ids.to_vec();
            sorted_ids.sort_unstable();
            sorted_ids.dedup();

            // Delta-encode
            let mut encoded = Vec::new();
            delta_encode(&sorted_ids, &mut encoded);

            // Write token (length-prefixed)
            let token_bytes = token.as_bytes();
            dict_file.write_all(&(token_bytes.len() as u16).to_le_bytes())?;
            dict_file.write_all(token_bytes)?;

            // Write offset, length, freq
            dict_file.write_all(&postings_offset.to_le_bytes())?;
            dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            dict_file.write_all(&(sorted_ids.len() as u32).to_le_bytes())?;

            // Write postings
            postings_file.write_all(&encoded)?;
            postings_offset += encoded.len() as u64;
        }

        dict_file.flush()?;
        postings_file.flush()?;
        Ok(())
    }

    /// Write line maps
    fn write_line_maps(&self, segment_path: &Path) -> Result<()> {
        let linemap_path = segment_path.join("linemap.bin");
        let mut file = BufWriter::new(File::create(&linemap_path)?);

        // Write count
        file.write_all(&(self.line_maps.len() as u32).to_le_bytes())?;

        // Sort by doc_id for consistent ordering
        let mut sorted: Vec<_> = self.line_maps.iter().collect();
        sorted.sort_by_key(|(id, _)| *id);

        for (&doc_id, offsets) in sorted {
            // Write doc_id, line count
            file.write_all(&doc_id.to_le_bytes())?;
            file.write_all(&(offsets.len() as u32).to_le_bytes())?;

            // Delta-encode line offsets
            let mut encoded = Vec::new();
            delta_encode(offsets, &mut encoded);
            file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            file.write_all(&encoded)?;
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

        let valid_doc_count = self.documents.len() as u32;

        let meta = IndexMeta {
            version: 1,
            root_path: self.root_path.clone(),
            doc_count: self.documents.len() as u32,
            segment_count: 1,
            base_segment: Some(self.segment_id),
            delta_segments: Vec::new(),
            stop_grams: stop_grams.iter().copied().collect(),
            created_at: now,
            updated_at: now,
            tombstone_count: 0, // Fresh index has no tombstones
            valid_doc_count,
            delta_baseline: 0, // No delta segments in single-segment index
        };

        let meta_path = self.index_path.join("meta.json");
        let file = File::create(&meta_path)?;
        serde_json::to_writer_pretty(file, &meta)?;

        Ok(())
    }

    /// Get current document count
    #[allow(dead_code)]
    pub fn doc_count(&self) -> usize {
        self.documents.len()
    }

    /// Mark a document as stale (for incremental updates)
    /// This tracks doc_ids from the existing index that should be skipped
    #[allow(dead_code)]
    pub fn mark_stale(&mut self, doc_id: DocId) {
        // First try to mark in our own documents
        if let Some(doc) = self.documents.iter_mut().find(|d| d.doc_id == doc_id) {
            doc.flags.set_stale();
        } else {
            // Track external doc_id for writing to stale file
            self.external_stale_ids.push(doc_id);
        }
    }

    /// Add a file with pre-processed data (for incremental updates)
    #[allow(dead_code)]
    pub fn add_processed(&mut self, file: ProcessedFile) -> Result<DocId> {
        let doc_id = self.documents.len() as DocId + 1;
        let path_id = self.add_path(&file.rel_path);

        // Create document entry
        let doc = Document {
            doc_id,
            path_id,
            size: file.size,
            mtime: file.mtime,
            language: file.language,
            flags: file.flags,
            segment_id: self.segment_id,
        };
        self.documents.push(doc);

        // Add trigrams to postings
        for trigram in file.trigrams {
            self.trigram_postings
                .entry(trigram)
                .or_default()
                .push(doc_id);
        }

        // Add tokens to postings
        for token in file.tokens {
            self.token_postings.entry(token).or_default().push(doc_id);
        }

        // Store line map
        self.line_maps.insert(doc_id, file.line_offsets);

        Ok(doc_id)
    }

    /// Finalize the delta segment - write all data to disk
    #[allow(dead_code)]
    pub fn finalize(&self) -> Result<()> {
        // Create segment directory
        let segment_path = self.index_path
            .join("segments")
            .join(format!("seg_{:04}", self.segment_id));
        fs::create_dir_all(&segment_path)?;

        // Write documents for this segment
        self.write_segment_documents(&segment_path)?;

        // Write paths for this segment
        self.write_segment_paths(&segment_path)?;

        // Write trigram index
        let stop_grams = self.compute_stop_grams();
        self.write_trigram_index(&segment_path, &stop_grams)?;

        // Write token index
        self.write_token_index(&segment_path)?;

        // Write line maps
        self.write_line_maps(&segment_path)?;

        // Write stale doc IDs (for delta segments)
        if !self.external_stale_ids.is_empty() {
            self.write_stale_ids(&segment_path)?;
        }

        Ok(())
    }

    /// Write documents for this segment
    fn write_segment_documents(&self, segment_path: &Path) -> Result<()> {
        use std::io::BufWriter;
        let docs_path = segment_path.join("documents.bin");
        let mut file = BufWriter::new(File::create(&docs_path)?);

        // Write count
        file.write_all(&(self.documents.len() as u32).to_le_bytes())?;

        // Write each document
        for doc in &self.documents {
            file.write_all(&doc.doc_id.to_le_bytes())?;
            file.write_all(&doc.path_id.to_le_bytes())?;
            file.write_all(&doc.size.to_le_bytes())?;
            file.write_all(&doc.mtime.to_le_bytes())?;
            file.write_all(&(doc.language as u8).to_le_bytes())?;
            file.write_all(&doc.flags.0.to_le_bytes())?;
            file.write_all(&doc.segment_id.to_le_bytes())?;
        }

        file.flush()?;
        Ok(())
    }

    /// Write paths for this segment
    fn write_segment_paths(&self, segment_path: &Path) -> Result<()> {
        use std::io::BufWriter;
        let paths_path = segment_path.join("paths.bin");
        let mut file = BufWriter::new(File::create(&paths_path)?);

        // Write count
        file.write_all(&(self.paths.len() as u32).to_le_bytes())?;

        // Write each path (length-prefixed)
        for path in &self.paths {
            let path_bytes = path.to_string_lossy().as_bytes().to_vec();
            file.write_all(&(path_bytes.len() as u32).to_le_bytes())?;
            file.write_all(&path_bytes)?;
        }

        file.flush()?;
        Ok(())
    }

    /// Write stale doc IDs file
    fn write_stale_ids(&self, segment_path: &Path) -> Result<()> {
        use std::io::BufWriter;
        let stale_path = segment_path.join("stale.bin");
        let mut file = BufWriter::new(File::create(&stale_path)?);

        // Write count
        file.write_all(&(self.external_stale_ids.len() as u32).to_le_bytes())?;

        // Write each stale doc_id
        for &doc_id in &self.external_stale_ids {
            file.write_all(&doc_id.to_le_bytes())?;
        }

        file.flush()?;
        Ok(())
    }
}

/// Build line offset map from content (used by add_file for incremental updates)
#[allow(dead_code)]
fn build_line_map(content: &[u8]) -> Vec<u32> {
    let mut offsets = vec![0u32]; // Line 1 starts at offset 0

    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' && i + 1 < content.len() {
            offsets.push((i + 1) as u32);
        }
    }

    offsets
}

// =============================================================================
// Delta Segment Writer - For incremental index updates
// =============================================================================

/// Maximum number of delta segments before forcing a full rebuild

/// Delta segment writer for incremental index updates.
/// Loads existing index data and writes a new delta segment containing only changed files.
pub struct DeltaSegmentWriter {
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

        // Build path lookup map
        let mut path_to_id: HashMap<PathBuf, PathId> = HashMap::new();
        for (idx, path) in existing_paths.iter().enumerate() {
            path_to_id.insert(path.clone(), idx as PathId);
        }

        // Calculate next IDs
        let next_doc_id = existing_documents.iter().map(|d| d.doc_id).max().unwrap_or(0) + 1;
        let next_path_id = existing_paths.len() as PathId;

        Ok(Self {
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
            self.trigram_postings.entry(trigram).or_default().push(doc_id);
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
    pub fn finalize(self, meta: &mut IndexMeta) -> Result<()> {
        // If no changes, nothing to do
        if !self.has_changes() {
            return Ok(());
        }

        // Create segment directory if we have new documents
        if !self.new_documents.is_empty() {
            let segment_path = self.index_path
                .join("segments")
                .join(format!("seg_{:04}", self.segment_id));
            fs::create_dir_all(&segment_path)?;

            // Write segment files
            self.write_segment_files(&segment_path)?;
        }

        // Merge existing and new documents, applying tombstones
        let tombstone_set: HashSet<DocId> = self.tombstone_doc_ids.iter().copied().collect();
        let mut all_documents: Vec<Document> = self.existing_documents
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
        meta.doc_count = all_documents.len() as u32;
        meta.delta_segments.push(self.segment_id);
        meta.segment_count = 1 + meta.delta_segments.len() as u16;
        meta.updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Track fragmentation metrics
        meta.tombstone_count = all_documents
            .iter()
            .filter(|d| d.flags.is_tombstone())
            .count() as u32;
        meta.valid_doc_count = all_documents
            .iter()
            .filter(|d| d.is_valid())
            .count() as u32;

        // Write meta.json atomically (commits the transaction)
        write_meta_atomic(&self.index_path, meta)?;

        Ok(())
    }

    /// Write segment files (trigrams, tokens, line maps, bloom filter)
    fn write_segment_files(&self, segment_path: &Path) -> Result<()> {
        // Compute bloom filter
        let estimated_trigrams = self.trigram_postings.len();
        let mut bloom_filter = BloomFilter::new(estimated_trigrams.max(1000), 0.01);
        for &trigram in self.trigram_postings.keys() {
            bloom_filter.insert(trigram);
        }

        // Write trigram index
        self.write_trigram_index(segment_path)?;

        // Write token index
        self.write_token_index(segment_path)?;

        // Write line maps
        self.write_line_maps(segment_path)?;

        // Write bloom filter
        Self::write_bloom_filter_static(segment_path, &bloom_filter)?;

        Ok(())
    }

    /// Write trigram index
    fn write_trigram_index(&self, segment_path: &Path) -> Result<()> {
        let dict_path = segment_path.join("grams.dict");
        let postings_path = segment_path.join("grams.postings");

        let mut dict_file = BufWriter::with_capacity(65536, File::create(&dict_path)?);
        let mut postings_file = BufWriter::with_capacity(65536, File::create(&postings_path)?);

        // Write entry count
        dict_file.write_all(&(self.trigram_postings.len() as u32).to_le_bytes())?;

        let mut postings_offset: u64 = 0;

        for (&trigram, doc_ids) in &self.trigram_postings {
            // Sort and deduplicate
            let mut sorted_ids = doc_ids.clone();
            sorted_ids.sort_unstable();
            sorted_ids.dedup();

            // Delta-encode
            let mut encoded = Vec::new();
            delta_encode(&sorted_ids, &mut encoded);

            // Write dictionary entry
            dict_file.write_all(&trigram.to_le_bytes())?;
            dict_file.write_all(&postings_offset.to_le_bytes())?;
            dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            dict_file.write_all(&(sorted_ids.len() as u32).to_le_bytes())?;

            // Write postings
            postings_file.write_all(&encoded)?;
            postings_offset += encoded.len() as u64;
        }

        dict_file.flush()?;
        postings_file.flush()?;
        Ok(())
    }

    /// Write token index
    fn write_token_index(&self, segment_path: &Path) -> Result<()> {
        let dict_path = segment_path.join("tokens.dict");
        let postings_path = segment_path.join("tokens.postings");

        let mut dict_file = BufWriter::with_capacity(65536, File::create(&dict_path)?);
        let mut postings_file = BufWriter::with_capacity(65536, File::create(&postings_path)?);

        // Write entry count
        dict_file.write_all(&(self.token_postings.len() as u32).to_le_bytes())?;

        let mut postings_offset: u64 = 0;

        for (token, doc_ids) in &self.token_postings {
            // Sort and deduplicate
            let mut sorted_ids = doc_ids.clone();
            sorted_ids.sort_unstable();
            sorted_ids.dedup();

            // Delta-encode
            let mut encoded = Vec::new();
            delta_encode(&sorted_ids, &mut encoded);

            // Write token (length-prefixed)
            let token_bytes = token.as_bytes();
            dict_file.write_all(&(token_bytes.len() as u16).to_le_bytes())?;
            dict_file.write_all(token_bytes)?;

            // Write offset, length, freq
            dict_file.write_all(&postings_offset.to_le_bytes())?;
            dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            dict_file.write_all(&(sorted_ids.len() as u32).to_le_bytes())?;

            // Write postings
            postings_file.write_all(&encoded)?;
            postings_offset += encoded.len() as u64;
        }

        dict_file.flush()?;
        postings_file.flush()?;
        Ok(())
    }

    /// Write line maps
    fn write_line_maps(&self, segment_path: &Path) -> Result<()> {
        let linemap_path = segment_path.join("linemap.bin");
        let mut file = BufWriter::with_capacity(65536, File::create(&linemap_path)?);

        // Write count
        file.write_all(&(self.line_maps.len() as u32).to_le_bytes())?;

        // Sort by doc_id
        let mut sorted: Vec<_> = self.line_maps.iter().collect();
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

    /// Write bloom filter
    fn write_bloom_filter_static(segment_path: &Path, bloom_filter: &BloomFilter) -> Result<()> {
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
    fn test_build_line_map() {
        let content = b"line1\nline2\nline3";
        let offsets = build_line_map(content);
        assert_eq!(offsets, vec![0, 6, 12]);
    }

    fn create_test_processed_file(rel_path: &str, content: &str) -> ProcessedFile {
        let trigrams: Vec<u32> = content
            .as_bytes()
            .windows(3)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], 0]))
            .collect();
        let tokens: Vec<String> = content
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .collect();
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
            create_test_processed_file("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}"),
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
        let meta: IndexMeta = serde_json::from_reader(
            File::open(index_path.join("meta.json")).unwrap()
        ).unwrap();
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
        let files = vec![
            create_test_processed_file("src/main.rs", "fn main() {}"),
        ];
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
        let files1 = vec![
            create_test_processed_file("a.rs", "hello world"),
        ];
        writer.write_chunk(1, files1).unwrap();

        let files2 = vec![
            create_test_processed_file("b.rs", "hello there"),
        ];
        writer.write_chunk(2, files2).unwrap();

        writer.finalize().unwrap();

        // Verify stop-grams were computed (meta.json should have stop_grams)
        let index_path = crate::utils::get_index_dir(root).unwrap();
        let meta: IndexMeta = serde_json::from_reader(
            File::open(index_path.join("meta.json")).unwrap()
        ).unwrap();

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
            let files = vec![
                create_test_processed_file(
                    &format!("src/file{}.rs", i),
                    &format!("pub fn func{}() {{}}", i),
                ),
            ];
            writer.write_chunk(i as u16, files).unwrap();
        }

        // Finalize should wait for all async writes
        writer.finalize().unwrap();

        // Verify all segments exist and have required files
        let index_path = crate::utils::get_index_dir(root).unwrap();
        for i in 1..=5 {
            let seg_path = index_path.join("segments").join(format!("seg_{:04}", i));
            assert!(seg_path.join("grams.dict").exists(), "seg_{:04} missing grams.dict", i);
            assert!(seg_path.join("grams.postings").exists(), "seg_{:04} missing grams.postings", i);
            assert!(seg_path.join("tokens.dict").exists(), "seg_{:04} missing tokens.dict", i);
            assert!(seg_path.join("tokens.postings").exists(), "seg_{:04} missing tokens.postings", i);
            assert!(seg_path.join("linemap.bin").exists(), "seg_{:04} missing linemap.bin", i);
            assert!(seg_path.join("bloom.bin").exists(), "seg_{:04} missing bloom.bin", i);
        }
    }
}
