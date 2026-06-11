use crate::index::reader::IndexReader;
use crate::index::types::{DocFlags, IndexConfig, IndexMeta, Language, SegmentId};
use crate::index::writer::ChunkedIndexWriter;
use crate::utils::{
    extract_tokens_and_positions, extract_trigrams, find_codebase_root,
    get_index_dir, is_binary, is_minified, remove_index,
};
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

/// Check if a file extension is a known binary format (skip reading content)
pub fn is_known_binary_ext(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(),
        // Compiled/binary
        "dll" | "exe" | "pdb" | "so" | "dylib" | "a" | "lib" | "o" | "obj" |
        // Archives
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "nupkg" | "jar" | "war" | "ear" |
        // Images
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico" | "webp" | "tiff" | "tif" | "psd" |
        // Fonts
        "woff" | "woff2" | "ttf" | "eot" | "otf" |
        // Documents (binary formats)
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" |
        // Media
        "mp3" | "mp4" | "avi" | "mov" | "wav" | "ogg" | "flac" | "mkv" | "webm" |
        // Certificates/keys
        "snk" | "pfx" | "p12" | "cer" | "crt" | "p7s" | "p7b" |
        // Other binary/cache
        "cache" | "db" | "sqlite" | "mdb" | "ldf" | "mdf"
    )
}

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
    /// Token positions for positional phrase queries: (token, word_position)
    pub token_positions: Vec<(String, u32)>,
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

    // Extract trigrams using optimized bitset-based extraction
    let trigrams: Vec<u32> = extract_trigrams(content);

    // Extract tokens and token positions in a single scan of the content
    let (tokens, token_positions): (Vec<String>, Vec<(String, u32)>) =
        if let Ok(text) = std::str::from_utf8(content) {
            extract_tokens_and_positions(text)
        } else {
            (Vec::new(), Vec::new())
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
        token_positions,
    })
}

/// File bytes backed by either a memory map (borrowed zero-copy) or an
/// owned buffer. Extraction only needs `&[u8]`.
enum FileBytes {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for FileBytes {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        match self {
            FileBytes::Mapped(m) => m,
            FileBytes::Owned(v) => v,
        }
    }
}

/// Build line offset map from content using fast memchr search
fn build_line_map(content: &[u8]) -> Vec<u32> {
    use memchr::memchr_iter;

    // Pre-allocate: estimate ~50 chars per line on average
    let estimated_lines = content.len() / 50 + 1;
    let mut offsets = Vec::with_capacity(estimated_lines);
    offsets.push(0u32);

    // Use memchr for SIMD-accelerated newline search
    for pos in memchr_iter(b'\n', content) {
        if pos + 1 < content.len() {
            offsets.push((pos + 1) as u32);
        }
    }
    offsets
}

/// Build or rebuild the search index
pub fn build_index(root_path: &Path, force: bool) -> Result<()> {
    build_index_with_options(root_path, force, false, None)
}

/// Build or rebuild the search index with custom chunk size
pub fn build_index_with_chunk_size(root_path: &Path, force: bool, chunk_size: Option<usize>) -> Result<()> {
    build_index_with_options(root_path, force, false, chunk_size)
}

/// Build or rebuild the search index with optional silent mode
pub fn build_index_with_progress(root_path: &Path, force: bool, silent: bool) -> Result<()> {
    build_index_with_options(root_path, force, silent, None)
}

/// Build or rebuild the search index with all options
pub fn build_index_with_options(root_path: &Path, force: bool, silent: bool, chunk_size_override: Option<usize>) -> Result<()> {
    let root = root_path.canonicalize().context("Invalid path")?;
    let index_path = get_index_dir(&root)?;

    // Check if we should force rebuild
    if force && index_path.exists() {
        remove_index(&root).context("Failed to remove existing index")?;
    }

    let config = IndexConfig::default();
    let max_file_size = config.max_file_size;

    if !silent {
        println!("Indexing: {}", root.display());
    }

    // Phase 1: Collect all file paths with spinner
    let collect_spinner = if !silent {
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap(),
        );
        spinner.set_message("Discovering files...");
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));
        Some(spinner)
    } else {
        None
    };

    // Use parallel walker for faster file discovery
    let file_entries: Vec<(PathBuf, PathBuf)> = {
        let entries = Arc::new(Mutex::new(Vec::new()));

        struct CollectVisitor {
            root: PathBuf,
            shared: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>,
            // Per-thread buffer flushed on drop — the shared mutex is taken
            // once per walker thread instead of once per file
            local: Vec<(PathBuf, PathBuf)>,
        }

        impl CollectVisitor {
            fn flush(&mut self) {
                if !self.local.is_empty() {
                    let mut entries = self
                        .shared
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    entries.append(&mut self.local);
                }
            }
        }

        impl ignore::ParallelVisitor for CollectVisitor {
            fn visit(&mut self, result: Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState {
                if let Ok(entry) = result
                    // file_type() comes from the directory entry — no extra
                    // stat(2) per file like path().is_file()
                    && entry.file_type().is_some_and(|t| t.is_file())
                    && let Ok(rel_path) = entry.path().strip_prefix(&self.root)
                {
                    let rel_path = rel_path.to_path_buf();
                    self.local.push((entry.into_path(), rel_path));
                }
                ignore::WalkState::Continue
            }
        }

        impl Drop for CollectVisitor {
            fn drop(&mut self) {
                self.flush();
            }
        }

        struct CollectBuilder {
            root: PathBuf,
            shared: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>,
        }

        impl<'s> ignore::ParallelVisitorBuilder<'s> for CollectBuilder {
            fn build(&mut self) -> Box<dyn ignore::ParallelVisitor + 's> {
                Box::new(CollectVisitor {
                    root: self.root.clone(),
                    shared: Arc::clone(&self.shared),
                    local: Vec::with_capacity(1024),
                })
            }
        }

        let mut builder = CollectBuilder {
            root: root.clone(),
            shared: Arc::clone(&entries),
        };

        WalkBuilder::new(&root)
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
            .build_parallel()
            .visit(&mut builder);

        drop(builder);
        Arc::try_unwrap(entries).unwrap().into_inner().unwrap()
    };

    let total_files = file_entries.len();

    if let Some(spinner) = collect_spinner {
        spinner.finish_with_message(format!("Found {} files", total_files));
    }

    // Determine chunk size: 0 means all files in one chunk
    let chunk_size = match chunk_size_override {
        Some(0) => total_files.max(1), // All in one chunk
        Some(n) => n,
        None => config.chunk_size,
    };

    // Phase 2: Process in chunks
    let mut chunked_writer = ChunkedIndexWriter::new(&root, config)?;
    let error_count = Arc::new(AtomicUsize::new(0));
    let total_processed = Arc::new(AtomicUsize::new(0));

    let num_chunks = if chunk_size == 0 { 1 } else { total_files.div_ceil(chunk_size) };

    if num_chunks > 1 && !silent {
        println!(
            "Processing in {} chunks of up to {} files each",
            num_chunks, chunk_size
        );
    }

    for (chunk_idx, chunk) in file_entries.chunks(chunk_size).enumerate() {
        let segment_id = (chunk_idx + 1) as SegmentId;

        // Create progress bar for this chunk
        let progress_bar = if !silent {
            let pb = ProgressBar::new(chunk.len() as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) {msg}")
                    .unwrap()
                    .progress_chars("█▓▒░  "),
            );
            if num_chunks > 1 {
                pb.set_message(format!("Chunk {}/{}", chunk_idx + 1, num_chunks));
            } else {
                pb.set_message("Processing files...");
            }
            Some(pb)
        } else {
            None
        };

        let pb_clone = progress_bar.clone();
        let error_count_clone = error_count.clone();
        let total_processed_clone = total_processed.clone();

        // Process chunk files in parallel
        let processed_files: Vec<ProcessedFile> = chunk
            .par_iter()
            .filter_map(|(full_path, rel_path)| {
                // Fast-path for known binary extensions - skip reading content entirely
                let ext = rel_path.extension().and_then(|e| e.to_str()).unwrap_or("");

                if is_known_binary_ext(ext) {
                    if let Some(ref pb) = pb_clone {
                        pb.inc(1);
                    }
                    return None;
                }

                // Open file and get metadata
                let file = match File::open(full_path) {
                    Ok(f) => f,
                    Err(_) => {
                        error_count_clone.fetch_add(1, Ordering::Relaxed);
                        if let Some(ref pb) = pb_clone {
                            pb.inc(1);
                        }
                        return None;
                    }
                };

                let metadata = match file.metadata() {
                    Ok(m) => m,
                    Err(_) => {
                        error_count_clone.fetch_add(1, Ordering::Relaxed);
                        if let Some(ref pb) = pb_clone {
                            pb.inc(1);
                        }
                        return None;
                    }
                };

                let file_size = metadata.len();

                // Check size limit before reading
                if file_size > max_file_size {
                    if let Some(ref pb) = pb_clone {
                        pb.inc(1);
                    }
                    return None;
                }

                // Skip empty files
                if file_size == 0 {
                    if let Some(ref pb) = pb_clone {
                        pb.inc(1);
                    }
                    return None;
                }

                // Use mmap for larger files (borrowed zero-copy — extraction
                // only needs &[u8]), fall back to read for small files
                let content: FileBytes = if file_size > 4096 {
                    match unsafe { Mmap::map(&file) } {
                        Ok(mmap) => FileBytes::Mapped(mmap),
                        Err(_) => match fs::read(full_path) {
                            Ok(c) => FileBytes::Owned(c),
                            Err(_) => {
                                error_count_clone.fetch_add(1, Ordering::Relaxed);
                                if let Some(ref pb) = pb_clone {
                                    pb.inc(1);
                                }
                                return None;
                            }
                        },
                    }
                } else {
                    match fs::read(full_path) {
                        Ok(c) => FileBytes::Owned(c),
                        Err(_) => {
                            error_count_clone.fetch_add(1, Ordering::Relaxed);
                            if let Some(ref pb) = pb_clone {
                                pb.inc(1);
                            }
                            return None;
                        }
                    }
                };

                // Get modification time
                let mtime = metadata
                    .modified()
                    .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
                    .unwrap_or(0);

                // Process file content (trigrams, tokens, line map)
                let result = process_file_content(rel_path.clone(), &content, mtime);

                if result.is_some() {
                    total_processed_clone.fetch_add(1, Ordering::Relaxed);
                }

                if let Some(ref pb) = pb_clone {
                    pb.inc(1);
                }

                result
            })
            .collect();

        let chunk_file_count = processed_files.len();

        if let Some(pb) = progress_bar {
            if num_chunks > 1 {
                pb.finish_with_message(format!(
                    "Chunk {}/{}: {} files",
                    chunk_idx + 1,
                    num_chunks,
                    chunk_file_count
                ));
            } else {
                pb.finish_with_message(format!("Processed {} files", chunk_file_count));
            }
        }

        // Write this chunk as a segment
        chunked_writer.write_chunk(segment_id, processed_files)?;

        // Memory freed here - processed_files dropped
    }

    let file_count = total_processed.load(Ordering::Relaxed);
    if num_chunks > 1 && !silent {
        println!(
            "Total: {} files processed across {} segments",
            file_count, num_chunks
        );
    }

    // Phase 3: Finalize - wait for segment writes and write global data
    let total_segments = chunked_writer.total_segments();
    let finalize_progress = if !silent && total_segments > 0 {
        let pb = ProgressBar::new(total_segments as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.cyan} [{bar:40.cyan/blue}] {pos}/{len} segments written {msg}")
                .unwrap()
                .progress_chars("█▓▒░  "),
        );
        pb.set_message("");
        pb.enable_steady_tick(std::time::Duration::from_millis(80));
        Some(pb)
    } else {
        None
    };

    let pb_for_closure = finalize_progress.clone();
    chunked_writer.finalize_with_progress(|completed, _total| {
        if let Some(ref pb) = pb_for_closure {
            pb.set_position(completed as u64);
        }
    })?;

    if let Some(pb) = finalize_progress {
        pb.finish_with_message("- finalizing...");
    }

    if !silent {
        println!("Index complete");
    }

    if !silent {
        println!("Index stored at: {}", index_path.display());
    }

    let errors = error_count.load(Ordering::Relaxed);
    if errors > 0 && !silent {
        eprintln!("({} files could not be read)", errors);
    }

    Ok(())
}

/// Threshold for incremental vs full rebuild (percentage of files changed)
const INCREMENTAL_THRESHOLD_PERCENT: usize = 30;

/// Result of comparing index with filesystem
#[derive(Debug)]
struct IndexDiff {
    /// New files to add
    new_files: Vec<(PathBuf, PathBuf)>, // (full_path, rel_path)
    /// Modified files (mtime changed)
    modified_files: Vec<(PathBuf, PathBuf, u32)>, // (full_path, rel_path, old_doc_id)
    /// Deleted files (relative paths, to mark as tombstones)
    deleted_files: Vec<PathBuf>,
    /// Total files currently in index
    indexed_count: usize,
}

/// Incrementally update the index (smart mode)
/// Returns Ok(true) if incremental update was performed, Ok(false) if full rebuild was needed
pub fn update_index(root_path: &Path) -> Result<bool> {
    let root = root_path.canonicalize().context("Invalid path")?;
    let index_path = get_index_dir(&root)?;

    // If no index exists, do full build
    if !index_path.exists() {
        println!("No existing index found, performing full build...");
        build_index(&root, false)?;
        return Ok(false);
    }

    // Read existing index metadata
    let meta_path = index_path.join("meta.json");
    let meta: IndexMeta = serde_json::from_reader(
        File::open(&meta_path).context("Failed to open meta.json")?
    )?;

    // Open existing index to get file list
    let reader = IndexReader::open(&root)?;

    // Build map of indexed files: rel_path -> (doc_id, mtime)
    let mut indexed_files: HashMap<PathBuf, (u32, u64)> = HashMap::new();
    for doc_id in reader.valid_doc_ids().iter() {
        if let Some(doc) = reader.get_document(doc_id)
            && let Some(path) = reader.get_path(doc)
        {
            indexed_files.insert(path.clone(), (doc_id, doc.mtime));
        }
    }

    // Compute diff with filesystem
    let diff = compute_index_diff(&root, &indexed_files)?;

    let total_changes = diff.new_files.len() + diff.modified_files.len() + diff.deleted_files.len();

    if total_changes == 0 {
        println!("Index is up to date, no changes detected.");
        return Ok(true);
    }

    // Calculate change percentage
    let change_percent = if diff.indexed_count > 0 {
        (total_changes * 100) / diff.indexed_count
    } else {
        100
    };

    println!(
        "Detected {} changes: {} new, {} modified, {} deleted ({:.1}% of index)",
        total_changes,
        diff.new_files.len(),
        diff.modified_files.len(),
        diff.deleted_files.len(),
        change_percent as f64
    );

    // If too many changes, do full rebuild
    if change_percent > INCREMENTAL_THRESHOLD_PERCENT {
        println!("Change threshold exceeded (>{}%), performing full rebuild...", INCREMENTAL_THRESHOLD_PERCENT);
        build_index(&root, true)?;
        return Ok(false);
    }

    // Perform incremental update
    println!("Performing incremental update...");
    perform_incremental_update(&root, &meta, diff)?;

    Ok(true)
}

/// Compute the difference between indexed files and filesystem
fn compute_index_diff(root: &Path, indexed_files: &HashMap<PathBuf, (u32, u64)>) -> Result<IndexDiff> {
    let config = IndexConfig::default();
    let max_file_size = config.max_file_size;

    // Walk filesystem
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git" | "node_modules" | "target" | ".codesearch" | "__pycache__" | ".venv" | "venv"
            )
        })
        .build();

    let mut new_files = Vec::new();
    let mut modified_files = Vec::new();
    let mut seen_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for entry in walker.filter_map(|e| e.ok()) {
        // file_type() comes from the directory entry — no extra stat(2)
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }

        let full_path = entry.path().to_path_buf();
        let rel_path = match full_path.strip_prefix(root) {
            Ok(p) => p.to_path_buf(),
            Err(_) => continue,
        };

        // Check file size (metadata() reuses walker data where possible)
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if metadata.len() > max_file_size || metadata.len() == 0 {
            continue;
        }

        let current_mtime = metadata
            .modified()
            .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(0);

        seen_paths.insert(rel_path.clone());

        if let Some(&(doc_id, indexed_mtime)) = indexed_files.get(&rel_path) {
            // File exists in index - check if modified
            if current_mtime != indexed_mtime {
                modified_files.push((full_path, rel_path, doc_id));
            }
        } else {
            // New file
            new_files.push((full_path, rel_path));
        }
    }

    // Find deleted files
    let deleted_files: Vec<PathBuf> = indexed_files
        .keys()
        .filter(|path| !seen_paths.contains(*path))
        .cloned()
        .collect();

    Ok(IndexDiff {
        new_files,
        modified_files,
        deleted_files,
        indexed_count: indexed_files.len(),
    })
}

/// Read and process a single file for an incremental update.
/// Returns None for binary, empty, oversized or unreadable files.
fn process_file_for_update(
    full_path: &Path,
    rel_path: &Path,
    max_file_size: u64,
) -> Option<ProcessedFile> {
    let ext = rel_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if is_known_binary_ext(ext) {
        return None;
    }

    let metadata = fs::metadata(full_path).ok()?;
    if metadata.len() == 0 || metadata.len() > max_file_size {
        return None;
    }

    // Seconds, matching the full-build path (compute_index_diff compares
    // against this for change detection)
    let mtime = metadata
        .modified()
        .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
        .unwrap_or(0);

    let content = fs::read(full_path).ok()?;
    process_file_content(rel_path.to_path_buf(), &content, mtime)
}

/// Perform incremental update by writing the diff as a delta segment:
/// deleted and modified files are tombstoned, new and modified files are
/// indexed into a new segment, and the index metadata is committed
/// atomically. This is the same mechanism the daemon's file watcher uses.
fn perform_incremental_update(root: &Path, meta: &IndexMeta, diff: IndexDiff) -> Result<()> {
    use crate::index::writer::DeltaSegmentWriter;

    let config = IndexConfig::default();
    let mut meta = meta.clone();

    // Next segment id after base + existing deltas
    let next_segment_id = meta
        .delta_segments
        .iter()
        .max()
        .copied()
        .unwrap_or(meta.base_segment.unwrap_or(0))
        + 1;

    let mut writer = DeltaSegmentWriter::new(root, next_segment_id)?;

    // Tombstone deleted and modified files (a modified file gets a fresh
    // doc entry in the delta segment; its old entry must die)
    for rel_path in &diff.deleted_files {
        writer.mark_tombstone(rel_path);
    }
    for (_, rel_path, _) in &diff.modified_files {
        writer.mark_tombstone(rel_path);
    }

    // Index new and modified files (in parallel - extraction is the
    // expensive part; add_file itself is cheap)
    let to_index: Vec<(&PathBuf, &PathBuf)> = diff
        .new_files
        .iter()
        .map(|(full, rel)| (full, rel))
        .chain(diff.modified_files.iter().map(|(full, rel, _)| (full, rel)))
        .collect();

    let processed: Vec<ProcessedFile> = to_index
        .par_iter()
        .filter_map(|(full, rel)| process_file_for_update(full, rel, config.max_file_size))
        .collect();

    let added_count = processed.len();
    for file in processed {
        writer.add_file(file);
    }

    if !writer.has_changes() {
        println!("No indexable changes to apply.");
        return Ok(());
    }

    // Commits segment -> docs.bin -> paths.bin -> meta.json atomically
    writer.finalize(&mut meta)?;
    println!(
        "Wrote delta segment seg_{:04}: {} files indexed, {} tombstones",
        next_segment_id,
        added_count,
        diff.deleted_files.len() + diff.modified_files.len()
    );

    // Compact when fragmentation builds up (same policy as the daemon)
    let tombstone_ratio = if meta.doc_count > 0 {
        meta.tombstone_count as f32 / meta.doc_count as f32
    } else {
        0.0
    };
    let new_deltas = meta.delta_segments.len().saturating_sub(meta.delta_baseline);
    if tombstone_ratio > 0.15 || new_deltas >= crate::server::watcher::DEFAULT_MERGE_SEGMENT_THRESHOLD {
        println!(
            "Index is fragmented ({} delta segments, {} tombstones), compacting...",
            new_deltas, meta.tombstone_count
        );
        crate::index::compact::merge_segments(root)?;
    }

    Ok(())
}

/// Build index, detecting codebase root from current directory
/// Uses incremental update by default, force=true for full rebuild
pub fn build_index_auto(start_path: &Path, force: bool, chunk_size: Option<usize>) -> Result<()> {
    let root = find_codebase_root(start_path)?;
    println!("Detected codebase root: {}", root.display());

    if force || chunk_size.is_some() {
        // Force full rebuild (also when chunk_size is specified, since incremental doesn't support it)
        build_index_with_chunk_size(&root, true, chunk_size)
    } else {
        // Try incremental update first
        update_index(&root)?;
        Ok(())
    }
}
