use crate::index::build::ProcessedFile;
use crate::index::types::*;
#[allow(unused_imports)]
use crate::utils::{delta_encode, extract_tokens, extract_trigrams, get_index_dir, is_binary, is_minified, BloomFilter};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Result from processing a single chunk in parallel
/// Contains all the data needed to write a segment and merge into global state
#[derive(Debug)]
pub struct ParallelChunkResult {
    pub segment_id: SegmentId,
    /// Document ID range start (pre-assigned)
    #[allow(dead_code)]
    pub doc_id_start: DocId,
    /// Documents created in this chunk (with correct doc_ids)
    pub documents: Vec<Document>,
    /// Paths discovered in this chunk (relative paths)
    pub paths: Vec<PathBuf>,
    /// Trigram postings for this segment
    pub trigram_postings: BTreeMap<Trigram, Vec<DocId>>,
    /// Token postings for this segment
    pub token_postings: BTreeMap<String, Vec<DocId>>,
    /// Line maps for documents in this segment
    pub line_maps: HashMap<DocId, Vec<u32>>,
    /// Trigram frequencies for stop-gram computation
    pub trigram_frequencies: HashMap<Trigram, u32>,
    /// Bloom filter for fast trigram pre-filtering
    pub bloom_filter: BloomFilter,
}

impl ParallelChunkResult {
    /// Create a new chunk result by processing files with pre-assigned doc IDs
    pub fn process(
        segment_id: SegmentId,
        doc_id_start: DocId,
        processed_files: Vec<ProcessedFile>,
    ) -> Self {
        let mut documents = Vec::with_capacity(processed_files.len());
        let mut paths = Vec::with_capacity(processed_files.len());
        let mut path_to_local_id: HashMap<PathBuf, PathId> = HashMap::new();
        let mut trigram_postings: BTreeMap<Trigram, Vec<DocId>> = BTreeMap::new();
        let mut token_postings: BTreeMap<String, Vec<DocId>> = BTreeMap::new();
        let mut line_maps: HashMap<DocId, Vec<u32>> = HashMap::new();
        let mut trigram_frequencies: HashMap<Trigram, u32> = HashMap::new();

        // Estimate unique trigrams for bloom filter sizing
        // Typical: ~1000-5000 unique trigrams per file, with high overlap
        let estimated_trigrams = processed_files.len() * 500;
        let mut bloom_filter = BloomFilter::new(estimated_trigrams.max(10000), 0.01);

        let mut next_doc_id = doc_id_start;

        for processed in processed_files {
            let doc_id = next_doc_id;
            next_doc_id += 1;

            // Get or create local path ID
            let path_id = if let Some(&id) = path_to_local_id.get(&processed.rel_path) {
                id
            } else {
                let id = paths.len() as PathId;
                paths.push(processed.rel_path.clone());
                path_to_local_id.insert(processed.rel_path.clone(), id);
                id
            };

            // Create document entry
            let doc = Document {
                doc_id,
                path_id, // This is a local path ID, will be remapped during merge
                size: processed.size,
                mtime: processed.mtime,
                language: processed.language,
                flags: processed.flags,
                segment_id,
            };
            documents.push(doc);

            // Add trigrams to segment postings, bloom filter, and track frequencies
            for trigram in processed.trigrams {
                trigram_postings.entry(trigram).or_default().push(doc_id);
                *trigram_frequencies.entry(trigram).or_insert(0) += 1;
                bloom_filter.insert(trigram);
            }

            // Add tokens to segment postings
            for token in processed.tokens {
                token_postings.entry(token).or_default().push(doc_id);
            }

            // Store line map
            line_maps.insert(doc_id, processed.line_offsets);
        }

        Self {
            segment_id,
            doc_id_start,
            documents,
            paths,
            trigram_postings,
            token_postings,
            line_maps,
            trigram_frequencies,
            bloom_filter,
        }
    }
}

/// Chunked index writer for memory-bounded index building.
/// Processes files in chunks and writes each chunk as a separate segment.
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
    // Accumulated trigram frequencies for stop-gram computation
    trigram_frequencies: HashMap<Trigram, u32>,
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

        Ok(Self {
            root_path,
            index_path,
            config,
            all_documents: Vec::new(),
            all_paths: Vec::new(),
            path_to_id: HashMap::new(),
            next_doc_id: 1,
            segment_ids: Vec::new(),
            trigram_frequencies: HashMap::new(),
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

    /// Write a chunk of processed files as a segment
    pub fn write_chunk(&mut self, segment_id: SegmentId, processed_files: Vec<ProcessedFile>) -> Result<()> {
        if processed_files.is_empty() {
            return Ok(());
        }

        self.segment_ids.push(segment_id);

        // Segment-local data
        let mut trigram_postings: BTreeMap<Trigram, Vec<DocId>> = BTreeMap::new();
        let mut token_postings: BTreeMap<String, Vec<DocId>> = BTreeMap::new();
        let mut line_maps: HashMap<DocId, Vec<u32>> = HashMap::new();

        // Create bloom filter for this segment
        let estimated_trigrams = processed_files.len() * 500;
        let mut bloom_filter = BloomFilter::new(estimated_trigrams.max(10000), 0.01);

        // Process each file
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

            // Add trigrams to segment postings, bloom filter, and track global frequencies
            for trigram in processed.trigrams {
                trigram_postings.entry(trigram).or_default().push(doc_id);
                *self.trigram_frequencies.entry(trigram).or_insert(0) += 1;
                bloom_filter.insert(trigram);
            }

            // Add tokens to segment postings
            for token in processed.tokens {
                token_postings.entry(token).or_default().push(doc_id);
            }

            // Store line map
            line_maps.insert(doc_id, processed.line_offsets);
        }

        // Write segment files
        let segment_name = format!("seg_{:04}", segment_id);
        let segment_path = self.index_path.join("segments").join(&segment_name);
        fs::create_dir_all(&segment_path)?;

        // Write trigram index (without stop-gram filtering - we'll filter at query time)
        self.write_trigram_index(&segment_path, &trigram_postings)?;

        // Write token index
        self.write_token_index(&segment_path, &token_postings)?;

        // Write line maps
        self.write_line_maps(&segment_path, &line_maps)?;

        // Write bloom filter for fast pre-filtering
        Self::write_bloom_filter_static(&segment_path, &bloom_filter)?;

        Ok(())
    }

    /// Write trigram index for a segment
    fn write_trigram_index(&self, segment_path: &Path, trigram_postings: &BTreeMap<Trigram, Vec<DocId>>) -> Result<()> {
        let dict_path = segment_path.join("grams.dict");
        let postings_path = segment_path.join("grams.postings");

        let mut dict_file = BufWriter::new(File::create(&dict_path)?);
        let mut postings_file = BufWriter::new(File::create(&postings_path)?);

        // Write entry count to dictionary
        dict_file.write_all(&(trigram_postings.len() as u32).to_le_bytes())?;

        let mut postings_offset: u64 = 0;

        for (&trigram, doc_ids) in trigram_postings {
            // Sort and deduplicate doc_ids
            let mut sorted_ids: Vec<_> = doc_ids.iter().copied().collect();
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

    /// Write token index for a segment
    fn write_token_index(&self, segment_path: &Path, token_postings: &BTreeMap<String, Vec<DocId>>) -> Result<()> {
        let dict_path = segment_path.join("tokens.dict");
        let postings_path = segment_path.join("tokens.postings");

        let mut dict_file = BufWriter::new(File::create(&dict_path)?);
        let mut postings_file = BufWriter::new(File::create(&postings_path)?);

        // Write entry count
        dict_file.write_all(&(token_postings.len() as u32).to_le_bytes())?;

        let mut postings_offset: u64 = 0;

        for (token, doc_ids) in token_postings {
            // Sort and deduplicate
            let mut sorted_ids: Vec<_> = doc_ids.iter().copied().collect();
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

    /// Write line maps for a segment
    fn write_line_maps(&self, segment_path: &Path, line_maps: &HashMap<DocId, Vec<u32>>) -> Result<()> {
        let linemap_path = segment_path.join("linemap.bin");
        let mut file = BufWriter::new(File::create(&linemap_path)?);

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

    /// Compute stop-grams from accumulated frequencies
    fn compute_stop_grams(&self) -> HashSet<Trigram> {
        let mut freq: Vec<_> = self.trigram_frequencies.iter()
            .map(|(&t, &count)| (t, count as usize))
            .collect();

        freq.sort_by(|a, b| b.1.cmp(&a.1));

        freq.into_iter()
            .take(self.config.stop_gram_count)
            .map(|(t, _)| t)
            .collect()
    }

    /// Finalize the index - write global data (docs, paths, meta)
    pub fn finalize(&self) -> Result<()> {
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
        let mut file = BufWriter::new(File::create(&docs_path)?);

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
        let mut file = BufWriter::new(File::create(&paths_path)?);

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

    /// Merge and write a parallel chunk result
    /// This writes the segment files and incorporates the chunk data into global state
    #[allow(dead_code)]
    pub fn merge_chunk_result(&mut self, result: ParallelChunkResult) -> Result<()> {
        if result.documents.is_empty() {
            return Ok(());
        }

        self.segment_ids.push(result.segment_id);

        // Build path ID mapping: local path ID -> global path ID
        let mut path_id_map: HashMap<PathId, PathId> = HashMap::new();
        for (local_id, path) in result.paths.iter().enumerate() {
            let global_id = self.add_path(path);
            path_id_map.insert(local_id as PathId, global_id);
        }

        // Remap documents and add to global list
        for mut doc in result.documents {
            // Remap path_id from local to global
            if let Some(&global_path_id) = path_id_map.get(&doc.path_id) {
                doc.path_id = global_path_id;
            }
            self.all_documents.push(doc);
        }

        // Track max doc_id for future chunks
        if let Some(last_doc) = self.all_documents.last() {
            if last_doc.doc_id >= self.next_doc_id {
                self.next_doc_id = last_doc.doc_id + 1;
            }
        }

        // Merge trigram frequencies
        for (trigram, count) in result.trigram_frequencies {
            *self.trigram_frequencies.entry(trigram).or_insert(0) += count;
        }

        // Write segment files
        let segment_name = format!("seg_{:04}", result.segment_id);
        let segment_path = self.index_path.join("segments").join(&segment_name);
        fs::create_dir_all(&segment_path)?;

        // Write trigram index
        self.write_trigram_index(&segment_path, &result.trigram_postings)?;

        // Write token index
        self.write_token_index(&segment_path, &result.token_postings)?;

        // Write line maps
        self.write_line_maps(&segment_path, &result.line_maps)?;

        Ok(())
    }

    /// Get the segments directory path
    pub fn segments_path(&self) -> PathBuf {
        self.index_path.join("segments")
    }

    /// Write a parallel chunk result directly to disk (for parallel I/O)
    /// Returns the paths of written files for verification
    pub fn write_chunk_segment(index_path: &Path, result: &ParallelChunkResult) -> Result<()> {
        let segment_name = format!("seg_{:04}", result.segment_id);
        let segment_path = index_path.join("segments").join(&segment_name);
        fs::create_dir_all(&segment_path)?;

        // Write trigram index
        Self::write_trigram_index_static(&segment_path, &result.trigram_postings)?;

        // Write token index
        Self::write_token_index_static(&segment_path, &result.token_postings)?;

        // Write line maps
        Self::write_line_maps_static(&segment_path, &result.line_maps)?;

        // Write bloom filter for fast pre-filtering
        Self::write_bloom_filter_static(&segment_path, &result.bloom_filter)?;

        Ok(())
    }

    /// Write bloom filter to segment
    fn write_bloom_filter_static(segment_path: &Path, bloom_filter: &BloomFilter) -> Result<()> {
        let bloom_path = segment_path.join("bloom.bin");
        let mut file = BufWriter::new(File::create(&bloom_path)?);

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

    /// Static version of write_trigram_index for parallel use
    fn write_trigram_index_static(segment_path: &Path, trigram_postings: &BTreeMap<Trigram, Vec<DocId>>) -> Result<()> {
        let dict_path = segment_path.join("grams.dict");
        let postings_path = segment_path.join("grams.postings");

        let mut dict_file = BufWriter::new(File::create(&dict_path)?);
        let mut postings_file = BufWriter::new(File::create(&postings_path)?);

        dict_file.write_all(&(trigram_postings.len() as u32).to_le_bytes())?;

        let mut postings_offset: u64 = 0;

        for (&trigram, doc_ids) in trigram_postings {
            let mut sorted_ids: Vec<_> = doc_ids.iter().copied().collect();
            sorted_ids.sort_unstable();
            sorted_ids.dedup();

            let mut encoded = Vec::new();
            delta_encode(&sorted_ids, &mut encoded);

            dict_file.write_all(&trigram.to_le_bytes())?;
            dict_file.write_all(&postings_offset.to_le_bytes())?;
            dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            dict_file.write_all(&(sorted_ids.len() as u32).to_le_bytes())?;

            postings_file.write_all(&encoded)?;
            postings_offset += encoded.len() as u64;
        }

        dict_file.flush()?;
        postings_file.flush()?;
        Ok(())
    }

    /// Static version of write_token_index for parallel use
    fn write_token_index_static(segment_path: &Path, token_postings: &BTreeMap<String, Vec<DocId>>) -> Result<()> {
        let dict_path = segment_path.join("tokens.dict");
        let postings_path = segment_path.join("tokens.postings");

        let mut dict_file = BufWriter::new(File::create(&dict_path)?);
        let mut postings_file = BufWriter::new(File::create(&postings_path)?);

        dict_file.write_all(&(token_postings.len() as u32).to_le_bytes())?;

        let mut postings_offset: u64 = 0;

        for (token, doc_ids) in token_postings {
            let mut sorted_ids: Vec<_> = doc_ids.iter().copied().collect();
            sorted_ids.sort_unstable();
            sorted_ids.dedup();

            let mut encoded = Vec::new();
            delta_encode(&sorted_ids, &mut encoded);

            let token_bytes = token.as_bytes();
            dict_file.write_all(&(token_bytes.len() as u16).to_le_bytes())?;
            dict_file.write_all(token_bytes)?;

            dict_file.write_all(&postings_offset.to_le_bytes())?;
            dict_file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            dict_file.write_all(&(sorted_ids.len() as u32).to_le_bytes())?;

            postings_file.write_all(&encoded)?;
            postings_offset += encoded.len() as u64;
        }

        dict_file.flush()?;
        postings_file.flush()?;
        Ok(())
    }

    /// Static version of write_line_maps for parallel use
    fn write_line_maps_static(segment_path: &Path, line_maps: &HashMap<DocId, Vec<u32>>) -> Result<()> {
        let linemap_path = segment_path.join("linemap.bin");
        let mut file = BufWriter::new(File::create(&linemap_path)?);

        file.write_all(&(line_maps.len() as u32).to_le_bytes())?;

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

    /// Merge global state from parallel chunk results (after segments are written)
    pub fn merge_parallel_results(&mut self, results: Vec<ParallelChunkResult>) -> Result<()> {
        for result in results {
            self.segment_ids.push(result.segment_id);

            // Build path ID mapping: local path ID -> global path ID
            let mut path_id_map: HashMap<PathId, PathId> = HashMap::new();
            for (local_id, path) in result.paths.iter().enumerate() {
                let global_id = self.add_path(path);
                path_id_map.insert(local_id as PathId, global_id);
            }

            // Remap documents and add to global list
            for mut doc in result.documents {
                if let Some(&global_path_id) = path_id_map.get(&doc.path_id) {
                    doc.path_id = global_path_id;
                }
                self.all_documents.push(doc);
            }

            // Merge trigram frequencies
            for (trigram, count) in result.trigram_frequencies {
                *self.trigram_frequencies.entry(trigram).or_insert(0) += count;
            }
        }

        // Update next_doc_id based on merged documents
        if let Some(max_doc) = self.all_documents.iter().map(|d| d.doc_id).max() {
            self.next_doc_id = max_doc + 1;
        }

        Ok(())
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
            let mut sorted_ids: Vec<_> = doc_ids.iter().copied().collect();
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
            let mut sorted_ids: Vec<_> = doc_ids.iter().copied().collect();
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
    #[allow(dead_code)]
    pub fn mark_stale(&mut self, doc_id: DocId) {
        if let Some(doc) = self.documents.iter_mut().find(|d| d.doc_id == doc_id) {
            doc.flags.set_stale();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build::ProcessedFile;

    #[test]
    fn test_build_line_map() {
        let content = b"line1\nline2\nline3";
        let offsets = build_line_map(content);
        assert_eq!(offsets, vec![0, 6, 12]);
    }

    #[test]
    fn test_parallel_chunk_result_empty() {
        let result = ParallelChunkResult::process(1, 1, vec![]);
        assert_eq!(result.segment_id, 1);
        assert_eq!(result.doc_id_start, 1);
        assert!(result.documents.is_empty());
        assert!(result.paths.is_empty());
        assert!(result.trigram_postings.is_empty());
        assert!(result.token_postings.is_empty());
        assert!(result.line_maps.is_empty());
    }

    #[test]
    fn test_parallel_chunk_result_single_file() {
        let processed = ProcessedFile {
            rel_path: PathBuf::from("test.rs"),
            mtime: 12345,
            size: 100,
            language: Language::Rust,
            flags: DocFlags::new(),
            trigrams: vec![0x616263, 0x626364], // "abc", "bcd"
            tokens: vec!["test".to_string(), "func".to_string()],
            line_offsets: vec![0, 10, 20],
        };

        let result = ParallelChunkResult::process(1, 100, vec![processed]);

        assert_eq!(result.segment_id, 1);
        assert_eq!(result.doc_id_start, 100);
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].doc_id, 100);
        assert_eq!(result.paths.len(), 1);
        assert_eq!(result.paths[0], PathBuf::from("test.rs"));
        assert_eq!(result.trigram_postings.len(), 2);
        assert_eq!(result.token_postings.len(), 2);
        assert_eq!(result.line_maps.len(), 1);
        assert_eq!(result.line_maps.get(&100).unwrap(), &vec![0, 10, 20]);
    }

    #[test]
    fn test_parallel_chunk_result_doc_id_assignment() {
        let files: Vec<ProcessedFile> = (0..3)
            .map(|i| ProcessedFile {
                rel_path: PathBuf::from(format!("file{}.rs", i)),
                mtime: 12345,
                size: 100,
                language: Language::Rust,
                flags: DocFlags::new(),
                trigrams: vec![],
                tokens: vec![],
                line_offsets: vec![0],
            })
            .collect();

        let result = ParallelChunkResult::process(2, 50, files);

        assert_eq!(result.documents.len(), 3);
        assert_eq!(result.documents[0].doc_id, 50);
        assert_eq!(result.documents[1].doc_id, 51);
        assert_eq!(result.documents[2].doc_id, 52);
        assert_eq!(result.paths.len(), 3);
    }

    #[test]
    fn test_parallel_chunk_result_deduplicates_paths() {
        // Two files with the same path should share path_id
        let files = vec![
            ProcessedFile {
                rel_path: PathBuf::from("same.rs"),
                mtime: 12345,
                size: 100,
                language: Language::Rust,
                flags: DocFlags::new(),
                trigrams: vec![],
                tokens: vec![],
                line_offsets: vec![0],
            },
            ProcessedFile {
                rel_path: PathBuf::from("same.rs"),
                mtime: 12346,
                size: 101,
                language: Language::Rust,
                flags: DocFlags::new(),
                trigrams: vec![],
                tokens: vec![],
                line_offsets: vec![0],
            },
        ];

        let result = ParallelChunkResult::process(1, 1, files);

        assert_eq!(result.documents.len(), 2);
        assert_eq!(result.paths.len(), 1); // Only one unique path
        assert_eq!(result.documents[0].path_id, result.documents[1].path_id);
    }

    #[test]
    fn test_parallel_chunk_result_trigram_aggregation() {
        let files = vec![
            ProcessedFile {
                rel_path: PathBuf::from("a.rs"),
                mtime: 12345,
                size: 100,
                language: Language::Rust,
                flags: DocFlags::new(),
                trigrams: vec![0x616263, 0x626364], // shared trigram
                tokens: vec![],
                line_offsets: vec![0],
            },
            ProcessedFile {
                rel_path: PathBuf::from("b.rs"),
                mtime: 12345,
                size: 100,
                language: Language::Rust,
                flags: DocFlags::new(),
                trigrams: vec![0x616263, 0x636465], // shared + unique trigram
                tokens: vec![],
                line_offsets: vec![0],
            },
        ];

        let result = ParallelChunkResult::process(1, 1, files);

        // Should have 3 unique trigrams
        assert_eq!(result.trigram_postings.len(), 3);

        // Shared trigram should have both doc_ids
        let shared_posting = result.trigram_postings.get(&0x616263).unwrap();
        assert_eq!(shared_posting.len(), 2);
        assert!(shared_posting.contains(&1));
        assert!(shared_posting.contains(&2));

        // Frequency tracking
        assert_eq!(*result.trigram_frequencies.get(&0x616263).unwrap(), 2);
        assert_eq!(*result.trigram_frequencies.get(&0x626364).unwrap(), 1);
    }
}
