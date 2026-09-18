use crate::index::reader::IndexReader;
use crate::index::types::{DocFlags, IndexConfig, IndexMeta, IndexProfile, Language, SegmentId};
use crate::index::writer::ChunkedIndexWriter;
use crate::utils::{
    extract_tokens_and_positions, extract_trigrams, find_codebase_root, get_index_dir, is_binary,
    is_minified,
};
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

/// Check if a file extension is a known binary format (skip reading content)
pub fn is_known_binary_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
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
        "mp3" | "mp4" | "avi" | "mov" | "wav" | "ogg" | "oga" | "flac" | "mkv" | "webm" |
        // Certificates/keys
        "snk" | "pfx" | "p12" | "cer" | "crt" | "p7s" | "p7b" |
        // Compiled web/mobile artifacts
        "wasm" | "svgz" | "pak" | "crx" | "apk" | "dex" | "class" |
        // Color profiles
        "icc" | "icm" |
        // Other binary/cache
        "cache" | "db" | "sqlite" | "mdb" | "ldf" | "mdf"
    )
}

fn checked_chunk_count(file_count: usize, chunk_size: usize) -> Result<usize> {
    anyhow::ensure!(
        chunk_size > 0,
        "Chunk size must be positive after resolving overrides"
    );
    let count = file_count.div_ceil(chunk_size);
    anyhow::ensure!(
        count <= usize::from(SegmentId::MAX),
        "Too many index segments ({count}); increase --chunk-size"
    );
    Ok(count)
}

/// Largest-first scheduling avoids concentrating byte-heavy directories in
/// one segment. Path-order ties make parallel discovery deterministic.
fn balance_chunks<T: Ord>(mut entries: Vec<(u64, T)>, max_files: usize) -> Vec<Vec<T>> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    assert!(max_files > 0);
    let count = entries.len().div_ceil(max_files);
    let mut chunks: Vec<Vec<T>> = (0..count).map(|_| Vec::new()).collect();
    let mut queue: BinaryHeap<_> = (0..count).map(|id| Reverse((0u64, id))).collect();
    entries.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (bytes, entry) in entries {
        let Reverse((total, id)) = queue.pop().expect("remaining chunk capacity");
        chunks[id].push(entry);
        if chunks[id].len() < max_files {
            queue.push(Reverse((total.saturating_add(bytes), id)));
        }
    }
    chunks
}

/// Read from the same handle used for metadata, retaining owned bytes and
/// enforcing the size limit even if the file grows after its metadata check.
fn read_index_source(
    file: impl Read,
    expected_size: u64,
    max_size: u64,
) -> std::io::Result<Option<Vec<u8>>> {
    if expected_size == 0 || expected_size > max_size {
        return Ok(None);
    }
    let capacity = usize::try_from(expected_size).map_err(std::io::Error::other)?;
    let mut content = Vec::new();
    content
        .try_reserve_exact(capacity)
        .map_err(std::io::Error::other)?;
    file.take(max_size.saturating_add(1))
        .read_to_end(&mut content)?;
    Ok((!content.is_empty() && content.len() as u64 <= max_size).then_some(content))
}

/// Retain one owned capture and reject detectable writes during the read.
/// Unix stamps also detect restored mtimes and inode metadata changes. Other
/// platforms retain the existing size/mtime checks; packs remain Unix-only.
fn read_stable_index_source(
    mut file: File,
    before: &fs::Metadata,
    max_size: u64,
) -> std::io::Result<Option<Vec<u8>>> {
    let content = read_index_source(&mut file, before.len(), max_size)?;
    let after = file.metadata()?;
    if before.len() != after.len()
        || before.modified()? != after.modified()?
        || crate::index::source_pack::stamp(before) != crate::index::source_pack::stamp(&after)
        || content
            .as_ref()
            .is_some_and(|bytes| bytes.len() as u64 != before.len())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "Source changed during indexing capture",
        ));
    }
    Ok(content)
}

/// Result of processing a single file (computed in parallel)
#[derive(Clone)]
pub struct ProcessedFile<T = Vec<String>> {
    pub rel_path: PathBuf,
    pub mtime: u64,
    pub size: u64,
    pub language: Language,
    pub flags: DocFlags,
    pub trigrams: Vec<u32>,
    pub tokens: T,
    pub line_offsets: Vec<u32>,
    /// Token positions for positional phrase queries:
    /// (index into `tokens`, word_position)
    pub token_positions: Vec<(u32, u32)>,
}

#[cfg(test)]
fn process_file_content(rel_path: PathBuf, content: &[u8], mtime: u64) -> Option<ProcessedFile> {
    process_file_content_with(
        rel_path,
        content,
        mtime,
        IndexProfile::Full,
        extract_tokens_and_positions,
    )
}

fn process_file_content_with<T>(
    rel_path: PathBuf,
    content: &[u8],
    mtime: u64,
    profile: IndexProfile,
    tokenize: impl FnOnce(&str) -> (T, Vec<(u32, u32)>),
) -> Option<ProcessedFile<T>> {
    // Check if binary
    if is_binary(content) {
        return None;
    }

    let text = std::str::from_utf8(content).ok()?;

    // Detect language from extension
    let ext = rel_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let language = Language::from_extension(ext);

    // Check for minified
    let mut flags = DocFlags::new();
    if is_minified(content) {
        flags.0 |= DocFlags::MINIFIED;
    }

    // Extract trigrams using optimized bitset-based extraction
    let trigrams: Vec<u32> = extract_trigrams(content);

    // Extract tokens and token positions in a single scan of the content
    let (tokens, token_positions) = tokenize(text);

    // Build line map
    let line_offsets = if profile == IndexProfile::Full {
        build_line_map(content)
    } else {
        Vec::new()
    };

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
pub fn build_index_with_chunk_size(
    root_path: &Path,
    force: bool,
    chunk_size: Option<usize>,
) -> Result<()> {
    build_index_with_options(root_path, force, false, chunk_size)
}

/// Build or rebuild the search index with optional silent mode
pub fn build_index_with_progress(root_path: &Path, force: bool, silent: bool) -> Result<()> {
    build_index_with_options(root_path, force, silent, None)
}

/// Build or rebuild the search index with all options
pub fn build_index_with_options(
    root_path: &Path,
    _force: bool,
    silent: bool,
    chunk_size_override: Option<usize>,
) -> Result<()> {
    let root = root_path.canonicalize().context("Invalid path")?;
    let meta_path = get_index_dir(&root)?.join("meta.json");
    let profile = match File::open(meta_path) {
        Ok(file) => match serde_json::from_reader::<_, IndexMeta>(file) {
            Ok(meta) => meta.profile,
            Err(error) => {
                eprintln!("Cannot recover index profile ({error}); rebuilding with full evidence");
                IndexProfile::Full
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => IndexProfile::Full,
        Err(error) => return Err(error.into()),
    };
    build_index_with_profile(root_path, _force, silent, chunk_size_override, profile)
}

pub fn build_index_with_profile(
    root_path: &Path,
    _force: bool,
    silent: bool,
    chunk_size_override: Option<usize>,
    profile: IndexProfile,
) -> Result<()> {
    let root = root_path.canonicalize().context("Invalid path")?;
    anyhow::ensure!(root.to_str().is_some(), "Index roots must be valid UTF-8");

    let config = IndexConfig {
        profile,
        ..IndexConfig::default()
    };
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
        let errors = Arc::new(Mutex::new(Vec::<String>::new()));
        let entries = Arc::new(Mutex::new(Vec::new()));

        struct CollectVisitor {
            root: PathBuf,
            errors: Arc<Mutex<Vec<String>>>,
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
            fn visit(
                &mut self,
                result: Result<ignore::DirEntry, ignore::Error>,
            ) -> ignore::WalkState {
                let entry = match result {
                    Ok(entry) => entry,
                    Err(error) => {
                        self.errors.lock().unwrap().push(error.to_string());
                        return ignore::WalkState::Continue;
                    }
                };
                if
                // file_type() comes from the directory entry — no extra
                // stat(2) per file like path().is_file()
                entry.file_type().is_some_and(|t| t.is_file())
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
            errors: Arc<Mutex<Vec<String>>>,
            shared: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>,
        }

        impl<'s> ignore::ParallelVisitorBuilder<'s> for CollectBuilder {
            fn build(&mut self) -> Box<dyn ignore::ParallelVisitor + 's> {
                Box::new(CollectVisitor {
                    root: self.root.clone(),
                    errors: Arc::clone(&self.errors),
                    shared: Arc::clone(&self.shared),
                    local: Vec::with_capacity(1024),
                })
            }
        }

        let mut builder = CollectBuilder {
            root: root.clone(),
            errors: Arc::clone(&errors),
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
                    ".git"
                        | "node_modules"
                        | "target"
                        | ".codesearch"
                        | "__pycache__"
                        | ".venv"
                        | "venv"
                )
            })
            .build_parallel()
            .visit(&mut builder);

        drop(builder);
        let errors = errors.lock().unwrap();
        if !errors.is_empty() {
            return Err(SourceReadError {
                path: root.to_path_buf(),
                source: std::io::Error::other(errors.join("; ")),
            }
            .into());
        }
        Arc::try_unwrap(entries).unwrap().into_inner().unwrap()
    };

    anyhow::ensure!(
        file_entries.iter().all(|(_, path)| path.to_str().is_some()),
        "Index paths must be valid UTF-8; refusing lossy path storage"
    );
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

    let num_chunks = checked_chunk_count(total_files, chunk_size)?;

    // Metadata weights prevent one fixed-file-count batch from retaining
    // a disproportionately large token/position expansion. Keep the existing
    // number of segments and file-count limit.
    let chunks = if file_entries.len() <= chunk_size {
        if file_entries.is_empty() {
            Vec::new()
        } else {
            vec![file_entries]
        }
    } else {
        let weighted = file_entries
            .into_par_iter()
            .map(|entry| {
                let ext = entry.1.extension().and_then(|s| s.to_str()).unwrap_or("");
                let size = if is_known_binary_ext(ext) {
                    0
                } else {
                    fs::metadata(&entry.0)
                        .ok()
                        .map(|m| m.len())
                        .filter(|&size| size <= max_file_size)
                        .unwrap_or(0)
                };
                (size, entry)
            })
            .collect();
        balance_chunks(weighted, chunk_size)
    };

    // Phase 2: Process in chunks
    let mut chunked_writer = ChunkedIndexWriter::new(&root, config)?;
    let error_count = Arc::new(AtomicUsize::new(0));
    let total_processed = Arc::new(AtomicUsize::new(0));
    // Files rejected after their content was read (binary sniff, no tokens):
    // recorded in meta so incremental scans skip them while unchanged
    let rejected_files = Arc::new(Mutex::new(Vec::<(PathBuf, u64)>::new()));

    if num_chunks > 1 && !silent {
        println!(
            "Processing in {} chunks of up to {} files each",
            num_chunks, chunk_size
        );
    }

    for (chunk_idx, chunk) in chunks.iter().enumerate() {
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
        let rejected_files_clone = rejected_files.clone();

        let source_capture = crate::index::source_pack::CaptureWriter::new(
            &chunked_writer
                .index_path()
                .join("segments")
                .join(format!("seg_{segment_id:04}")),
            crate::index::source_pack::requested(),
        )?;
        // Process chunk files in parallel
        let processed_files: Vec<ProcessedFile<crate::utils::PackedTokens>> = chunk
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

                // Source files are mutable; own their bytes before validating
                // UTF-8 or extracting tokens so external writes cannot change
                // the memory behind those borrows.
                let content = match read_stable_index_source(file, &metadata, max_file_size) {
                    Ok(Some(content)) => content,
                    Ok(None) => {
                        if let Some(ref pb) = pb_clone {
                            pb.inc(1);
                        }
                        return None;
                    }
                    Err(_) => {
                        error_count_clone.fetch_add(1, Ordering::Relaxed);
                        if let Some(ref pb) = pb_clone {
                            pb.inc(1);
                        }
                        return None;
                    }
                };

                // Get modification time
                let mtime = metadata
                    .modified()
                    .map(|t| {
                        t.duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                            .min(u64::MAX as u128) as u64
                    })
                    .unwrap_or(0);

                // Process file content (trigrams, tokens, line map)
                let result =
                    process_file_content_with(rel_path.clone(), &content, mtime, profile, |text| {
                        if profile == IndexProfile::Full {
                            crate::utils::extract_packed_tokens_and_positions(text)
                        } else {
                            (crate::utils::PackedTokens::default(), Vec::new())
                        }
                    });

                if result.is_some()
                    && let Some(capture) = &source_capture
                    && let Err(error) = capture.capture(
                        rel_path,
                        std::str::from_utf8(&content).expect("processed UTF-8"),
                        &metadata,
                    )
                {
                    eprintln!("Cannot capture {}: {error:#}", rel_path.display());
                    error_count_clone.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                if result.is_some() {
                    total_processed_clone.fetch_add(1, Ordering::Relaxed);
                } else {
                    // Content was read but rejected (binary sniff, no tokens):
                    // remember it so change scans skip it while unchanged
                    rejected_files_clone
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((rel_path.clone(), mtime));
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
        chunked_writer.write_packed_chunk(segment_id, processed_files, source_capture)?;

        // Memory freed here - processed_files dropped
    }

    if error_count.load(Ordering::Relaxed) > 0 {
        return Err(SourceReadError {
            path: root.clone(),
            source: std::io::Error::other(format!(
                "{} source files could not be read or captured; previous index retained",
                error_count.load(Ordering::Relaxed)
            )),
        }
        .into());
    }

    let rejected = std::mem::take(
        &mut *rejected_files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    chunked_writer.set_rejected_files(rejected);

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
        println!("Index stored at: {}", get_index_dir(&root)?.display());
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
    /// Previously rejected files still present with unchanged mtime
    /// (carried forward into the updated rejected list)
    rejected_unchanged: Vec<(PathBuf, u64)>,
    /// Total files currently in index
    indexed_count: usize,
}

/// Incrementally update the index (smart mode)
/// Returns Ok(true) if incremental update was performed, Ok(false) if full rebuild was needed
pub fn update_index(root_path: &Path) -> Result<bool> {
    Ok(!matches!(
        reconcile_index(root_path, None, INCREMENTAL_THRESHOLD_PERCENT)?,
        UpdateOutcome::Rebuilt
    ))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UpdateOutcome {
    Unchanged(PathBuf),
    Incremental,
    Rebuilt,
    /// Query-visible memory snapshot; the durable generation is unchanged.
    Visible,
}

/// A watcher can reuse its immutable reader only while CURRENT still names
/// that generation. A force-build by another process must invalidate reuse.
pub(crate) fn reconcile_index(
    root_path: &Path,
    cached: Option<&IndexReader>,
    rebuild_threshold_percent: usize,
) -> Result<UpdateOutcome> {
    reconcile_index_with_paths(
        root_path,
        cached,
        rebuild_threshold_percent,
        None,
        None,
        false,
    )
}

/// Reconcile precise file notifications without scanning unrelated subtrees.
/// Paths are relative to the root. Directory changes, ignore-control files and
/// an externally replaced generation require the ordinary complete scan.
/// Native notifications are evidence of a possible content change even when
/// an editor preserves the file's mtime and size.
#[cfg(test)]
pub(crate) fn reconcile_index_paths(
    root_path: &Path,
    cached: Option<&IndexReader>,
    rebuild_threshold_percent: usize,
    paths: &[PathBuf],
) -> Result<UpdateOutcome> {
    reconcile_index_with_paths(
        root_path,
        cached,
        rebuild_threshold_percent,
        Some(paths),
        None,
        false,
    )
}

/// Publish a bounded, fully indexed memory snapshot before durable generation
/// construction. The callback must not mistake this snapshot for a disk commit.
pub(crate) fn reconcile_index_paths_with_visibility(
    root_path: &Path,
    cached: Option<&IndexReader>,
    rebuild_threshold_percent: usize,
    paths: &[PathBuf],
    visible: &mut dyn FnMut(IndexReader),
    preview_only: bool,
) -> Result<UpdateOutcome> {
    reconcile_index_with_paths(
        root_path,
        cached,
        rebuild_threshold_percent,
        Some(paths),
        Some(visible),
        preview_only,
    )
}

fn reconcile_index_with_paths(
    root_path: &Path,
    cached: Option<&IndexReader>,
    rebuild_threshold_percent: usize,
    paths: Option<&[PathBuf]>,
    visible: Option<&mut dyn FnMut(IndexReader)>,
    preview_only: bool,
) -> Result<UpdateOutcome> {
    let trace = std::env::var_os("FXI_TRACE_UPDATES").is_some_and(|value| value == "1");
    let started = std::time::Instant::now();
    let root = root_path.canonicalize().context("Invalid path")?;
    anyhow::ensure!(root.to_str().is_some(), "Index roots must be valid UTF-8");
    let index_path = get_index_dir(&root)?;

    // If no index exists, do full build
    if !index_path.exists() {
        println!("No existing index found, performing full build...");
        build_index(&root, false)?;
        return Ok(UpdateOutcome::Rebuilt);
    }

    let reusable = cached
        .filter(|reader| reader.root_path() == root && reader.generation_path() == index_path);
    let opened;
    let reader = if let Some(reader) = reusable {
        reader
    } else {
        opened = IndexReader::open(&root)?;
        &opened
    };
    let meta = &reader.meta;

    let hinted: Option<HashSet<PathBuf>> = paths.map(|paths| paths.iter().cloned().collect());
    let candidate_scope = hinted.as_ref().filter(|_| reusable.is_some());
    let indexed_count = reader.valid_doc_ids().len() as usize;
    // Watched readers prime their shared path lookup outside the event path.
    // Only the hinted documents need metadata on ordinary file saves.
    let mut indexed_files: HashMap<PathBuf, (u32, u64, u64)> =
        HashMap::with_capacity(candidate_scope.map_or(indexed_count, HashSet::len));
    if let Some(paths) = candidate_scope {
        for path in paths {
            if let Some(doc) = reader.document_for_path(path) {
                indexed_files.insert(path.clone(), (doc.doc_id, doc.mtime, doc.size));
            }
        }
    } else {
        for doc_id in reader.valid_doc_ids().iter() {
            if let Some(doc) = reader.get_document(doc_id)
                && let Some(path) = reader.get_path(doc)
            {
                indexed_files.insert(path.clone(), (doc_id, doc.mtime, doc.size));
            }
        }
    }

    // Previously rejected files (binary sniff etc.) with their mtimes.
    let rejected: HashMap<PathBuf, u64> = meta.rejected_files.iter().cloned().collect();
    let scoped = if let Some(paths) = candidate_scope {
        file_hints_can_be_scoped(&root, paths, &indexed_files, reader, &rejected)?
    } else {
        false
    };
    if candidate_scope.is_some() && !scoped {
        indexed_files.reserve(indexed_count.saturating_sub(indexed_files.len()));
        for doc_id in reader.valid_doc_ids().iter() {
            if let Some(doc) = reader.get_document(doc_id)
                && let Some(path) = reader.get_path(doc)
            {
                indexed_files.insert(path.clone(), (doc_id, doc.mtime, doc.size));
            }
        }
    }

    // A scoped scan starts at the same root and loads the same ignore rules as
    // a complete scan; its filter only prunes branches unrelated to the hints.
    // Starting a walker at an individual file would bypass ignore filtering.
    let diff_started = std::time::Instant::now();
    let diff = compute_index_diff(
        &root,
        &indexed_files,
        indexed_count,
        &rejected,
        hinted.as_ref().filter(|_| scoped),
        hinted.as_ref(),
    )?;
    if trace {
        eprintln!(
            "fxid: update timing {}: prepare={:.3}ms diff={:.3}ms scoped={}",
            root.display(),
            diff_started.duration_since(started).as_secs_f64() * 1000.0,
            diff_started.elapsed().as_secs_f64() * 1000.0,
            scoped,
        );
    }

    let total_changes = diff.new_files.len() + diff.modified_files.len() + diff.deleted_files.len();

    if total_changes == 0 {
        println!("Index is up to date, no changes detected.");
        return Ok(UpdateOutcome::Unchanged(
            reader.generation_path().to_path_buf(),
        ));
    }

    // Calculate change percentage
    let change_percent = (total_changes * 100)
        .checked_div(diff.indexed_count)
        .unwrap_or(100);

    println!(
        "Detected {} changes: {} new, {} modified, {} deleted ({:.1}% of index)",
        total_changes,
        diff.new_files.len(),
        diff.modified_files.len(),
        diff.deleted_files.len(),
        change_percent as f64
    );

    // If too many changes, do full rebuild
    if change_percent > rebuild_threshold_percent {
        println!(
            "Change threshold exceeded (>{}%), performing full rebuild...",
            rebuild_threshold_percent
        );
        build_index(&root, true)?;
        return Ok(UpdateOutcome::Rebuilt);
    }

    // Perform incremental update
    println!("Performing incremental update...");
    let publication_started = std::time::Instant::now();
    if perform_incremental_update_visible(&root, meta, diff, Some(reader), visible, preview_only)? {
        return Ok(UpdateOutcome::Visible);
    }
    if trace {
        eprintln!(
            "fxid: update timing {}: publication={:.3}ms",
            root.display(),
            publication_started.elapsed().as_secs_f64() * 1000.0,
        );
    }

    Ok(UpdateOutcome::Incremental)
}

type ScannedFile = (PathBuf, PathBuf, u64, u64);

fn file_hints_can_be_scoped(
    root: &Path,
    paths: &HashSet<PathBuf>,
    indexed_files: &HashMap<PathBuf, (u32, u64, u64)>,
    reader: &IndexReader,
    rejected: &HashMap<PathBuf, u64>,
) -> Result<bool> {
    let has_descendants = |path: &Path| {
        reader
            .valid_doc_ids()
            .iter()
            .filter_map(|id| reader.get_document(id))
            .filter_map(|doc| reader.get_path(doc).map(PathBuf::as_path))
            .chain(rejected.keys().map(PathBuf::as_path))
            .any(|existing| existing != path && existing.starts_with(path))
    };
    let mut checked_ancestors = HashSet::new();
    for path in paths {
        if path.as_os_str().is_empty()
            || path.components().any(|component| {
                !matches!(component, Component::Normal(_))
                    || matches!(
                        component.as_os_str().to_str(),
                        Some(".git" | ".gitignore" | ".ignore")
                    )
            })
        {
            return Ok(false);
        }
        // A child notification may be the only evidence of its directory
        // being deleted or replaced by a symlink. Scope cannot leave the
        // other indexed children behind. Check shared ancestors only once.
        for ancestor in path
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
        {
            if !checked_ancestors.insert(ancestor.to_path_buf()) {
                continue;
            }
            let remains_directory = match fs::symlink_metadata(root.join(ancestor)) {
                Ok(metadata) => metadata.is_dir(),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    false
                }
                Err(error) => return Err(error).context("Cannot inspect watcher ancestor"),
            };
            if !remains_directory && has_descendants(ancestor) {
                return Ok(false);
            }
        }
        // A directory may have vanished, become a symlink, or even become a
        // regular file. Unknown missing editor temporary paths have no indexed
        // descendants and safely remain no-ops on the scoped path.
        if !indexed_files.contains_key(path)
            && !rejected.contains_key(path)
            && has_descendants(path)
        {
            return Ok(false);
        }
        match fs::symlink_metadata(root.join(path)) {
            Ok(metadata) if metadata.is_dir() => return Ok(false),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error).context("Cannot inspect watcher path"),
        }
    }
    Ok(true)
}

/// Compute the difference between indexed files and filesystem. A scope limits
/// membership changes to exact hinted paths; forced paths are re-read even when
/// timestamps are unchanged, including when a hint requires a full scan.
fn compute_index_diff(
    root: &Path,
    indexed_files: &HashMap<PathBuf, (u32, u64, u64)>,
    indexed_count: usize,
    rejected: &HashMap<PathBuf, u64>,
    scope: Option<&HashSet<PathBuf>>,
    forced: Option<&HashSet<PathBuf>>,
) -> Result<IndexDiff> {
    let config = IndexConfig::default();
    let max_file_size = config.max_file_size;

    // Complete scans parallelize metadata reads across the tree. A precise
    // scope uses the serial walker to avoid starting workers for a few files.
    let scanned: Vec<ScannedFile> = {
        let errors = Arc::new(Mutex::new(Vec::<String>::new()));
        let entries: Arc<Mutex<Vec<ScannedFile>>> = Arc::new(Mutex::new(Vec::with_capacity(
            scope.map_or(indexed_files.len(), HashSet::len),
        )));

        struct ScanVisitor {
            root: PathBuf,
            errors: Arc<Mutex<Vec<String>>>,
            max_file_size: u64,
            shared: Arc<Mutex<Vec<ScannedFile>>>,
            // Batch into a thread-local vec; take the shared lock once per
            // walker thread instead of once per file
            local: Vec<ScannedFile>,
        }

        impl ScanVisitor {
            fn metadata(&self, entry: &ignore::DirEntry) -> Option<std::fs::Metadata> {
                match entry.metadata() {
                    Ok(meta) => Some(meta),
                    Err(error) => {
                        self.errors.lock().unwrap().push(error.to_string());
                        None
                    }
                }
            }

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

        impl ignore::ParallelVisitor for ScanVisitor {
            fn visit(
                &mut self,
                result: Result<ignore::DirEntry, ignore::Error>,
            ) -> ignore::WalkState {
                let entry = match result {
                    Ok(entry) => entry,
                    Err(error) => {
                        self.errors.lock().unwrap().push(error.to_string());
                        return ignore::WalkState::Continue;
                    }
                };
                if
                // file_type() comes from the directory entry — no extra
                // stat(2) per file like path().is_file()
                entry.file_type().is_some_and(|t| t.is_file())
                    && let Ok(rel_path) = entry.path().strip_prefix(&self.root)
                    // Known-binary extensions are never indexed; skipping them
                    // here keeps them from showing up as eternally-"new" files
                    // that every incremental update re-reads and rejects
                    && !rel_path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(is_known_binary_ext)
                    && let Some(metadata) = self.metadata(&entry)
                    && metadata.len() <= self.max_file_size
                    && metadata.len() > 0
                {
                    let mtime = metadata
                        .modified()
                        .map(|t| {
                            t.duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos()
                                .min(u64::MAX as u128) as u64
                        })
                        .unwrap_or(0);
                    let rel_path = rel_path.to_path_buf();
                    self.local
                        .push((entry.into_path(), rel_path, mtime, metadata.len()));
                }
                ignore::WalkState::Continue
            }
        }

        impl Drop for ScanVisitor {
            fn drop(&mut self) {
                self.flush();
            }
        }

        struct ScanBuilder {
            root: PathBuf,
            errors: Arc<Mutex<Vec<String>>>,
            max_file_size: u64,
            shared: Arc<Mutex<Vec<ScannedFile>>>,
        }

        impl<'s> ignore::ParallelVisitorBuilder<'s> for ScanBuilder {
            fn build(&mut self) -> Box<dyn ignore::ParallelVisitor + 's> {
                Box::new(ScanVisitor {
                    root: self.root.clone(),
                    errors: Arc::clone(&self.errors),
                    max_file_size: self.max_file_size,
                    shared: Arc::clone(&self.shared),
                    local: Vec::with_capacity(1024),
                })
            }
        }

        let mut builder = ScanBuilder {
            root: root.to_path_buf(),
            errors: Arc::clone(&errors),
            max_file_size,
            shared: Arc::clone(&entries),
        };

        // Include every ancestor so the walker constructs the normal nested
        // ignore state, but do not descend into unrelated directories.
        let relevant: Option<HashSet<PathBuf>> = scope.map(|paths| {
            paths
                .iter()
                .flat_map(|path| path.ancestors().map(Path::to_path_buf))
                .collect()
        });
        let filter_scope = scope.cloned();
        let filter_root = root.to_path_buf();
        let mut walk = WalkBuilder::new(root);
        walk.hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .filter_entry(move |entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    ".git"
                        | "node_modules"
                        | "target"
                        | ".codesearch"
                        | "__pycache__"
                        | ".venv"
                        | "venv"
                ) && relevant.as_ref().is_none_or(|paths| {
                    entry.path().strip_prefix(&filter_root).is_ok_and(|path| {
                        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                            paths.contains(path)
                        } else {
                            filter_scope
                                .as_ref()
                                .is_some_and(|scope| scope.contains(path))
                        }
                    })
                })
            });
        if let Some(scope) = scope {
            let mut visitor = ScanVisitor {
                root: root.to_path_buf(),
                errors: Arc::clone(&errors),
                max_file_size,
                shared: Arc::clone(&entries),
                local: Vec::with_capacity(scope.len()),
            };
            for entry in walk.build() {
                ignore::ParallelVisitor::visit(&mut visitor, entry);
            }
        } else {
            walk.build_parallel().visit(&mut builder);
        }

        drop(builder);
        let errors = errors.lock().unwrap();
        if !errors.is_empty() {
            return Err(SourceReadError {
                path: root.to_path_buf(),
                source: std::io::Error::other(errors.join("; ")),
            }
            .into());
        }
        Arc::try_unwrap(entries).unwrap().into_inner().unwrap()
    };

    let mut new_files = Vec::new();
    let mut modified_files = Vec::new();
    let mut rejected_unchanged: Vec<_> = rejected
        .iter()
        .filter(|(path, _)| scope.is_some_and(|paths| !paths.contains(*path)))
        .map(|(path, mtime)| (path.clone(), *mtime))
        .collect();
    let mut seen_paths: std::collections::HashSet<&Path> =
        std::collections::HashSet::with_capacity(scanned.len());

    for (full_path, rel_path, current_mtime, current_size) in &scanned {
        seen_paths.insert(rel_path.as_path());

        if let Some(&(doc_id, indexed_mtime, indexed_size)) = indexed_files.get(rel_path) {
            // File exists in index - check if modified
            if *current_mtime != indexed_mtime
                || *current_size != indexed_size
                || forced.is_some_and(|paths| paths.contains(rel_path))
            {
                modified_files.push((full_path.clone(), rel_path.clone(), doc_id));
            }
        } else if rejected.get(rel_path) == Some(current_mtime)
            && !forced.is_some_and(|paths| paths.contains(rel_path))
        {
            // Previously rejected (binary sniff etc.) and unchanged since:
            // skip instead of re-reading and re-rejecting it
            rejected_unchanged.push((rel_path.clone(), *current_mtime));
        } else {
            // New file
            new_files.push((full_path.clone(), rel_path.clone()));
        }
    }

    // Find deleted files
    let deleted_files: Vec<PathBuf> = indexed_files
        .keys()
        .filter(|path| {
            scope.is_none_or(|paths| paths.contains(*path)) && !seen_paths.contains(path.as_path())
        })
        .cloned()
        .collect();

    Ok(IndexDiff {
        new_files,
        modified_files,
        deleted_files,
        rejected_unchanged,
        indexed_count,
    })
}

/// Read and process a single file for an incremental update.
/// Returns None for binary, empty, oversized or unreadable files.
/// A transient source failure must never become a cached eligibility rejection.
#[derive(Debug)]
pub(crate) struct SourceReadError {
    path: PathBuf,
    source: std::io::Error,
}
impl std::fmt::Display for SourceReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Cannot read source {}: {}",
            self.path.display(),
            self.source
        )
    }
}
impl std::error::Error for SourceReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn process_file_for_update(
    full_path: &Path,
    rel_path: &Path,
    max_file_size: u64,
    profile: IndexProfile,
    capture: Option<&crate::index::source_pack::CaptureWriter>,
) -> Result<Option<ProcessedFile>> {
    anyhow::ensure!(
        rel_path.to_str().is_some(),
        "Index paths must be valid UTF-8"
    );
    let read_error = |source| SourceReadError {
        path: full_path.to_path_buf(),
        source,
    };
    let ext = rel_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if is_known_binary_ext(ext) {
        return Ok(None);
    }
    let file = File::open(full_path).map_err(read_error)?;
    let metadata = file.metadata().map_err(read_error)?;
    if metadata.len() == 0 || metadata.len() > max_file_size {
        return Ok(None);
    }
    let mtime = metadata
        .modified()
        .map_err(read_error)?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    let content = read_stable_index_source(file, &metadata, max_file_size).map_err(read_error)?;
    let Some(content) = content else {
        return Ok(None);
    };
    let processed = {
        process_file_content_with(rel_path.to_path_buf(), &content, mtime, profile, |text| {
            if profile == IndexProfile::Full {
                extract_tokens_and_positions(text)
            } else {
                (Vec::new(), Vec::new())
            }
        })
    };
    if processed.is_some()
        && let Some(capture) = capture
    {
        capture.capture(
            rel_path,
            std::str::from_utf8(&content).expect("processed UTF-8"),
            &metadata,
        )?;
    }
    Ok(processed)
}

/// Perform incremental update by writing the diff as a delta segment:
/// deleted and modified files are tombstoned, new and modified files are
/// indexed into a new segment, and the index metadata is committed
/// atomically. This is the same mechanism the daemon's file watcher uses.
#[cfg(test)]
fn perform_incremental_update(root: &Path, meta: &IndexMeta, diff: IndexDiff) -> Result<()> {
    perform_incremental_update_visible(root, meta, diff, None, None, false).map(|_| ())
}

fn perform_incremental_update_visible(
    root: &Path,
    meta: &IndexMeta,
    diff: IndexDiff,
    base: Option<&IndexReader>,
    visible: Option<&mut dyn FnMut(IndexReader)>,
    preview_only: bool,
) -> Result<bool> {
    use crate::index::writer::DeltaSegmentWriter;

    let config = IndexConfig::default();
    let mut meta = meta.clone();
    let old_rejected_files = meta.rejected_files.clone();

    // Next segment id after base + existing deltas
    let next_segment_id = meta
        .base_segment
        .into_iter()
        .chain(meta.delta_segments.iter().copied())
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .context(
            "Segment ID capacity exhausted; compact the index or rebuild with a larger chunk size",
        )?;

    // Index new and modified files (in parallel - extraction is the
    // expensive part; add_file itself is cheap)
    let to_index: Vec<(&PathBuf, &PathBuf)> = diff
        .new_files
        .iter()
        .map(|(full, rel)| (full, rel))
        .chain(diff.modified_files.iter().map(|(full, rel, _)| (full, rel)))
        .collect();

    let source_capture = crate::index::source_pack::CaptureWriter::temporary(
        root,
        crate::index::source_pack::requested()
            || crate::index::source_pack::present(&get_index_dir(root)?),
    )?;
    let outcomes: Vec<Result<ProcessedFile, (PathBuf, u64)>> = to_index
        .par_iter()
        .map(|(full, rel)| {
            Ok(
                match process_file_for_update(
                    full,
                    rel,
                    config.max_file_size,
                    meta.profile,
                    source_capture.as_ref(),
                )? {
                    Some(p) => Ok(p),
                    None => {
                        // Rejected (binary sniff etc.): remember it with its
                        // current mtime so future scans skip it while unchanged
                        let mtime = fs::metadata(full)
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .map(|t| {
                                t.duration_since(UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_nanos()
                                    .min(u64::MAX as u128) as u64
                            })
                            .unwrap_or(0);
                        Err(((*rel).clone(), mtime))
                    }
                },
            )
        })
        .collect::<Result<Vec<_>>>()?;

    // Keep the extra memory bounded. Large updates retain the durable path;
    // small saves reuse exactly the bytes/tokens already extracted for it.
    if let (Some(base), Some(visible)) = (base, visible)
        && to_index.len() + diff.deleted_files.len() <= 256
        && outcomes
            .iter()
            .filter_map(|outcome| outcome.as_ref().ok())
            .map(|file| file.size)
            .sum::<u64>()
            <= 8 * 1024 * 1024
    {
        let files = outcomes
            .iter()
            .filter_map(|outcome| outcome.as_ref().ok())
            .cloned()
            .collect();
        let removed: Vec<_> = diff
            .deleted_files
            .iter()
            .chain(diff.modified_files.iter().map(|(_, path, _)| path))
            .cloned()
            .collect();
        match base.with_memory_delta(files, &removed) {
            Ok(reader) => {
                visible(reader);
                if preview_only {
                    return Ok(true);
                }
            }
            // Visibility acceleration is optional; a capacity limit must not
            // prevent the durable update or its normal recovery.
            Err(error) => {
                eprintln!("fxid: memory update unavailable; persisting normally: {error:#}")
            }
        }
    }

    let mut writer = DeltaSegmentWriter::new(root, next_segment_id)?;
    for rel_path in &diff.deleted_files {
        writer.mark_tombstone(rel_path);
    }
    for (_, rel_path, _) in &diff.modified_files {
        writer.mark_tombstone(rel_path);
    }

    let mut added_count = 0;
    let mut rejected_files = diff.rejected_unchanged.clone();
    for outcome in outcomes {
        match outcome {
            Ok(file) => {
                added_count += 1;
                writer.add_file(file)?;
            }
            Err(rejection) => rejected_files.push(rejection),
        }
    }
    rejected_files.sort();
    meta.rejected_files = rejected_files;

    if !writer.has_changes() {
        // Nothing indexable, but the rejected-file list may have grown (e.g.
        // newly seen binaries): persist it so the next scan skips them
        if meta.rejected_files != old_rejected_files {
            writer.finalize_with_capture(&mut meta, source_capture)?;
        }
        println!("No indexable changes to apply.");
        return Ok(false);
    }

    // Commits segment -> docs.bin -> paths.bin -> meta.json atomically
    writer.finalize_with_capture(&mut meta, source_capture)?;
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
    let new_deltas = meta
        .delta_segments
        .len()
        .saturating_sub(meta.delta_baseline);
    if tombstone_ratio > 0.15
        || new_deltas >= crate::server::watcher::DEFAULT_MERGE_SEGMENT_THRESHOLD
    {
        println!(
            "Index is fragmented ({} delta segments, {} tombstones), compacting...",
            new_deltas, meta.tombstone_count
        );
        crate::index::compact::merge_segments(root)?;
    }

    Ok(false)
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

#[cfg(test)]
mod encoding_tests {
    use super::*;

    #[test]
    fn stable_capture_rejects_changes_since_metadata_observation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a.txt");
        fs::write(&path, "alpha needle\n").unwrap();
        let file = File::open(&path).unwrap();
        let before = file.metadata().unwrap();
        fs::write(&path, "omega marker\n").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(before.modified().unwrap() + std::time::Duration::from_secs(2)),
            )
            .unwrap();
        #[cfg(unix)]
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        let error = read_stable_index_source(file, &before, 1024).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    }

    #[cfg(unix)]
    #[test]
    fn postings_and_packs_keep_one_capture_through_publication_and_compaction() {
        use crate::index::source_pack::{CaptureWriter, SourcePack};
        for compressed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            fs::create_dir(root.join(".git")).unwrap();
            let path = root.join("a.txt");
            fs::write(&path, "alpha needle\n").unwrap();
            let mut writer = ChunkedIndexWriter::new(
                &root,
                IndexConfig {
                    profile: IndexProfile::Lean,
                    ..IndexConfig::default()
                },
            )
            .unwrap();
            let directory = writer.index_path().join("segments/seg_0001");
            let capture = CaptureWriter::with_compression(&directory, true, compressed)
                .unwrap()
                .unwrap();
            let processed = process_file_for_update(
                &path,
                Path::new("a.txt"),
                1024,
                IndexProfile::Lean,
                Some(&capture),
            )
            .unwrap()
            .unwrap();
            // Same-size replacement between extraction and publication must not
            // become the pack associated with the earlier postings.
            fs::write(&path, "omega marker\n").unwrap();
            let processed = ProcessedFile {
                rel_path: processed.rel_path,
                mtime: processed.mtime,
                size: processed.size,
                language: processed.language,
                flags: processed.flags,
                trigrams: processed.trigrams,
                tokens: processed.tokens.into(),
                line_offsets: processed.line_offsets,
                token_positions: processed.token_positions,
            };
            writer
                .write_packed_chunk(1, vec![processed], Some(capture))
                .unwrap();
            writer.finalize().unwrap();
            let reader = IndexReader::open(&root).unwrap();
            let pack = SourcePack::open(&directory).unwrap();
            assert_eq!(
                pack.captured(1, Path::new("a.txt"), 13).unwrap().0,
                "alpha needle\n"
            );
            assert!(pack.read(1, Path::new("a.txt"), &path).is_none());
            assert!(
                reader
                    .get_trigram_docs(crate::index::types::bytes_to_trigram(b'a', b'l', b'p'))
                    .unwrap()
                    .contains(1)
            );
            assert!(
                reader
                    .get_trigram_docs(crate::index::types::bytes_to_trigram(b'o', b'm', b'e'))
                    .unwrap()
                    .is_empty()
            );
            drop((pack, reader, writer));

            // A captured delta also must not reopen its file at finalization.
            let base = IndexReader::open(&root).unwrap();
            let mut meta = base.meta.clone();
            let mut delta = crate::index::writer::DeltaSegmentWriter::new(&root, 2).unwrap();
            let staged = crate::index::generation::Generation::new(&root).unwrap();
            let capture =
                CaptureWriter::with_compression(&staged.path.join("capture"), true, compressed)
                    .unwrap()
                    .unwrap();
            let processed = process_file_for_update(
                &path,
                Path::new("a.txt"),
                1024,
                IndexProfile::Lean,
                Some(&capture),
            )
            .unwrap()
            .unwrap();
            fs::write(&path, "sigma changed\n").unwrap();
            delta.mark_tombstone(Path::new("a.txt"));
            delta.add_file(processed).unwrap();
            delta
                .finalize_with_capture(&mut meta, Some(capture))
                .unwrap();
            drop((staged, base));
            let generation = get_index_dir(&root).unwrap();
            let pack = SourcePack::open(&generation.join("segments/seg_0002")).unwrap();
            assert_eq!(
                pack.captured(2, Path::new("a.txt"), 13).unwrap().0,
                "omega marker\n"
            );
            assert!(pack.read(2, Path::new("a.txt"), &path).is_none());
            drop(pack);
            crate::index::compact::merge_segments(&root).unwrap();
            let generation = get_index_dir(&root).unwrap();
            let pack = SourcePack::open(&generation.join("segments/seg_0001")).unwrap();
            assert_eq!(
                pack.captured(1, Path::new("a.txt"), 13).unwrap().0,
                "omega marker\n"
            );
            assert!(pack.read(1, Path::new("a.txt"), &path).is_none());
            drop(pack);
            crate::utils::remove_index(&root).unwrap();
        }
    }

    #[test]
    fn lean_preview_preserves_capabilities_and_searches_new_content() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for id in 0..10 {
            fs::write(root.join(format!("{id}.txt")), "original content\n").unwrap();
        }
        build_index_with_profile(&root, true, true, None, IndexProfile::Lean).unwrap();
        let base = IndexReader::open(&root).unwrap();
        fs::write(root.join("0.txt"), "new preview needle\n").unwrap();
        let mut seen = false;
        reconcile_index_paths_with_visibility(
            &root,
            Some(&base),
            100,
            &["0.txt".into()],
            &mut |preview| {
                seen = true;
                assert_eq!(preview.meta.profile, IndexProfile::Lean);
                assert!(preview.get_token_docs("needle").is_err());
                assert_eq!(preview.get_line_map(1).unwrap(), None);
                let files = crate::query::QueryExecutor::new(&preview)
                    .execute_files_only(&crate::query::parse_query("needle"), 0)
                    .unwrap();
                assert_eq!(files, vec![PathBuf::from("0.txt")]);
            },
            true,
        )
        .unwrap();
        assert!(seen);
        assert_eq!(
            IndexReader::open(&root).unwrap().generation_path(),
            base.generation_path()
        );
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn segment_counts_cannot_wrap_the_disk_identifier() {
        let max = usize::from(SegmentId::MAX);
        assert_eq!(checked_chunk_count(max, 1).unwrap(), max);
        assert!(checked_chunk_count(max + 1, 1).is_err());
        assert!(checked_chunk_count(1, 0).is_err());
        assert_eq!(checked_chunk_count(usize::MAX, usize::MAX).unwrap(), 1);
        assert_eq!(checked_chunk_count(0, 2000).unwrap(), 0);
    }

    #[test]
    fn source_reads_enforce_growth_and_empty_file_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.txt");
        for (content, expected, limit, accepted) in [
            ("12345678", 8, 8, true),
            ("123456789", 2, 8, false),
            ("", 2, 8, false),
            ("short", 8, 8, true),
            ("short", 5, 4, false),
        ] {
            fs::write(&path, content).unwrap();
            let result = read_index_source(File::open(&path).unwrap(), expected, limit).unwrap();
            assert_eq!(result.is_some(), accepted);
            if let Some(bytes) = result {
                assert_eq!(bytes, content.as_bytes());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn source_reads_do_not_reopen_replaced_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.txt");
        fs::write(&path, "original").unwrap();
        let file = File::open(&path).unwrap();
        let size = file.metadata().unwrap().len();
        fs::rename(&path, dir.path().join("moved.txt")).unwrap();
        fs::write(&path, "replacement").unwrap();
        assert_eq!(
            read_index_source(file, size, 100).unwrap().unwrap(),
            b"original"
        );
    }

    #[test]
    fn exhausted_delta_ids_preserve_the_published_generation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join("base.txt"), "needle\n").unwrap();
        let mut writer =
            ChunkedIndexWriter::new(&root, crate::index::types::IndexConfig::default()).unwrap();
        let file = process_file_content(PathBuf::from("base.txt"), b"needle\n", 0).unwrap();
        writer.write_chunk(SegmentId::MAX, vec![file]).unwrap();
        writer.finalize().unwrap();
        drop(writer);
        let published = get_index_dir(&root).unwrap();
        let before = fs::read(published.join("meta.json")).unwrap();
        let meta: IndexMeta = serde_json::from_slice(&before).unwrap();
        assert_eq!(meta.base_segment, Some(SegmentId::MAX));
        fs::write(root.join("new.txt"), "new needle\n").unwrap();
        let diff = IndexDiff {
            new_files: vec![(root.join("new.txt"), PathBuf::from("new.txt"))],
            modified_files: vec![],
            deleted_files: vec![],
            rejected_unchanged: vec![],
            indexed_count: 1,
        };
        let error = perform_incremental_update(&root, &meta, diff).unwrap_err();
        assert!(error.to_string().contains("Segment ID capacity exhausted"));
        assert_eq!(get_index_dir(&root).unwrap(), published);
        assert_eq!(fs::read(published.join("meta.json")).unwrap(), before);
        let reader = IndexReader::open(&root).unwrap();
        assert_eq!(reader.valid_doc_ids().len(), 1);
        let query = crate::query::parse_query("needle");
        assert_eq!(
            crate::query::QueryExecutor::new(&reader)
                .execute_files_only(&query, 0)
                .unwrap(),
            vec![PathBuf::from("base.txt")]
        );
        drop(reader);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn balanced_chunks_preserve_files_limits_and_determinism() {
        for count in [0usize, 1, 7, 100, 4097] {
            for limit in [1, 7, 2000] {
                let entries: Vec<_> = (0..count).map(|id| ((id % 17) as u64, id)).collect();
                let chunks = balance_chunks(entries.clone(), limit);
                assert_eq!(chunks.len(), count.div_ceil(limit));
                assert!(chunks.iter().all(|c| !c.is_empty() && c.len() <= limit));
                let mut actual: Vec<_> = chunks.iter().flatten().copied().collect();
                actual.sort_unstable();
                assert_eq!(actual, (0..count).collect::<Vec<_>>());
                assert_eq!(
                    chunks,
                    balance_chunks(entries.into_iter().rev().collect(), limit)
                );
            }
        }
        let sizes: Vec<u64> = (0..6000).map(|i| if i < 2000 { 1000 } else { 1 }).collect();
        let chunks = balance_chunks(
            sizes
                .iter()
                .enumerate()
                .map(|(id, &size)| (size, id))
                .collect(),
            2000,
        );
        let loads: Vec<u64> = chunks
            .iter()
            .map(|c| c.iter().map(|&id| sizes[id]).sum())
            .collect();
        assert!(loads.iter().max().unwrap() - loads.iter().min().unwrap() <= 1000);
    }

    #[test]
    fn index_eligibility_matches_utf8_verification() {
        assert!(process_file_content("a.txt".into(), b"needle \xff", 0).is_none());
        assert!(process_file_content("a.txt".into(), "needle K Σ".as_bytes(), 0).is_some());
    }
}

#[cfg(test)]
mod scoped_reconciliation_tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..12 {
            fs::write(
                temp.path().join(format!("base{index}.rs")),
                "original marker\n",
            )
            .unwrap();
        }
        temp
    }

    fn paths(reader: &IndexReader) -> Vec<PathBuf> {
        let mut paths: Vec<_> = reader
            .valid_doc_ids()
            .iter()
            .map(|id| {
                reader
                    .get_path(reader.get_document(id).unwrap())
                    .unwrap()
                    .clone()
            })
            .collect();
        paths.sort();
        paths
    }

    fn matches(reader: &IndexReader, text: &str) -> Vec<PathBuf> {
        crate::query::QueryExecutor::new(reader)
            .execute_files_only(&crate::query::parse_query(text), 0)
            .unwrap()
    }

    #[test]
    fn transient_source_failure_preserves_generation_and_can_retry() {
        let temp = fixture();
        let root = temp.path().canonicalize().unwrap();
        build_index(&root, true).unwrap();
        let generation = get_index_dir(&root).unwrap();
        let reader = IndexReader::open(&root).unwrap();
        // Inject a disappeared source after discovery: deterministic on every OS.
        let path = root.join("new.rs");
        let diff = IndexDiff {
            new_files: vec![(path.clone(), "new.rs".into())],
            modified_files: vec![],
            deleted_files: vec![],
            rejected_unchanged: vec![],
            indexed_count: 10,
        };
        let error = perform_incremental_update(&root, &reader.meta, diff).unwrap_err();
        assert!(error.downcast_ref::<SourceReadError>().is_some());
        assert_eq!(get_index_dir(&root).unwrap(), generation);
        assert!(
            IndexReader::open(&root)
                .unwrap()
                .meta
                .rejected_files
                .is_empty()
        );
        fs::write(&path, "retryMarker\n").unwrap();
        update_index(&root).unwrap();
        assert_eq!(
            matches(&IndexReader::open(&root).unwrap(), "retryMarker"),
            vec![PathBuf::from("new.rs")]
        );
        crate::utils::remove_index(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_subtree_never_becomes_a_mass_deletion() {
        use std::os::unix::fs::PermissionsExt;
        let temp = fixture();
        let root = temp.path().canonicalize().unwrap();
        let subtree = root.join("private");
        fs::create_dir(&subtree).unwrap();
        fs::write(subtree.join("secret.rs"), "directoryRetryMarker\n").unwrap();
        build_index(&root, true).unwrap();
        let generation = get_index_dir(&root).unwrap();
        fs::set_permissions(&subtree, fs::Permissions::from_mode(0o0)).unwrap();
        if fs::read_dir(&subtree).is_ok() {
            fs::set_permissions(&subtree, fs::Permissions::from_mode(0o755)).unwrap();
            crate::utils::remove_index(&root).unwrap();
            return;
        }
        let update = update_index(&root);
        let rebuild = build_index(&root, true);
        fs::set_permissions(&subtree, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            update
                .unwrap_err()
                .downcast_ref::<SourceReadError>()
                .is_some()
        );
        assert!(
            rebuild
                .unwrap_err()
                .downcast_ref::<SourceReadError>()
                .is_some()
        );
        assert_eq!(get_index_dir(&root).unwrap(), generation);
        assert_eq!(
            matches(&IndexReader::open(&root).unwrap(), "directoryRetryMarker"),
            vec![PathBuf::from("private/secret.rs")]
        );
        crate::utils::remove_index(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn temporary_permissions_are_not_cached_as_content_rejection() {
        use std::os::unix::fs::PermissionsExt;
        let temp = fixture();
        let root = temp.path().canonicalize().unwrap();
        build_index(&root, true).unwrap();
        let generation = get_index_dir(&root).unwrap();
        let path = root.join("new.rs");
        fs::write(&path, "permissionRetryMarker\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
        if File::open(&path).is_ok() {
            // privileged test runners bypass mode bits
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            crate::utils::remove_index(&root).unwrap();
            return;
        }
        let result = update_index(&root);
        assert!(
            build_index(&root, true)
                .unwrap_err()
                .downcast_ref::<SourceReadError>()
                .is_some()
        );
        assert_eq!(get_index_dir(&root).unwrap(), generation);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            result
                .unwrap_err()
                .downcast_ref::<SourceReadError>()
                .is_some()
        );
        assert_eq!(get_index_dir(&root).unwrap(), generation);
        update_index(&root).unwrap();
        assert_eq!(
            matches(&IndexReader::open(&root).unwrap(), "permissionRetryMarker"),
            vec![PathBuf::from("new.rs")]
        );
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn file_hints_force_preserved_timestamp_edits_and_leave_other_subtrees_alone() {
        let temp = fixture();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir(root.join("unrelated")).unwrap();
        fs::write(root.join("unrelated/other.rs"), "beforeother\n").unwrap();
        build_index(&root, true).unwrap();
        let before = IndexReader::open(&root).unwrap();
        let path = root.join("base0.rs");
        let time = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "changedx marker\n").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(time))
            .unwrap();
        fs::write(root.join("unrelated/other.rs"), "afterxother\n").unwrap();
        assert_eq!(
            reconcile_index_paths(
                &root,
                Some(&before),
                30,
                &["base0.rs".into(), ".missing-save-temp".into()]
            )
            .unwrap(),
            UpdateOutcome::Incremental
        );
        let after = IndexReader::open(&root).unwrap();
        assert_eq!(matches(&after, "changedx"), vec![PathBuf::from("base0.rs")]);
        assert!(
            matches(&after, "afterxother").is_empty(),
            "a scoped scan visited an unrelated subtree"
        );
        assert_eq!(paths(&before), paths(&after));
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn oversized_preview_batches_use_the_durable_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for index in 0..1024 {
            fs::write(root.join(format!("old{index}.rs")), "original content\n").unwrap();
        }
        build_index(&root, true).unwrap();
        let before = IndexReader::open(&root).unwrap();
        let hints: Vec<PathBuf> = (0..257)
            .map(|index| format!("new{index}.rs").into())
            .collect();
        for path in &hints {
            fs::write(root.join(path), "boundedUpdateMarker\n").unwrap();
        }
        let outcome = reconcile_index_paths_with_visibility(
            &root,
            Some(&before),
            30,
            &hints,
            &mut |_| panic!("oversized batch must not allocate a memory snapshot"),
            true,
        )
        .unwrap();
        assert_eq!(outcome, UpdateOutcome::Incremental);
        let after = IndexReader::open(&root).unwrap();
        assert_ne!(before.generation_path(), after.generation_path());
        assert_eq!(after.valid_doc_ids().len(), 1281);
        assert_eq!(matches(&after, "boundedUpdateMarker").len(), 257);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn scoped_file_eligibility_matches_a_full_walk_and_keeps_unrelated_rejections() {
        let temp = fixture();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "*.skip\nblocked/\n").unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("nested/.ignore"), "denied.rs\n").unwrap();
        fs::write(root.join("rejected.dat"), b"binary\0\0bytes").unwrap();
        build_index(&root, true).unwrap();
        let before = IndexReader::open(&root).unwrap();
        assert!(
            before
                .meta
                .rejected_files
                .iter()
                .any(|(path, _)| path == Path::new("rejected.dat"))
        );
        let additions = [
            "nested/good.rs",
            "nested/denied.rs",
            "ignored.skip",
            "blocked/file.rs",
            "node_modules/file.rs",
            ".hidden.rs",
            "image.png",
            "empty.rs",
        ];
        for name in additions {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                if name == "empty.rs" {
                    ""
                } else {
                    "newmarker\n"
                },
            )
            .unwrap();
        }
        let hints: Vec<_> = additions.into_iter().map(PathBuf::from).collect();
        reconcile_index_paths(&root, Some(&before), 100, &hints).unwrap();
        let after = IndexReader::open(&root).unwrap();
        assert_eq!(
            matches(&after, "newmarker"),
            vec![PathBuf::from("nested/good.rs")]
        );
        assert_eq!(before.meta.rejected_files, after.meta.rejected_files);
        let scoped_paths = paths(&after);
        build_index(&root, true).unwrap();
        assert_eq!(scoped_paths, paths(&IndexReader::open(&root).unwrap()));
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn scoped_deletions_and_rejected_to_text_changes_replace_exact_paths() {
        let temp = fixture();
        let root = temp.path().canonicalize().unwrap();
        let rejected_path = root.join("rejected.dat");
        fs::write(&rejected_path, [0; 12]).unwrap();
        build_index(&root, true).unwrap();
        let before = IndexReader::open(&root).unwrap();
        let time = fs::metadata(&rejected_path).unwrap().modified().unwrap();
        fs::write(&rejected_path, "newcontents\n").unwrap();
        File::options()
            .write(true)
            .open(&rejected_path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(time))
            .unwrap();
        fs::remove_file(root.join("base0.rs")).unwrap();
        reconcile_index_paths(
            &root,
            Some(&before),
            100,
            &[
                "rejected.dat".into(),
                "base0.rs".into(),
                "missing.tmp".into(),
            ],
        )
        .unwrap();
        let after = IndexReader::open(&root).unwrap();
        assert_eq!(
            matches(&after, "newcontents"),
            vec![PathBuf::from("rejected.dat")]
        );
        assert!(!paths(&after).contains(&PathBuf::from("base0.rs")));
        assert_eq!(after.valid_doc_ids().len(), 12);
        assert!(after.meta.rejected_files.is_empty());
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn directory_and_ignore_hints_fall_back_to_full_reconciliation() {
        let temp = fixture();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir(root.join("subtree")).unwrap();
        fs::write(root.join("subtree/old.rs"), "subtreemarker\n").unwrap();
        build_index(&root, true).unwrap();
        let before = IndexReader::open(&root).unwrap();
        fs::rename(root.join("subtree"), root.join("renamed")).unwrap();
        reconcile_index_paths(&root, Some(&before), 100, &["subtree".into()]).unwrap();
        let renamed = IndexReader::open(&root).unwrap();
        assert_eq!(
            matches(&renamed, "subtreemarker"),
            vec![PathBuf::from("renamed/old.rs")]
        );
        fs::write(root.join(".ignore"), "base*.rs\n").unwrap();
        reconcile_index_paths(&root, Some(&renamed), 100, &[".ignore".into()]).unwrap();
        let ignored = IndexReader::open(&root).unwrap();
        assert_eq!(paths(&ignored), vec![PathBuf::from("renamed/old.rs")]);
        crate::utils::remove_index(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scoped_symlinks_obey_the_full_walk_policy() {
        let temp = fixture();
        let outside = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(outside.path().join("outside.rs"), "outsidemarker\n").unwrap();
        build_index(&root, true).unwrap();
        let before = IndexReader::open(&root).unwrap();
        fs::remove_file(root.join("base0.rs")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("outside.rs"), root.join("base0.rs"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("linked")).unwrap();
        reconcile_index_paths(
            &root,
            Some(&before),
            100,
            &["base0.rs".into(), "linked/outside.rs".into()],
        )
        .unwrap();
        let after = IndexReader::open(&root).unwrap();
        assert!(!paths(&after).contains(&PathBuf::from("base0.rs")));
        assert!(matches(&after, "outsidemarker").is_empty());
        let scoped_paths = paths(&after);
        build_index(&root, true).unwrap();
        assert_eq!(scoped_paths, paths(&IndexReader::open(&root).unwrap()));
        crate::utils::remove_index(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_child_hint_reconciles_other_descendants_of_a_replaced_ancestor() {
        let temp = fixture();
        let root = temp.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        for directory in ["symlinked", "removed", "filed"] {
            fs::create_dir(root.join(directory)).unwrap();
            for name in ["first.rs", "second.rs"] {
                fs::write(root.join(directory).join(name), "descendantmarker\n").unwrap();
            }
        }
        build_index(&root, true).unwrap();
        for directory in ["symlinked", "removed", "filed"] {
            let before = IndexReader::open(&root).unwrap();
            fs::remove_dir_all(root.join(directory)).unwrap();
            match directory {
                "symlinked" => {
                    std::os::unix::fs::symlink(outside.path(), root.join(directory)).unwrap()
                }
                "filed" => fs::write(root.join(directory), "replacementmarker\n").unwrap(),
                _ => {}
            }
            reconcile_index_paths(
                &root,
                Some(&before),
                100,
                &[PathBuf::from(directory).join("first.rs")],
            )
            .unwrap();
            let after = IndexReader::open(&root).unwrap();
            assert!(
                !paths(&after)
                    .iter()
                    .any(|path| path.starts_with(directory) && path != Path::new(directory))
            );
            if directory == "filed" {
                assert_eq!(
                    matches(&after, "replacementmarker"),
                    vec![PathBuf::from(directory)]
                );
            }
        }
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn external_generations_and_untrusted_paths_force_complete_reconciliation() {
        let temp = fixture();
        let root = temp.path().canonicalize().unwrap();
        build_index(&root, true).unwrap();
        let before = IndexReader::open(&root).unwrap();
        build_index(&root, true).unwrap();
        fs::write(root.join("unhinted.rs"), "externalmarker\n").unwrap();
        reconcile_index_paths(&root, Some(&before), 100, &["base0.rs".into()]).unwrap();
        let after = IndexReader::open(&root).unwrap();
        assert_eq!(
            matches(&after, "externalmarker"),
            vec![PathBuf::from("unhinted.rs")]
        );
        fs::write(root.join("another.rs"), "anothermarker\n").unwrap();
        reconcile_index_paths(&root, Some(&after), 100, &["../untrusted".into()]).unwrap();
        assert_eq!(
            matches(&IndexReader::open(&root).unwrap(), "anothermarker"),
            vec![PathBuf::from("another.rs")]
        );
        crate::utils::remove_index(&root).unwrap();
    }
}
