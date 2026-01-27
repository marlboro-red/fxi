use crate::index::types::*;
use crate::utils::{delta_encode, extract_tokens, extract_trigrams, get_index_dir, is_binary, is_minified};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Index writer for building and updating the search index
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

    /// Add a file to the index
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

/// Build line offset map from content
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

    #[test]
    fn test_build_line_map() {
        let content = b"line1\nline2\nline3";
        let offsets = build_line_map(content);
        assert_eq!(offsets, vec![0, 6, 12]);
    }
}
