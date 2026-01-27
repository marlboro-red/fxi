use crate::index::types::{DocFlags, IndexConfig, Language};
use crate::index::writer::IndexWriter;
use crate::utils::{
    extract_tokens, extract_trigrams, find_codebase_root, get_index_dir, is_binary, is_minified,
    remove_index,
};
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// Result of processing a single file (computed in parallel)
pub struct ProcessedFile {
    pub rel_path: PathBuf,
    pub mtime: u64,
    pub size: u64,
    pub language: Language,
    pub flags: DocFlags,
    pub trigrams: Vec<u32>,
    pub tokens: Vec<String>,
    pub line_offsets: Vec<u32>,
}

/// Process a single file's content (can run in parallel)
fn process_file_content(rel_path: PathBuf, content: &[u8], mtime: u64) -> Option<ProcessedFile> {
    // Check if binary
    if is_binary(content) {
        return None;
    }

    // Detect language from extension
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

    // Extract trigrams
    let trigrams: Vec<u32> = extract_trigrams(content).into_iter().collect();

    // Extract tokens
    let tokens: Vec<String> = if let Ok(text) = std::str::from_utf8(content) {
        extract_tokens(text).into_iter().collect()
    } else {
        Vec::new()
    };

    // Build line map
    let line_offsets = build_line_map(content);

    Some(ProcessedFile {
        rel_path,
        mtime,
        size: content.len() as u64,
        language,
        flags,
        trigrams,
        tokens,
        line_offsets,
    })
}

/// Build line offset map from content
fn build_line_map(content: &[u8]) -> Vec<u32> {
    let mut offsets = vec![0u32];
    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' && i + 1 < content.len() {
            offsets.push((i + 1) as u32);
        }
    }
    offsets
}

/// Build or rebuild the search index
pub fn build_index(root_path: &Path, force: bool) -> Result<()> {
    let root = root_path.canonicalize().context("Invalid path")?;
    let index_path = get_index_dir(&root)?;

    // Check if we should force rebuild
    if force && index_path.exists() {
        remove_index(&root).context("Failed to remove existing index")?;
    }

    let config = IndexConfig::default();
    let mut writer = IndexWriter::new(&root, config.clone())?;

    println!("Indexing: {}", root.display());

    // Phase 1: Collect all file paths
    let walker = WalkBuilder::new(&root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            // Skip common non-code directories
            !matches!(
                name.as_ref(),
                ".git" | "node_modules" | "target" | ".codesearch" | "__pycache__" | ".venv" | "venv"
            )
        })
        .build();

    // Collect file entries for parallel processing
    let file_entries: Vec<_> = walker
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| {
            let path = entry.path().to_path_buf();
            let rel_path = path.strip_prefix(&root).ok()?.to_path_buf();
            Some((path, rel_path))
        })
        .collect();

    let total_files = file_entries.len();
    println!("Found {} files to index", total_files);

    // Phase 2: Process files in parallel
    let processed_count = Arc::new(AtomicUsize::new(0));
    let error_count = Arc::new(AtomicUsize::new(0));
    let max_file_size = config.max_file_size;

    let processed_files: Vec<ProcessedFile> = file_entries
        .par_iter()
        .filter_map(|(full_path, rel_path)| {
            // Read file content
            let content = match fs::read(full_path) {
                Ok(c) => c,
                Err(_) => {
                    error_count.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            };

            // Check size limit
            if content.len() as u64 > max_file_size {
                return None;
            }

            // Get modification time
            let mtime = full_path
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64)
                .unwrap_or(0);

            // Process file content (trigrams, tokens, line map)
            let result = process_file_content(rel_path.clone(), &content, mtime);

            if result.is_some() {
                let count = processed_count.fetch_add(1, Ordering::Relaxed) + 1;
                if count % 1000 == 0 {
                    print!("\rProcessing files... {}/{}", count, total_files);
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
            }

            result
        })
        .collect();

    let file_count = processed_files.len();
    println!("\rProcessed {} files.                    ", file_count);

    // Phase 3: Merge results into writer (sequential)
    print!("Merging index data...");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    for processed in processed_files {
        writer.add_processed_file(processed);
    }
    println!(" done.");

    // Write the index
    writer.write().context("Failed to write index")?;

    println!("Index stored at: {}", index_path.display());

    let errors = error_count.load(Ordering::Relaxed);
    if errors > 0 {
        eprintln!("({} files could not be read)", errors);
    }

    Ok(())
}

/// Incrementally update the index
#[allow(dead_code)]
pub fn update_index(root_path: &Path) -> Result<()> {
    // For now, just rebuild. Full incremental support would require:
    // 1. Reading existing meta.json
    // 2. Comparing mtimes with indexed files
    // 3. Creating delta segment for changed files
    // 4. Merging if delta count exceeds threshold
    build_index(root_path, false)
}

/// Build index, detecting codebase root from current directory
pub fn build_index_auto(start_path: &Path, force: bool) -> Result<()> {
    let root = find_codebase_root(start_path)?;
    println!("Detected codebase root: {}", root.display());
    build_index(&root, force)
}
