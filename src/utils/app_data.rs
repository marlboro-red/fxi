use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::fs;

const APP_NAME: &str = "fxi";
const CONFIG_FILE: &str = "config.json";

/// Application configuration stored in the app data directory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Enable parallel chunk processing during indexing
    #[serde(default = "default_parallel_chunk_indexing")]
    pub parallel_chunk_indexing: bool,

    /// Maximum number of chunks to process in parallel (limits memory usage)
    /// If None or 0, uses the number of CPU cores
    #[serde(default = "default_parallel_chunk_count")]
    pub parallel_chunk_count: usize,
}

fn default_parallel_chunk_indexing() -> bool {
    false
}

fn default_parallel_chunk_count() -> usize {
    0 // 0 means use CPU count
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            parallel_chunk_indexing: default_parallel_chunk_indexing(),
            parallel_chunk_count: default_parallel_chunk_count(),
        }
    }
}

impl AppConfig {
    /// Load config from the app data directory, or return default if not found
    pub fn load() -> Result<Self> {
        let config_path = get_config_path()?;

        if config_path.exists() {
            let content = fs::read_to_string(&config_path)
                .context("Failed to read config file")?;
            let config: AppConfig = serde_json::from_str(&content)
                .context("Failed to parse config file")?;
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    /// Save config to the app data directory
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let config_path = get_config_path()?;
        let content = serde_json::to_string_pretty(self)
            .context("Failed to serialize config")?;
        fs::write(&config_path, content)
            .context("Failed to write config file")?;
        Ok(())
    }

    /// Get the effective parallel chunk count (resolves 0 to CPU count)
    pub fn effective_parallel_chunk_count(&self) -> usize {
        if self.parallel_chunk_count == 0 {
            num_cpus()
        } else {
            self.parallel_chunk_count
        }
    }
}

/// Get the number of CPUs available
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Get the path to the config file
pub fn get_config_path() -> Result<PathBuf> {
    let app_dir = get_app_data_dir()?;
    Ok(app_dir.join(CONFIG_FILE))
}

/// Get the application data directory for storing indexes
pub fn get_app_data_dir() -> Result<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        dirs::home_dir()
            .map(|h| h.join("Library").join("Application Support"))
    } else if cfg!(target_os = "windows") {
        dirs::data_local_dir()
    } else {
        // Linux/Unix: use XDG_DATA_HOME or ~/.local/share
        dirs::data_dir()
    };

    let base = base.context("Could not determine app data directory")?;
    let app_dir = base.join(APP_NAME);

    fs::create_dir_all(&app_dir)?;
    Ok(app_dir)
}

/// Get the index directory for a specific codebase root
pub fn get_index_dir(root_path: &Path) -> Result<PathBuf> {
    let app_data = get_app_data_dir()?;
    let indexes_dir = app_data.join("indexes");
    fs::create_dir_all(&indexes_dir)?;

    // Create a unique folder name from the root path
    let folder_name = hash_path(root_path);
    let index_dir = indexes_dir.join(&folder_name);

    Ok(index_dir)
}

/// Hash a path to create a unique folder name
/// Format: first 8 chars of dir name + hash
fn hash_path(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path_str = canonical.to_string_lossy();

    // Get directory name for readability
    let dir_name = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    // Sanitize directory name (remove special chars, truncate)
    let sanitized: String = dir_name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(16)
        .collect();

    // Hash the full path
    let mut hasher = DefaultHasher::new();
    path_str.hash(&mut hasher);
    let hash = hasher.finish();

    format!("{}-{:016x}", sanitized, hash)
}

/// Find the root of a codebase starting from a given path
/// Walks up the directory tree looking for:
/// 1. A .git directory (git repo root)
/// 2. A previously indexed root (stored in our app data)
pub fn find_codebase_root(start_path: &Path) -> Result<PathBuf> {
    let start = start_path.canonicalize()?;
    let mut current = start.as_path();

    // First, try to find a git root
    loop {
        let git_dir = current.join(".git");
        if git_dir.exists() {
            return Ok(current.to_path_buf());
        }

        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }

    // No git root found, check if any parent is already indexed
    current = start.as_path();
    loop {
        if is_indexed(current)? {
            return Ok(current.to_path_buf());
        }

        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }

    // No existing index found, use the start path as root
    Ok(start)
}

/// Check if a path has an existing index
pub fn is_indexed(root_path: &Path) -> Result<bool> {
    let index_dir = get_index_dir(root_path)?;
    let meta_path = index_dir.join("meta.json");
    Ok(meta_path.exists())
}

/// Get metadata about an indexed codebase
#[allow(dead_code)]
pub fn get_index_metadata(root_path: &Path) -> Result<Option<IndexLocation>> {
    let index_dir = get_index_dir(root_path)?;
    let meta_path = index_dir.join("meta.json");

    if !meta_path.exists() {
        return Ok(None);
    }

    Ok(Some(IndexLocation {
        root_path: root_path.to_path_buf(),
        index_dir,
    }))
}

/// List all indexed codebases
pub fn list_indexed_codebases() -> Result<Vec<IndexLocation>> {
    let app_data = get_app_data_dir()?;
    let indexes_dir = app_data.join("indexes");

    if !indexes_dir.exists() {
        return Ok(Vec::new());
    }

    let mut codebases = Vec::new();

    for entry in fs::read_dir(&indexes_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let meta_path = path.join("meta.json");
            if meta_path.exists() {
                // Read the meta.json to get the root path
                if let Ok(file) = fs::File::open(&meta_path) {
                    if let Ok(meta) = serde_json::from_reader::<_, serde_json::Value>(file) {
                        if let Some(root) = meta.get("root_path").and_then(|v| v.as_str()) {
                            codebases.push(IndexLocation {
                                root_path: PathBuf::from(root),
                                index_dir: path,
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(codebases)
}

/// Remove an index for a codebase
pub fn remove_index(root_path: &Path) -> Result<()> {
    let index_dir = get_index_dir(root_path)?;
    if index_dir.exists() {
        fs::remove_dir_all(&index_dir)?;
    }
    Ok(())
}

/// Information about an indexed codebase
#[derive(Debug, Clone)]
pub struct IndexLocation {
    pub root_path: PathBuf,
    pub index_dir: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_path() {
        let hash1 = hash_path(Path::new("/home/user/project"));
        let hash2 = hash_path(Path::new("/home/user/project"));
        let hash3 = hash_path(Path::new("/home/user/other"));

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_app_config_default() {
        let config = AppConfig::default();
        assert!(!config.parallel_chunk_indexing);
        assert_eq!(config.parallel_chunk_count, 0);
    }

    #[test]
    fn test_app_config_effective_parallel_count() {
        let mut config = AppConfig::default();

        // 0 should resolve to CPU count
        let cpu_count = config.effective_parallel_chunk_count();
        assert!(cpu_count >= 1);

        // Explicit value should be used as-is
        config.parallel_chunk_count = 4;
        assert_eq!(config.effective_parallel_chunk_count(), 4);
    }

    #[test]
    fn test_app_config_serialization() {
        let config = AppConfig {
            parallel_chunk_indexing: true,
            parallel_chunk_count: 8,
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.parallel_chunk_indexing, true);
        assert_eq!(parsed.parallel_chunk_count, 8);
    }

    #[test]
    fn test_app_config_partial_json() {
        // Should use defaults for missing fields
        let json = r#"{"parallel_chunk_indexing": true}"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();

        assert!(config.parallel_chunk_indexing);
        assert_eq!(config.parallel_chunk_count, 0); // default
    }

    #[test]
    fn test_app_config_empty_json() {
        // Empty object should use all defaults
        let json = "{}";
        let config: AppConfig = serde_json::from_str(json).unwrap();

        assert!(!config.parallel_chunk_indexing);
        assert_eq!(config.parallel_chunk_count, 0);
    }
}
