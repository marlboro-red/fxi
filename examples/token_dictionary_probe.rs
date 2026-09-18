//! Measure eager index opening and public token APIs on a fixed corpus.
//! Result fingerprints sort paths, making them independent of assigned doc IDs.
use fxi::index::reader::IndexReader;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::time::Instant;

fn main() {
    let root = std::env::args()
        .nth(1)
        .expect("token_dictionary_probe ROOT");
    let start = Instant::now();
    let reader = IndexReader::open(Path::new(&root)).expect("open index");
    println!("open_ms\t{}", start.elapsed().as_secs_f64() * 1000.0);
    for contains in [false, true] {
        for token in ["operations", "folio", "return", "nonexistentmarker94283"] {
            // Warm the thread pool and this API path before measuring lookup.
            let mut elapsed = Vec::new();
            let mut fingerprint = None;
            let mut count = 0;
            for repetition in 0..12 {
                let start = Instant::now();
                let docs = if contains {
                    reader.get_token_docs_containing(token)
                } else {
                    reader.get_token_docs(token)
                };
                let milliseconds = start.elapsed().as_secs_f64() * 1000.0;
                let mut paths: Vec<_> = docs
                    .iter()
                    .map(|id| {
                        reader
                            .get_path(reader.get_document(id).expect("document"))
                            .expect("path")
                    })
                    .collect();
                paths.sort_unstable();
                let mut hasher = DefaultHasher::new();
                paths.hash(&mut hasher);
                let actual = hasher.finish();
                if let Some(expected) = fingerprint {
                    assert_eq!(actual, expected);
                }
                fingerprint = Some(actual);
                count = paths.len();
                if repetition > 0 {
                    elapsed.push(milliseconds);
                }
            }
            elapsed.sort_by(f64::total_cmp);
            println!(
                "query\t{contains}\t{token}\t{count}\t{}\t{}",
                fingerprint.unwrap(),
                elapsed[elapsed.len() / 2]
            );
        }
    }
}
