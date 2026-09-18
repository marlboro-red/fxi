//! Replay with FXI_STATEFUL_SEED=<decimal seed> cargo test --test stateful_differential.
use anyhow::{Context, Result, ensure};
use fxi::index::{
    build::{build_index_with_options, update_index},
    compact::merge_segments,
    reader::IndexReader,
};
use fxi::query::{QueryExecutor, parse_query};
use fxi::utils::{IndexLock, app_data::remove_index};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const NEEDLES: &[&str] = &[
    "alpha",
    "beta",
    "needle",
    "vector::start",
    "café",
    "absentmarker",
];
#[derive(Clone, Debug)]
enum Operation {
    Write(usize, String),
    Delete(usize),
    Rename(usize, usize),
    Checkpoint,
    Compact,
    Rebuild,
}
struct Fixture(tempfile::TempDir);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = remove_index(self.0.path());
    }
}
fn name(slot: usize) -> PathBuf {
    let directories = ["src", "nested/deep", "space dir", "unicode-é"];
    PathBuf::from(directories[slot % 4]).join(format!(
        "file {slot}.{}",
        if slot.is_multiple_of(2) { "rs" } else { "txt" }
    ))
}
fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
fn sequence(seed: u64) -> Vec<Operation> {
    let mut state = seed.max(1);
    let bodies = [
        "alpha beta\nneedle needle\n",
        "beta alpha\r\nvector::start\r\n",
        "café needle\n\nalpha",
        "",
        "unrelated\n",
        "alpha\nbeta\nneedle\n",
        "needle",
        "beta beta\nalpha alpha\n",
    ];
    let mut result = vec![
        Operation::Write(0, "omega beta\nneedle\n".into()),
        Operation::Checkpoint,
        Operation::Write(0, "alpha beta\nneedle\n".into()),
        Operation::Checkpoint,
        Operation::Checkpoint, // Idempotent no-change update.
    ];
    for round in 0..16 {
        // Adjacent rewrites intentionally include equal-length edits and edits
        // within one clock tick; no sleep masks metadata/freshness mistakes.
        for _ in 0..3 {
            let slot = (random(&mut state) % 16) as usize;
            result.push(match random(&mut state) % 5 {
                0 => Operation::Delete(slot),
                1 => Operation::Rename(slot, (random(&mut state) % 16) as usize),
                _ => Operation::Write(
                    slot,
                    bodies[(random(&mut state) % bodies.len() as u64) as usize].into(),
                ),
            });
        }
        result.push(Operation::Checkpoint);
        if round % 4 == 3 {
            result.push(Operation::Compact);
        }
        if round == 8 {
            result.push(Operation::Rebuild);
        }
    }
    result
}
// Independently walk and scan the actual live source, without index metadata,
// candidate planning, regex matching, or the mutation generator's state.
fn oracle(root: &Path) -> Result<BTreeMap<String, Vec<(PathBuf, usize)>>> {
    fn walk(root: &Path, at: &Path, files: &mut Vec<(PathBuf, String)>) -> Result<()> {
        for entry in fs::read_dir(at)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, files)?;
            } else {
                files.push((
                    path.strip_prefix(root)?.to_path_buf(),
                    fs::read_to_string(path)?,
                ));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    let mut result = BTreeMap::new();
    for needle in NEEDLES {
        let mut matches = files
            .iter()
            .filter_map(|(path, text)| {
                let count = text.lines().filter(|line| line.contains(needle)).count();
                (count > 0).then(|| (path.clone(), count))
            })
            .collect::<Vec<_>>();
        matches.sort();
        result.insert((*needle).into(), matches);
    }
    Ok(result)
}
fn verify(root: &Path) -> Result<()> {
    let expected = oracle(root)?;
    // Reopen independently to exercise serialized state and repeated mapping.
    for _ in 0..2 {
        let reader = IndexReader::open(root)?;
        let executor = QueryExecutor::new(&reader);
        for needle in NEEDLES {
            let query = parse_query(&format!("re:/{needle}/"));
            let mut files = executor.execute_files_only(&query, 0)?;
            files.sort();
            let wanted = expected[*needle]
                .iter()
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>();
            ensure!(
                files == wanted,
                "files mismatch for {needle}: got {files:?}, expected {wanted:?}"
            );
            let mut counts = executor.execute_match_counts(&query, 0)?;
            counts.sort();
            ensure!(
                counts == expected[*needle],
                "counts mismatch for {needle}: got {counts:?}, expected {:?}",
                expected[*needle]
            );
        }
    }
    Ok(())
}
fn replay(operations: &[Operation]) -> Result<()> {
    let fixture = Fixture(tempfile::tempdir()?);
    let root = fixture.0.path();
    fs::create_dir(root.join(".git"))?;
    for slot in 0..16 {
        let path = root.join(name(slot));
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, "alpha beta\nneedle\n")?;
    }
    // Stable files keep deletion/update sequences below automatic rebuild
    // thresholds, ensuring delta/tombstone paths get exercised as well.
    for slot in 0..32 {
        fs::write(root.join(format!("stable-{slot}.txt")), "stable alpha\n")?;
    }
    build_index_with_options(root, true, true, Some(5))?;
    let pinned = IndexReader::open(root)?;
    let original_postings = pinned.get_token_docs("needle");
    let original_lines = original_postings
        .iter()
        .map(|id| (id, pinned.get_line_map(id).unwrap()))
        .collect::<Vec<_>>();
    verify(root)?;
    for (step, operation) in operations.iter().enumerate() {
        let apply = || -> Result<()> {
            match operation {
                Operation::Write(slot, content) => fs::write(root.join(name(*slot)), content)?,
                Operation::Delete(slot) => {
                    let path = root.join(name(*slot));
                    if path.exists() {
                        fs::remove_file(path)?;
                    }
                }
                Operation::Rename(from, to) => {
                    let source = root.join(name(*from));
                    let target = root.join(name(*to));
                    if source.exists() && from != to {
                        // Explicit removal has portable replace semantics on Windows.
                        if target.exists() {
                            fs::remove_file(&target)?;
                        }
                        fs::rename(source, target)?;
                    }
                }
                Operation::Checkpoint | Operation::Compact | Operation::Rebuild => {
                    {
                        let _lock = IndexLock::acquire(root)?;
                        update_index(root)?;
                    }
                    verify(root)?;
                    if matches!(operation, Operation::Compact) {
                        merge_segments(root)?;
                        verify(root)?;
                    }
                    if matches!(operation, Operation::Rebuild) {
                        build_index_with_options(root, true, true, Some(5))?;
                        verify(root)?;
                    }
                    // Old reader promises pinned resources, not a snapshot of
                    // source files. Inspect stored postings/line maps only.
                    ensure!(
                        pinned.get_token_docs("needle") == original_postings,
                        "pinned postings changed"
                    );
                    for (id, lines) in &original_lines {
                        ensure!(
                            pinned.get_line_map(*id)? == *lines,
                            "pinned line map changed for {id}"
                        );
                    }
                }
            }
            Ok(())
        };
        apply().with_context(|| format!("step {step}: {operation:?}"))?;
    }
    Ok(())
}
#[test]
fn generated_mutations_match_exhaustive_live_source_at_durable_checkpoints() {
    let seeds = std::env::var("FXI_STATEFUL_SEED")
        .map(|s| vec![s.parse::<u64>().expect("decimal FXI_STATEFUL_SEED")])
        .unwrap_or_else(|_| vec![1, 0x194920260918, 0xdeadbeef, 0x123456789abcdef]);
    for seed in seeds {
        let operations = sequence(seed);
        if let Err(error) = replay(&operations) {
            // Failure-only delta debugging: replay fresh isolated roots, remove
            // chunks, and retain reductions that still fail. The final sequence
            // and seed are printed so CI failures do not depend on temporary data.
            let mut reduced = operations.clone();
            let mut chunk = reduced.len() / 2;
            while chunk > 0 {
                let mut start = 0;
                while start + chunk <= reduced.len() {
                    let mut candidate = reduced.clone();
                    candidate.drain(start..start + chunk);
                    if replay(&candidate).is_err() {
                        reduced = candidate;
                    } else {
                        start += chunk;
                    }
                }
                chunk /= 2;
            }
            panic!("seed={seed}; original failure: {error:#}; minimized replay:\n{reduced:#?}");
        }
    }
}
