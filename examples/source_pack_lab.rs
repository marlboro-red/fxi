//! Offline verification experiment: separate editable files versus a private
//! immutable packed copy, with per-file metadata validation and edit fallback.
//! This is NOT an on-disk index format or end-to-end CLI timing.
use anyhow::{Context, Result, ensure};
use fxi::index::reader::IndexReader;
use memchr::memmem::Finder;
use rayon::prelude::*;
use std::fs::{self, File, Metadata};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}
impl From<Metadata> for Stamp {
    fn from(meta: Metadata) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            created: meta.created().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
            },
        }
    }
}
struct Entry {
    id: u32,
    path: PathBuf,
    relative: PathBuf,
    stamp: Stamp,
    start: usize,
    end: usize,
}
fn ordinary(entry: &Entry, finder: &Finder<'_>) -> bool {
    fs::read_to_string(&entry.path).is_ok_and(|text| finder.find(text.as_bytes()).is_some())
}
fn packed(entry: &Entry, bytes: &[u8], finder: &Finder<'_>) -> bool {
    let Ok(metadata) = fs::metadata(&entry.path) else {
        return false;
    };
    if !cfg!(unix) || Stamp::from(metadata) != entry.stamp {
        return ordinary(entry, finder);
    }
    bytes
        .get(entry.start..entry.end)
        .and_then(|text| std::str::from_utf8(text).ok())
        .is_some_and(|text| finder.find(text.as_bytes()).is_some())
}

fn main() -> Result<()> {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("source_pack_lab ROOT [LITERAL ...]")?,
    )
    .canonicalize()?;
    let mut patterns: Vec<_> = std::env::args().skip(2).collect();
    if patterns.is_empty() {
        patterns = ["return", "struct file_operations", "folio_wait_bit_common"]
            .into_iter()
            .map(str::to_owned)
            .collect();
    }
    ensure!(
        patterns
            .iter()
            .all(|p| !p.is_empty() && !p.contains(['\r', '\n'])),
        "only nonempty line-local fixed literals"
    );
    let reader = IndexReader::open_uncached(&root)?;
    let mut file = tempfile::tempfile()?;
    let mut entries = Vec::new();
    let mut end = 0;
    let start = Instant::now();
    for doc in reader.documents().iter().filter(|d| d.is_valid()) {
        let path = reader.get_full_path(doc).context("document path")?;
        let mut source = File::open(&path)?;
        let before = Stamp::from(source.metadata()?);
        let mut text = String::new();
        source.read_to_string(&mut text)?;
        ensure!(
            before == Stamp::from(source.metadata()?),
            "source changed during copy"
        );
        file.write_all(text.as_bytes())?;
        entries.push(Entry {
            id: doc.doc_id,
            path,
            relative: reader.get_path(doc).unwrap().clone(),
            stamp: before,
            start: end,
            end: end + text.len(),
        });
        end += text.len();
    }
    file.flush()?;
    let copy_seconds = start.elapsed().as_secs_f64();
    // Private tempfile is never written again, and is kept alive with the map.
    // Editable source files are never mapped.
    let map = unsafe { memmap2::Mmap::map(&file)? };
    let mut rows = Vec::new();
    for pattern in patterns {
        let grams: Vec<_> = fxi::utils::query_trigrams(&pattern)
            .into_iter()
            .filter(|g| !reader.is_stop_gram(*g))
            .collect();
        let ids = if grams.is_empty() {
            reader.valid_doc_ids().clone()
        } else {
            reader.get_trigram_docs_with_bloom(&grams)? & reader.valid_doc_ids()
        };
        let candidates: Vec<_> = entries.iter().filter(|e| ids.contains(e.id)).collect();
        let oracle = std::process::Command::new("rg")
            .args(["-l", "-F", "--color=never", "--", &pattern, "."])
            .current_dir(&root)
            .output()?;
        ensure!(
            matches!(oracle.status.code(), Some(0 | 1)),
            "ripgrep failed"
        );
        let mut expected: Vec<_> = std::str::from_utf8(&oracle.stdout)?
            .lines()
            .map(|s| PathBuf::from(s.strip_prefix("./").unwrap_or(s)))
            .collect();
        expected.sort();
        let finder = Finder::new(pattern.as_bytes());
        let mut samples = [Vec::new(), Vec::new()];
        for repetition in 0..12 {
            for mode in [repetition % 2, 1 - repetition % 2] {
                let start = Instant::now();
                let mut actual: Vec<_> = candidates
                    .par_iter()
                    .with_min_len((candidates.len() / 4).max(1))
                    .filter(|entry| {
                        if mode == 0 {
                            ordinary(entry, &finder)
                        } else {
                            packed(entry, &map, &finder)
                        }
                    })
                    .map(|entry| &entry.relative)
                    .collect();
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                actual.sort();
                ensure!(
                    actual == expected.iter().collect::<Vec<_>>(),
                    "result mismatch for {pattern}, mode {mode}"
                );
                if repetition != 0 {
                    samples[mode].push(ms);
                }
            }
        }
        let medians: Vec<_> = samples
            .iter()
            .map(|sample| {
                let mut copy = sample.clone();
                copy.sort_by(f64::total_cmp);
                copy[copy.len() / 2]
            })
            .collect();
        rows.push(serde_json::json!({"pattern":pattern,"candidate_files":candidates.len(),"matching_files":expected.len(),"ordinary_median_ms":medians[0],"packed_median_ms":medians[1],"ordinary_samples_ms":samples[0],"packed_samples_ms":samples[1]}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"root":root,"files":entries.len(),"source_pack_bytes":end,"copy_seconds":copy_seconds,"rows":rows,"limits":"Offline verification-stage experiment, warm filesystem, four read tasks; index opening/planning, path sorting, CLI startup and serialization excluded. Both paths verify complete UTF-8 and every sample matches ripgrep. Pack retains uncompressed source plus in-memory metadata; no serialized format, corruption checksum, incremental update/compaction integration, cold-storage result, or whole-index build-time claim. Packed reads are disabled on non-Unix platforms. Changed stamps fall back to ordinary source reads; source races have the same point-in-time limitations as the current content cache."})
        )?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packed_evidence_falls_back_after_edits_and_rejects_deleted_or_invalid_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, "oldneedle").unwrap();
        let entry = Entry {
            id: 0,
            relative: PathBuf::from("file"),
            stamp: fs::metadata(&path).unwrap().into(),
            path: path.clone(),
            start: 0,
            end: 9,
        };
        assert!(packed(&entry, b"oldneedle", &Finder::new(b"oldneedle")));
        fs::write(&path, "newneedle").unwrap();
        assert!(!packed(&entry, b"oldneedle", &Finder::new(b"oldneedle")));
        assert!(packed(&entry, b"oldneedle", &Finder::new(b"newneedle")));
        fs::write(&path, b"oldneedle\xff").unwrap();
        assert!(!packed(&entry, b"oldneedle", &Finder::new(b"oldneedle")));
        fs::remove_file(path).unwrap();
        assert!(!packed(&entry, b"oldneedle", &Finder::new(b"oldneedle")));
    }
}
