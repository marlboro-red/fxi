//! Offline candidate-precision upper bound for longer byte grams.
//! No index mutation, query latency claim, or implemented persisted format.
use anyhow::{Context, Result, ensure};
use fxi::index::reader::IndexReader;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

fn grams(bytes: &[u8], width: usize) -> Vec<Vec<u8>> {
    bytes
        .windows(width)
        .map(<[u8]>::to_vec)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
fn bridge(gram: &[u8]) -> bool {
    gram.iter().any(u8::is_ascii_alphanumeric) && gram.iter().any(|b| !b.is_ascii_alphanumeric())
}
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let root =
        Path::new(&args.next().context("phrase_gram_lab ROOT [LITERAL ...]")?).canonicalize()?;
    let literals: Vec<_> = args.collect();
    ensure!(!literals.is_empty(), "provide at least one literal");
    let reader = IndexReader::open(&root)?;
    let mut rows = Vec::new();
    for literal in literals {
        ensure!(
            literal.is_ascii() && literal.len() >= 3 && !literal.contains(['\r', '\n']),
            "use ASCII single-line literals of at least three bytes"
        );
        let query_grams: Vec<_> = fxi::utils::query_trigrams(&literal)
            .into_iter()
            .filter(|g| !reader.is_stop_gram(*g))
            .collect();
        let baseline = if query_grams.is_empty() {
            reader.valid_doc_ids().clone()
        } else {
            reader.get_trigram_docs_with_bloom(&query_grams) & reader.valid_doc_ids()
        };
        let oracle = std::process::Command::new("rg")
            .args(["-l", "-F", "--null", "--color=never", "--", &literal, "."])
            .current_dir(&root)
            .output()?;
        ensure!(
            matches!(oracle.status.code(), Some(0 | 1)),
            "ripgrep failed"
        );
        let expected: BTreeSet<PathBuf> = std::str::from_utf8(&oracle.stdout)?
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|s| {
                Path::new(s)
                    .components()
                    .filter(|c| *c != Component::CurDir)
                    .collect()
            })
            .collect();
        let sources: Vec<_> = baseline
            .iter()
            .map(|id| -> Result<_> {
                let doc = reader.get_document(id).context("document missing")?;
                let path = reader.get_path(doc).context("path missing")?.clone();
                let text = std::fs::read(root.join(&path))?;
                Ok((path, text.to_ascii_lowercase()))
            })
            .collect::<Result<_>>()?;
        let paths: BTreeSet<_> = sources.iter().map(|(p, _)| p.clone()).collect();
        ensure!(
            expected.is_subset(&paths),
            "stale index or incomplete corpus coverage"
        );
        let lowered = literal.to_ascii_lowercase();
        let mut widths = Vec::new();
        for width in [4, 5, 6, 8] {
            if lowered.len() < width {
                continue;
            }
            let grams = grams(lowered.as_bytes(), width);
            let mut frequencies = vec![0usize; grams.len()];
            let mut accepted = BTreeSet::new();
            let mut bridge_accepted = BTreeSet::new();
            for (path, source) in &sources {
                let mut all = true;
                let mut all_bridges = true;
                for (i, gram) in grams.iter().enumerate() {
                    if memchr::memmem::find(source, gram).is_some() {
                        frequencies[i] += 1;
                    } else {
                        all = false;
                        if bridge(gram) {
                            all_bridges = false;
                        }
                    }
                }
                if all {
                    accepted.insert(path.clone());
                }
                if all_bridges {
                    bridge_accepted.insert(path.clone());
                }
            }
            ensure!(
                expected.is_subset(&accepted),
                "longer grams lost a true match"
            );
            ensure!(
                expected.is_subset(&bridge_accepted),
                "bridge grams lost a true match"
            );
            let best = frequencies
                .iter()
                .enumerate()
                .min_by_key(|(_, n)| *n)
                .unwrap();
            widths.push(serde_json::json!({"width":width,"all_gram_files":accepted.len(),"bridge_only_files":bridge_accepted.len(),"best_single_gram":String::from_utf8_lossy(&grams[best.0]),"best_single_files":best.1,"query_grams":grams.len(),"bridge_grams":grams.iter().filter(|g|bridge(g)).count()}));
        }
        rows.push(serde_json::json!({"literal":literal,"baseline_files":baseline.len(),"baseline_source_bytes":sources.iter().map(|(_,s)|s.len()).sum::<usize>(),"matching_files":expected.len(),"widths":widths}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"root":root,"rows":rows,"method":"Offline exact longer-byte-gram presence, ASCII-folded source and query, inside existing trigram candidates. Bridge grams contain alphanumeric and delimiter bytes. Every retained set must contain independent ripgrep matches. This is a precision upper bound: no storage, indexing or query overhead included; no speed or novelty claim. Sources must match the existing index."})
        )?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn true_substrings_survive_all_and_bridge_grams() {
        for literal in [
            "struct file_operations",
            "static const",
            "x-y.z",
            " foo ",
            "abc___def",
        ] {
            for prefix in ["", "prefix", "🦀", "destruct "] {
                for suffix in ["", "_suffix", "\r\n", "λ"] {
                    let source = format!("{prefix}{literal}{suffix}")
                        .into_bytes()
                        .to_ascii_lowercase();
                    for width in [4, 5, 6, 8] {
                        for gram in grams(literal.to_ascii_lowercase().as_bytes(), width) {
                            assert!(memchr::memmem::find(&source, &gram).is_some());
                        }
                    }
                }
            }
        }
        assert!(bridge(b"ct f"));
        assert!(!bridge(b"file"));
        assert!(!bridge(b"___ "));
    }
}
