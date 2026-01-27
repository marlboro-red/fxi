use crate::index::types::IndexConfig;
use crate::index::writer::IndexWriter;
use crate::utils::{find_codebase_root, get_index_dir, remove_index};
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Build or rebuild the search index
pub fn build_index(root_path: &Path, force: bool) -> Result<()> {
    let root = root_path.canonicalize().context("Invalid path")?;
    let index_path = get_index_dir(&root)?;

    // Check if we should force rebuild
    if force && index_path.exists() {
        remove_index(&root).context("Failed to remove existing index")?;
    }

    let config = IndexConfig::default();
    let mut writer = IndexWriter::new(&root, config)?;

    println!("Indexing: {}", root.display());

    // Walk the directory respecting .gitignore
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

    let mut file_count = 0;
    let mut error_count = 0;

    for entry in walker {
        match entry {
            Ok(entry) => {
                let path = entry.path();

                // Skip directories
                if !path.is_file() {
                    continue;
                }

                // Get relative path
                let rel_path = match path.strip_prefix(&root) {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                // Read file content
                let content = match fs::read(path) {
                    Ok(c) => c,
                    Err(e) => {
                        error_count += 1;
                        if error_count <= 5 {
                            eprintln!("Warning: Could not read {}: {}", path.display(), e);
                        }
                        continue;
                    }
                };

                // Get modification time
                let mtime = path
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64)
                    .unwrap_or(0);

                // Add to index
                match writer.add_file(rel_path, &content, mtime) {
                    Ok(doc_id) => {
                        if doc_id > 0 {
                            file_count += 1;
                            if file_count % 1000 == 0 {
                                print!("\rIndexed {} files...", file_count);
                                let _ = std::io::Write::flush(&mut std::io::stdout());
                            }
                        }
                    }
                    Err(e) => {
                        error_count += 1;
                        if error_count <= 5 {
                            eprintln!("Warning: Could not index {}: {}", path.display(), e);
                        }
                    }
                }
            }
            Err(e) => {
                error_count += 1;
                if error_count <= 5 {
                    eprintln!("Warning: Walk error: {}", e);
                }
            }
        }
    }

    println!("\rIndexed {} files.                    ", file_count);

    // Write the index
    writer.write().context("Failed to write index")?;

    println!("Index stored at: {}", index_path.display());

    if error_count > 5 {
        eprintln!("({} total warnings/errors suppressed)", error_count - 5);
    }

    Ok(())
}

/// Incrementally update the index
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
