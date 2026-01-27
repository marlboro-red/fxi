use crate::index::types::{DocFlags, IndexConfig, Language, SegmentId};
use crate::index::writer::ChunkedIndexWriter;
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
    let chunk_size = config.chunk_size;
    let max_file_size = config.max_file_size;

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

    // Phase 2: Process in chunks
    let mut chunked_writer = ChunkedIndexWriter::new(&root, config)?;
    let error_count = Arc::new(AtomicUsize::new(0));
    let total_processed = Arc::new(AtomicUsize::new(0));

    let num_chunks = (total_files + chunk_size - 1) / chunk_size;
    if num_chunks > 1 {
        println!("Processing in {} chunks of up to {} files each", num_chunks, chunk_size);
    }

    for (chunk_idx, chunk) in file_entries.chunks(chunk_size).enumerate() {
        let segment_id = (chunk_idx + 1) as SegmentId;

        if num_chunks > 1 {
            print!("\rChunk {}/{}: processing...", chunk_idx + 1, num_chunks);
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }

        // Process chunk files in parallel
        let processed_files: Vec<ProcessedFile> = chunk
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
                    let count = total_processed.fetch_add(1, Ordering::Relaxed) + 1;
                    if num_chunks == 1 && count % 1000 == 0 {
                        print!("\rProcessing files... {}/{}", count, total_files);
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                }

                result
            })
            .collect();

        let chunk_file_count = processed_files.len();

        // Write this chunk as a segment
        chunked_writer.write_chunk(segment_id, processed_files)?;

        if num_chunks > 1 {
            println!("\rChunk {}/{}: {} files written to seg_{:04}         ",
                chunk_idx + 1, num_chunks, chunk_file_count, segment_id);
        }

        // Memory freed here - processed_files dropped
    }

    let file_count = total_processed.load(Ordering::Relaxed);
    if num_chunks == 1 {
        println!("\rProcessed {} files.                    ", file_count);
    } else {
        println!("Total: {} files processed across {} segments", file_count, num_chunks);
    }

    // Phase 3: Finalize global data
    print!("Finalizing index...");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    chunked_writer.finalize()?;
    println!(" done.");

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
