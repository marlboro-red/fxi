use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const APP_NAME: &str = "fxi";

struct TestAppData {
    path: PathBuf,
    owner: u32,
}

static TEST_APP_DATA: OnceLock<std::result::Result<TestAppData, String>> = OnceLock::new();

/// Give this test process its own application data directory before using FXI.
///
/// Unit-test builds select this automatically. Integration tests and benchmark
/// fixtures link the ordinary library, so they must call this explicitly before
/// any index operation. All threads share one directory without changing the
/// process environment. An explicit `FXI_INDEXES` still takes precedence; child
/// CLI processes must receive `FXI_APP_DATA` and `FXI_INDEXES` through `Command::env`
/// so configuration reads also use the fixture's private directory.
/// Normal process exit removes only the directory created here. An abort or kill
/// can leave temporary files, but never creates indexes in the user's app data.
#[doc(hidden)]
pub fn isolate_test_storage() -> Result<PathBuf> {
    let storage = TEST_APP_DATA.get_or_init(|| {
        (|| -> Result<TestAppData> {
            let owner = std::process::id();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let mut attempt = 0u64;
            let path = loop {
                let path = env::temp_dir().join(format!("fxi-tests-{owner}-{nonce:x}-{attempt}"));
                let builder = fs::DirBuilder::new();
                #[cfg(unix)]
                let builder = {
                    use std::os::unix::fs::DirBuilderExt;
                    let mut builder = builder;
                    builder.mode(0o700);
                    builder
                };
                match builder.create(&path) {
                    Ok(()) => break path,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        attempt = attempt.checked_add(1).context("Test directory capacity")?;
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            // SAFETY: this callback has the required C ABI, uses process-lifetime
            // OnceLock state, and only removes our exclusively created directory.
            // Static values do not run Drop at exit, so a TempDir in a static
            // would otherwise leak every test process's files.
            if unsafe { libc::atexit(cleanup_test_storage) } != 0 {
                let _ = fs::remove_dir_all(&path);
                anyhow::bail!("Cannot register test storage cleanup");
            }
            Ok(TestAppData { path, owner })
        })()
        .map_err(|error| error.to_string())
    });
    match storage {
        Ok(storage) => Ok(storage.path.clone()),
        Err(error) => anyhow::bail!("Cannot isolate test storage: {error}"),
    }
}

extern "C" fn cleanup_test_storage() {
    if let Some(Ok(storage)) = TEST_APP_DATA.get()
        && storage.owner == std::process::id()
    {
        // A forked child must not remove its parent's shared fixture directory.
        let _ = fs::remove_dir_all(&storage.path);
    }
}

/// Resolve the application data path without creating production directories.
/// Test builds and explicitly initialized test fixtures use private storage.
pub fn get_app_data_path() -> Result<PathBuf> {
    if cfg!(test) || TEST_APP_DATA.get().is_some() {
        return isolate_test_storage();
    }
    if let Some(custom_dir) = env::var_os("FXI_APP_DATA") {
        return Ok(PathBuf::from(custom_dir));
    }
    let base = if cfg!(target_os = "macos") {
        dirs::home_dir().map(|h| h.join("Library").join("Application Support"))
    } else if cfg!(target_os = "windows") {
        dirs::data_local_dir()
    } else {
        // Linux/Unix: use XDG_DATA_HOME or ~/.local/share
        dirs::data_dir()
    };

    Ok(base
        .context("Could not determine app data directory")?
        .join(APP_NAME))
}

/// Get or create the application data directory for storing indexes.
pub fn get_app_data_dir() -> Result<PathBuf> {
    let app_dir = get_app_data_path()?;
    fs::create_dir_all(&app_dir)?;
    Ok(app_dir)
}

/// Get the indexes directory, using FXI_INDEXES env var if set
fn get_indexes_dir() -> Result<PathBuf> {
    let indexes_dir = if let Some(custom_dir) = env::var_os("FXI_INDEXES") {
        PathBuf::from(custom_dir)
    } else {
        let app_data = get_app_data_dir()?;
        app_data.join("indexes")
    };
    fs::create_dir_all(&indexes_dir)?;
    Ok(indexes_dir)
}

/// Get the index directory for a specific codebase root
pub fn get_index_dir(root_path: &Path) -> Result<PathBuf> {
    crate::index::generation::resolve(&get_index_container(root_path)?)
}

/// Stable container and lock identity, independent of published generation.
pub fn get_index_container(root_path: &Path) -> Result<PathBuf> {
    let canonical = root_path
        .canonicalize()
        .unwrap_or_else(|_| root_path.to_path_buf());
    anyhow::ensure!(
        canonical.to_str().is_some(),
        "Non-UTF-8 index root paths are not supported; rename the directory to valid UTF-8"
    );
    let indexes_dir = get_indexes_dir()?;

    // Validate before hashing; lossy conversion can alias distinct Unix paths.
    let folder_name = hash_path(&canonical);
    let index_dir = indexes_dir.join(&folder_name);

    Ok(index_dir)
}

/// Hash a path to create a unique folder name
/// Format: first 8 chars of dir name + hash
fn hash_path(path: &Path) -> String {
    let canonical = path;
    let path_str = canonical
        .to_str()
        .expect("index root validated before hashing");

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
    let indexes_dir = get_indexes_dir()?;

    if !indexes_dir.exists() {
        return Ok(Vec::new());
    }

    let mut codebases = Vec::new();

    for entry in fs::read_dir(&indexes_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = crate::index::generation::resolve(&entry.path())?;

        if path.is_dir() {
            let meta_path = path.join("meta.json");
            // Read the meta.json to get the root path
            if meta_path.exists()
                && let Ok(file) = fs::File::open(&meta_path)
                && let Ok(meta) = serde_json::from_reader::<_, serde_json::Value>(file)
                && let Some(root) = meta.get("root_path").and_then(|v| v.as_str())
            {
                codebases.push(IndexLocation {
                    root_path: PathBuf::from(root),
                    index_dir: path,
                });
            }
        }
    }

    Ok(codebases)
}

/// Remove an index for a codebase
pub fn remove_index(root_path: &Path) -> Result<()> {
    let index_dir = get_index_container(root_path)?;
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
    fn unit_test_storage_is_automatic_and_shared_with_workers() {
        let directory = get_app_data_dir().unwrap();
        assert_eq!(directory.parent(), Some(env::temp_dir().as_path()));
        assert!(
            directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("fxi-tests-")
        );
        let workers: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| get_app_data_dir().unwrap()))
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), directory);
        }
    }
}

#[cfg(all(test, unix))]
mod invalid_root_tests {
    #[test]
    fn non_utf8_root_is_rejected_before_lossy_hashing() {
        use std::os::unix::ffi::OsStringExt;
        let root =
            std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/invalid-root-\xff".to_vec()));
        let error = super::get_index_container(&root).unwrap_err();
        assert!(error.to_string().contains("Non-UTF-8"));
    }
}
