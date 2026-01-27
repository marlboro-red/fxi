use crate::index::types::*;
use crate::utils::{decode_varint, delta_decode, get_index_dir};
use anyhow::{Context, Result};
use memmap2::Mmap;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

/// Memory-mapped index reader for fast queries
pub struct IndexReader {
    root_path: PathBuf,
    index_path: PathBuf,
    pub meta: IndexMeta,
    documents: Vec<Document>,
    paths: Vec<PathBuf>,
    trigram_dict: TrigramDict,
    trigram_postings: Mmap,
    token_dict: TokenDict,
    token_postings: Mmap,
    line_maps: HashMap<DocId, Vec<u32>>,
}

/// Trigram dictionary entry
struct TrigramDictEntry {
    trigram: Trigram,
    offset: u64,
    length: u32,
    doc_freq: u32,
}

/// Trigram dictionary
struct TrigramDict {
    entries: Vec<TrigramDictEntry>,
}

impl TrigramDict {
    fn lookup(&self, trigram: Trigram) -> Option<&TrigramDictEntry> {
        self.entries
            .binary_search_by_key(&trigram, |e| e.trigram)
            .ok()
            .map(|i| &self.entries[i])
    }
}

/// Token dictionary entry
struct TokenDictEntry {
    token: String,
    offset: u64,
    length: u32,
    doc_freq: u32,
}

/// Token dictionary
struct TokenDict {
    entries: Vec<TokenDictEntry>,
}

impl TokenDict {
    fn lookup(&self, token: &str) -> Option<&TokenDictEntry> {
        self.entries
            .binary_search_by(|e| e.token.as_str().cmp(token))
            .ok()
            .map(|i| &self.entries[i])
    }
}

impl IndexReader {
    /// Open an existing index
    pub fn open(root_path: &Path) -> Result<Self> {
        let root_path = root_path.canonicalize()?;
        let index_path = get_index_dir(&root_path)?;

        if !index_path.exists() {
            anyhow::bail!("No index found. Run 'fxi index' first.");
        }

        // Read metadata
        let meta_path = index_path.join("meta.json");
        let meta_file = File::open(&meta_path).context("Failed to open meta.json")?;
        let meta: IndexMeta = serde_json::from_reader(meta_file)?;

        // Read documents
        let documents = read_documents(&index_path)?;

        // Read paths
        let paths = read_paths(&index_path)?;

        // Get the base segment path
        let segment_id = meta.base_segment.unwrap_or(1);
        let segment_path = index_path
            .join("segments")
            .join(format!("seg_{:04}", segment_id));

        // Read trigram dictionary
        let trigram_dict = read_trigram_dict(&segment_path)?;

        // mmap trigram postings
        let postings_path = segment_path.join("grams.postings");
        let trigram_postings = if postings_path.exists() {
            let file = File::open(&postings_path)?;
            unsafe { Mmap::map(&file)? }
        } else {
            // Empty mmap for empty index
            unsafe { Mmap::map(&File::open(&index_path.join("meta.json"))?)? }
        };

        // Read token dictionary
        let token_dict = read_token_dict(&segment_path)?;

        // mmap token postings
        let token_postings_path = segment_path.join("tokens.postings");
        let token_postings = if token_postings_path.exists() {
            let file = File::open(&token_postings_path)?;
            unsafe { Mmap::map(&file)? }
        } else {
            unsafe { Mmap::map(&File::open(&index_path.join("meta.json"))?)? }
        };

        // Read line maps
        let line_maps = read_line_maps(&segment_path)?;

        Ok(Self {
            root_path,
            index_path,
            meta,
            documents,
            paths,
            trigram_dict,
            trigram_postings,
            token_dict,
            token_postings,
            line_maps,
        })
    }

    /// Get document by ID
    pub fn get_document(&self, doc_id: DocId) -> Option<&Document> {
        self.documents.iter().find(|d| d.doc_id == doc_id)
    }

    /// Get path for document
    pub fn get_path(&self, doc: &Document) -> Option<&PathBuf> {
        self.paths.get(doc.path_id as usize)
    }

    /// Get full path for document
    pub fn get_full_path(&self, doc: &Document) -> Option<PathBuf> {
        self.get_path(doc).map(|p| self.root_path.join(p))
    }

    /// Get all documents
    pub fn documents(&self) -> &[Document] {
        &self.documents
    }

    /// Get documents matching a trigram
    pub fn get_trigram_docs(&self, trigram: Trigram) -> Vec<DocId> {
        if let Some(entry) = self.trigram_dict.lookup(trigram) {
            let start = entry.offset as usize;
            let end = start + entry.length as usize;

            if end <= self.trigram_postings.len() {
                return delta_decode(&self.trigram_postings[start..end]);
            }
        }
        Vec::new()
    }

    /// Get documents matching a token
    pub fn get_token_docs(&self, token: &str) -> Vec<DocId> {
        let lower = token.to_lowercase();
        if let Some(entry) = self.token_dict.lookup(&lower) {
            let start = entry.offset as usize;
            let end = start + entry.length as usize;

            if end <= self.token_postings.len() {
                return delta_decode(&self.token_postings[start..end]);
            }
        }
        Vec::new()
    }

    /// Get line offsets for a document
    pub fn get_line_map(&self, doc_id: DocId) -> Option<&Vec<u32>> {
        self.line_maps.get(&doc_id)
    }

    /// Convert byte offset to line number
    pub fn offset_to_line(&self, doc_id: DocId, offset: usize) -> u32 {
        if let Some(line_map) = self.line_maps.get(&doc_id) {
            // Binary search for the line
            match line_map.binary_search(&(offset as u32)) {
                Ok(i) => i as u32 + 1,
                Err(i) => i as u32,
            }
        } else {
            1
        }
    }

    /// Check if a trigram is a stop-gram
    pub fn is_stop_gram(&self, trigram: Trigram) -> bool {
        self.meta.stop_grams.contains(&trigram)
    }

    /// Get all valid (non-stale, non-tombstone) doc IDs
    pub fn valid_doc_ids(&self) -> Vec<DocId> {
        self.documents
            .iter()
            .filter(|d| d.is_valid())
            .map(|d| d.doc_id)
            .collect()
    }

    /// Get the root path
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }
}

/// Read documents from docs.bin
fn read_documents(index_path: &Path) -> Result<Vec<Document>> {
    let docs_path = index_path.join("docs.bin");
    let mut file = BufReader::new(File::open(&docs_path)?);

    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];
    let mut buf2 = [0u8; 2];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut documents = Vec::with_capacity(count);

    for _ in 0..count {
        file.read_exact(&mut buf4)?;
        let doc_id = u32::from_le_bytes(buf4);

        file.read_exact(&mut buf4)?;
        let path_id = u32::from_le_bytes(buf4);

        file.read_exact(&mut buf8)?;
        let size = u64::from_le_bytes(buf8);

        file.read_exact(&mut buf8)?;
        let mtime = u64::from_le_bytes(buf8);

        file.read_exact(&mut buf2)?;
        let lang_val = u16::from_le_bytes(buf2);
        let language = unsafe { std::mem::transmute::<u16, Language>(lang_val) };

        file.read_exact(&mut buf2)?;
        let flags = DocFlags(u16::from_le_bytes(buf2));

        file.read_exact(&mut buf2)?;
        let segment_id = u16::from_le_bytes(buf2);

        documents.push(Document {
            doc_id,
            path_id,
            size,
            mtime,
            language,
            flags,
            segment_id,
        });
    }

    Ok(documents)
}

/// Read paths from paths.bin
fn read_paths(index_path: &Path) -> Result<Vec<PathBuf>> {
    let paths_path = index_path.join("paths.bin");
    let mut file = BufReader::new(File::open(&paths_path)?);

    let mut buf4 = [0u8; 4];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut paths = Vec::with_capacity(count);

    for _ in 0..count {
        // Read length
        file.read_exact(&mut buf4)?;
        let len = u32::from_le_bytes(buf4) as usize;

        // Read path bytes
        let mut path_bytes = vec![0u8; len];
        file.read_exact(&mut path_bytes)?;

        let path_str = String::from_utf8_lossy(&path_bytes);
        paths.push(PathBuf::from(path_str.as_ref()));
    }

    Ok(paths)
}

/// Read trigram dictionary
fn read_trigram_dict(segment_path: &Path) -> Result<TrigramDict> {
    let dict_path = segment_path.join("grams.dict");

    if !dict_path.exists() {
        return Ok(TrigramDict {
            entries: Vec::new(),
        });
    }

    let mut file = BufReader::new(File::open(&dict_path)?);

    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut entries = Vec::with_capacity(count);

    for _ in 0..count {
        // trigram (u32)
        file.read_exact(&mut buf4)?;
        let trigram = u32::from_le_bytes(buf4);

        // offset (u64)
        file.read_exact(&mut buf8)?;
        let offset = u64::from_le_bytes(buf8);

        // length (u32)
        file.read_exact(&mut buf4)?;
        let length = u32::from_le_bytes(buf4);

        // doc_freq (u32)
        file.read_exact(&mut buf4)?;
        let doc_freq = u32::from_le_bytes(buf4);

        entries.push(TrigramDictEntry {
            trigram,
            offset,
            length,
            doc_freq,
        });
    }

    // Sort by trigram for binary search
    entries.sort_by_key(|e| e.trigram);

    Ok(TrigramDict { entries })
}

/// Read token dictionary
fn read_token_dict(segment_path: &Path) -> Result<TokenDict> {
    let dict_path = segment_path.join("tokens.dict");

    if !dict_path.exists() {
        return Ok(TokenDict {
            entries: Vec::new(),
        });
    }

    let mut file = BufReader::new(File::open(&dict_path)?);

    let mut buf2 = [0u8; 2];
    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut entries = Vec::with_capacity(count);

    for _ in 0..count {
        // token length (u16)
        file.read_exact(&mut buf2)?;
        let token_len = u16::from_le_bytes(buf2) as usize;

        // token bytes
        let mut token_bytes = vec![0u8; token_len];
        file.read_exact(&mut token_bytes)?;
        let token = String::from_utf8_lossy(&token_bytes).to_string();

        // offset (u64)
        file.read_exact(&mut buf8)?;
        let offset = u64::from_le_bytes(buf8);

        // length (u32)
        file.read_exact(&mut buf4)?;
        let length = u32::from_le_bytes(buf4);

        // doc_freq (u32)
        file.read_exact(&mut buf4)?;
        let doc_freq = u32::from_le_bytes(buf4);

        entries.push(TokenDictEntry {
            token,
            offset,
            length,
            doc_freq,
        });
    }

    // Sort by token for binary search
    entries.sort_by(|a, b| a.token.cmp(&b.token));

    Ok(TokenDict { entries })
}

/// Read line maps
fn read_line_maps(segment_path: &Path) -> Result<HashMap<DocId, Vec<u32>>> {
    let linemap_path = segment_path.join("linemap.bin");

    if !linemap_path.exists() {
        return Ok(HashMap::new());
    }

    let mut file = BufReader::new(File::open(&linemap_path)?);

    let mut buf4 = [0u8; 4];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut line_maps = HashMap::with_capacity(count);

    for _ in 0..count {
        // doc_id
        file.read_exact(&mut buf4)?;
        let doc_id = u32::from_le_bytes(buf4);

        // line count (not used, but included for consistency)
        file.read_exact(&mut buf4)?;
        let _line_count = u32::from_le_bytes(buf4);

        // encoded length
        file.read_exact(&mut buf4)?;
        let encoded_len = u32::from_le_bytes(buf4) as usize;

        // encoded data
        let mut encoded = vec![0u8; encoded_len];
        file.read_exact(&mut encoded)?;

        // Decode
        let offsets = delta_decode(&encoded);
        line_maps.insert(doc_id, offsets);
    }

    Ok(line_maps)
}
