//! Existing token-index feasibility for exact, case-sensitive byte substrings.
//!
//! FXI_INDEXES=... cargo run --release --example interior_token_lab -- ROOT [LITERAL ...]
//! Only tokens bounded entirely inside the literal constrain candidates. Its
//! first/last token fragments remain unconstrained. This is an offline precision
//! probe: no index changes and no end-to-end latency claims.
use anyhow::{Context, Result, ensure};
use fxi::index::reader::IndexReader;
use roaring::RoaringBitmap;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// All returned ASCII tokens have both boundaries fixed by literal bytes.
/// Ordinals still count omitted short/long/edge tokens, preserving exact gaps.
/// Never apply this proof to case-insensitive literals: Unicode case folding can
/// replace an ASCII letter with bytes that the tokenizer treats as delimiters.
fn interior_tokens(literal: &str) -> Vec<(String, u32)> {
    let bytes = literal.as_bytes();
    let mut result = Vec::new();
    let mut start = None;
    let mut previous_lower = false;
    let mut position = 0;
    let mut emit = |start: usize, end: usize, position| {
        if start > 0 && end < bytes.len() && (2..=128).contains(&(end - start)) {
            result.push((literal[start..end].to_ascii_lowercase(), position));
        }
    };
    for (offset, byte) in bytes.iter().copied().chain(std::iter::once(0)).enumerate() {
        if byte.is_ascii_alphanumeric() {
            if byte.is_ascii_uppercase() && previous_lower {
                if let Some(begin) = start {
                    emit(begin, offset, position);
                    position += 1;
                }
                start = Some(offset);
            } else if start.is_none() {
                start = Some(offset);
            }
            previous_lower = byte.is_ascii_lowercase();
        } else {
            if let Some(begin) = start.take() {
                emit(begin, offset, position);
                position += 1;
            }
            previous_lower = false;
        }
    }
    result
}

fn paths(reader: &IndexReader, ids: &RoaringBitmap) -> BTreeSet<PathBuf> {
    ids.iter()
        .filter_map(|id| {
            reader
                .get_document(id)
                .and_then(|d| reader.get_path(d))
                .cloned()
        })
        .collect()
}
fn bytes(reader: &IndexReader, ids: &RoaringBitmap) -> u64 {
    ids.iter()
        .filter_map(|id| reader.get_document(id))
        .map(|d| d.size)
        .sum()
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .context("interior_token_lab ROOT [LITERAL ...]")?;
    let root = Path::new(&root).canonicalize()?;
    let mut literals: Vec<_> = args.collect();
    if literals.is_empty() {
        literals = [
            "struct file_operations",
            "const struct file_operations",
            "file_operations",
            "static const struct",
            "static inline",
            "return -EINVAL",
            "unsigned long flags",
            "void __iomem",
            "if (unlikely(",
            "MODULE_DESCRIPTION(",
            "folio_wait_bit_common",
            "return",
            "auditNonexistentSymbol94283",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
    }
    let reader = IndexReader::open(&root)?; // Intentionally opens the existing token index.
    let mut rows = Vec::new();
    for literal in literals {
        ensure!(
            !literal.is_empty() && !literal.contains(['\r', '\n']),
            "nonempty line-local literal required"
        );
        let grams: Vec<_> = fxi::utils::query_trigrams(&literal)
            .into_iter()
            .filter(|g| !reader.is_stop_gram(*g))
            .collect();
        let baseline = if grams.is_empty() {
            reader.valid_doc_ids().clone()
        } else {
            reader.get_trigram_docs_with_bloom(&grams) & reader.valid_doc_ids()
        };
        let tokens = interior_tokens(&literal);
        let mut narrowed = baseline.clone();
        let mut token_frequencies = Vec::new();
        for (token, _) in &tokens {
            let postings = reader.get_token_docs(token)?;
            token_frequencies.push((token.clone(), postings.len()));
            narrowed &= postings;
        }
        let positional = reader.resolve_phrase_positional(&tokens, Some(&narrowed))?;
        let position_available = positional.is_some();
        let positional = positional.unwrap_or_else(|| narrowed.clone());
        let oracle = std::process::Command::new("rg")
            .args(["-l", "-F", "--null", "--color=never", "--", &literal, "."])
            .current_dir(&root)
            .output()?;
        ensure!(
            matches!(oracle.status.code(), Some(0 | 1)),
            "ripgrep failed: {}",
            String::from_utf8_lossy(&oracle.stderr)
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
        let original_paths = paths(&reader, &baseline);
        let token_paths = paths(&reader, &narrowed);
        let position_paths = paths(&reader, &positional);
        ensure!(
            expected.is_subset(&original_paths),
            "existing index misses {literal:?}; use an unchanged prepared corpus"
        );
        ensure!(
            expected.is_subset(&token_paths),
            "interior token filter lost true matches for {literal:?}: {:?}",
            expected.difference(&token_paths).collect::<Vec<_>>()
        );
        ensure!(
            expected.is_subset(&position_paths),
            "interior position filter lost true matches for {literal:?}: {:?}",
            expected.difference(&position_paths).collect::<Vec<_>>()
        );
        rows.push(serde_json::json!({"literal":literal,"interior_tokens":tokens,"token_document_frequencies":token_frequencies,
            "baseline_files":baseline.len(),"baseline_bytes":bytes(&reader,&baseline),
            "token_files":narrowed.len(),"token_bytes":bytes(&reader,&narrowed),
            "positional_files":positional.len(),"positional_bytes":bytes(&reader,&positional),
            "positional_data_used":position_available,"matching_files":expected.len()}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"root":root,"rows":rows,
        "method":"Only complete ASCII tokens with both boundaries inside an exact case-sensitive byte literal constrain candidates. Case folding, token truncation, and edge fragments cannot be treated as exact-token evidence. Token ordinals count all lexical tokens, including unindexed short/long ones. Matching files are independently checked with ripgrep; no true match may be removed.",
        "limits":"Offline precision probe using existing indexes, not a production optimization or timing benchmark. Forcing eager token loading may regress one-shot startup. Token positions constrain ordinal gaps, not byte distances or punctuation: surviving candidates still need source verification. Missing position files conservatively fall back to token-document filtering. Existing stale-index visibility limits remain; sources must match the index. No novelty claim."}))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn satisfies(source: &str, constraints: &[(String, u32)]) -> bool {
        let indexed = fxi::utils::extract_tokens_with_positions(source);
        let Some((first, offset)) = constraints.first() else {
            return true;
        };
        indexed
            .iter()
            .filter(|(token, _)| token == first)
            .any(|(_, position)| {
                constraints.iter().all(|(token, query_position)| {
                    let delta = query_position - offset;
                    position.checked_add(delta).is_some_and(|expected| {
                        indexed
                            .iter()
                            .any(|(term, pos)| term == token && *pos == expected)
                    })
                })
            })
    }

    #[test]
    fn endpoints_are_fragments_and_internal_boundaries_are_proven() {
        assert_eq!(
            interior_tokens("struct file_operations"),
            vec![("file".into(), 1)]
        );
        assert_eq!(
            interior_tokens("const struct file_operations"),
            vec![("struct".into(), 1), ("file".into(), 2)]
        );
        assert!(interior_tokens("static inline").is_empty());
        assert!(interior_tokens("file_operations").is_empty());
        assert_eq!(
            interior_tokens(" fooBar "),
            vec![("foo".into(), 0), ("bar".into(), 1)]
        );
        assert!(satisfies(
            "prefixdestruct file_operations_extra",
            &interior_tokens("struct file_operations")
        ));
        assert!(!satisfies(
            "const struct other file_operations",
            &interior_tokens("const struct file_operations")
        ));
    }

    #[test]
    fn omitted_short_and_long_tokens_keep_their_ordinal_gaps() {
        let literal = format!("edge before a {} after tail", "q".repeat(129));
        let constraints = interior_tokens(&literal);
        assert_eq!(constraints, vec![("before".into(), 1), ("after".into(), 4)]);
        assert!(satisfies(&format!("prefix{literal}suffix"), &constraints));
        assert_eq!(
            interior_tokens(&format!(" ({}) ", "q".repeat(128)))[0]
                .0
                .len(),
            128
        );
        assert!(interior_tokens(&format!(" ({}) ", "q".repeat(129))).is_empty());
    }

    #[test]
    fn every_unicode_boundary_substring_has_sound_interior_constraints() {
        let source = "pfooBar_baz XYZHttpRequest fooébar Kabc foo42BAR a q_tail";
        let boundaries: Vec<_> = source
            .char_indices()
            .map(|(p, _)| p)
            .chain(std::iter::once(source.len()))
            .collect();
        for (index, &start) in boundaries.iter().enumerate() {
            for &end in &boundaries[index + 1..] {
                let literal = &source[start..end];
                assert!(satisfies(source, &interior_tokens(literal)), "{literal:?}");
            }
        }
    }

    #[test]
    fn surrounding_bytes_never_change_interior_token_alignment() {
        for literal in [
            "struct file_operations",
            "aFooBar_bazTail",
            "XMLHttpRequest_Body",
            "éfoo_barλ",
            " AA foo42BAR ",
        ] {
            for prefix in ["", "a", "A", "42", "_", "é", "aA", "unrelated "] {
                for suffix in ["", "a", "A", "42", "_", "é"] {
                    assert!(
                        satisfies(
                            &format!("{prefix}{literal}{suffix}"),
                            &interior_tokens(literal)
                        ),
                        "{prefix:?} + {literal:?} + {suffix:?}"
                    );
                }
            }
        }
    }
}
