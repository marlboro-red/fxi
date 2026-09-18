//! Optimistic query-selected boundary-posting prototype on an immutable fixture.
//! This is not a general index builder or the production query/API path.
use anyhow::{Context, Result, ensure};
use fxi::index::reader::IndexReader;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let root =
        Path::new(&args.next().context("phrase_filter_lab ROOT LITERAL...")?).canonicalize()?;
    let literals: Vec<_> = args.collect();
    ensure!(!literals.is_empty(), "provide literals");
    let reader = IndexReader::open(&root)?;
    let pool = rayon::ThreadPoolBuilder::new().num_threads(8).build()?;
    let mut rows = Vec::new();
    for literal in literals {
        ensure!(
            literal.is_ascii() && literal.len() >= 8 && !literal.contains(['\r', '\n']),
            "ASCII line-local literals, >=8 bytes"
        );
        let grams: Vec<_> = fxi::utils::query_trigrams(&literal)
            .into_iter()
            .filter(|g| !reader.is_stop_gram(*g))
            .collect();
        ensure!(!grams.is_empty(), "need trigram candidates");
        let baseline = reader.get_trigram_docs_with_bloom(&grams)? & reader.valid_doc_ids();
        let lowered = literal.to_ascii_lowercase();
        let keys: BTreeSet<Vec<u8>> = lowered
            .as_bytes()
            .windows(8)
            .filter(|g| {
                g.iter().any(u8::is_ascii_alphanumeric)
                    && g.iter().any(|b| !b.is_ascii_alphanumeric())
            })
            .map(<[u8]>::to_vec)
            .collect();
        ensure!(!keys.is_empty(), "need boundary-crossing grams");
        // Optimistically construct only the current query's grams, only inside
        // its trigram candidate set. This does NOT measure a complete index cost.
        let build_start = Instant::now();
        let mut selected = BTreeMap::<Vec<u8>, RoaringBitmap>::new();
        for key in keys {
            selected.insert(key, RoaringBitmap::new());
        }
        let mut bytes_read = 0usize;
        for id in baseline.iter() {
            let doc = reader.get_document(id).context("missing document")?;
            let path = reader.get_full_path(doc).context("missing path")?;
            let bytes = std::fs::read(path)?.to_ascii_lowercase();
            bytes_read += bytes.len();
            for (key, postings) in &mut selected {
                if memchr::memmem::find(&bytes, key).is_some() {
                    postings.insert(id);
                }
            }
        }
        let build_ms = build_start.elapsed().as_secs_f64() * 1000.0;
        // A hypothetical key + length + delta-varint representation; no header,
        // checksum, generation binding or dictionary-allocation cost included.
        let mut packed_bytes = 0usize;
        for postings in selected.values() {
            let mut bytes = Vec::new();
            fxi::utils::delta_encode(&postings.iter().collect::<Vec<_>>(), &mut bytes);
            packed_bytes += 8 + 8 + bytes.len();
        }
        let mut posting_lists: Vec<_> = selected.values().collect();
        posting_lists.sort_by_key(|p| p.len());
        let expected = std::process::Command::new("rg")
            .args(["-l", "-F", "--null", "--color=never", "--", &literal, "."])
            .current_dir(&root)
            .output()?;
        ensure!(matches!(expected.status.code(), Some(0 | 1)), "rg failed");
        let expected: BTreeSet<PathBuf> = std::str::from_utf8(&expected.stdout)?
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(|p| PathBuf::from(p.trim_start_matches("./")))
            .collect();
        let finder = memchr::memmem::Finder::new(literal.as_bytes());
        let mut samples = [Vec::new(), Vec::new()];
        let mut final_candidates = 0;
        for rep in 0..23 {
            for which in if rep % 2 == 0 { [0, 1] } else { [1, 0] } {
                let start = Instant::now();
                let mut ids = reader.get_trigram_docs_with_bloom(&grams)? & reader.valid_doc_ids();
                if which == 1 {
                    for postings in &posting_lists {
                        ids &= *postings;
                    }
                    final_candidates = ids.len();
                }
                let lookup_ms = start.elapsed().as_secs_f64() * 1000.0;
                let ids: Vec<_> = ids.iter().collect();
                let mut found: Vec<_> = pool.install(|| {
                    ids.par_iter()
                        .with_min_len((ids.len() / 8).max(1))
                        .filter_map(|id| {
                            let doc = reader.get_document(*id)?;
                            let full = reader.get_full_path(doc)?;
                            // Both paths use precisely the same live metadata-validated
                            // source cache and prepared literal verifier.
                            let source = reader.read_file_cached(&full)?;
                            finder
                                .find(source.as_bytes())
                                .map(|_| reader.get_path(doc).unwrap().clone())
                        })
                        .collect()
                });
                found.sort_unstable();
                let total_ms = start.elapsed().as_secs_f64() * 1000.0;
                ensure!(
                    found.len() == expected.len()
                        && found.iter().cloned().collect::<BTreeSet<_>>() == expected,
                    "file-set mismatch; fixture must remain immutable"
                );
                if rep >= 2 {
                    samples[which].push(serde_json::json!({"total_ms":total_ms,"lookup_ms":lookup_ms,"verify_and_sort_ms":total_ms-lookup_ms}));
                }
            }
        }
        rows.push(serde_json::json!({"literal":literal,"before_candidates":baseline.len(),"after_candidates":final_candidates,"matching_files":expected.len(),"selected_grams":selected.len(),"selected_posting_packed_bytes":packed_bytes,"selected_build_ms":build_ms,"source_bytes_read":bytes_read,"before":samples[0],"after":samples[1]}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"root":root,"threads":8,"rows":rows,"limits":"Optimistic query-selected postings on one immutable fixture, derived from live bytes outside timing. Not a persistent safe index, not general build/storage cost, not production API speed. Both timed paths use identical prepared literal verification and live metadata validation; excluded docs are not checked, so fixture changes invalidate experiment. Two warmups then 21 alternating-order samples, exact rg parity each sample."})
        )?
    );
    Ok(())
}
