//! Compare short-pattern narrowing with an independent full source regex scan.
use fxi::index::{
    build::{build_index_with_options, update_index},
    reader::IndexReader,
};
use fxi::query::{QueryExecutor, parse_query};
use std::{fs, path::PathBuf};

fn check(root: &std::path::Path) {
    let reader = IndexReader::open(root).unwrap();
    for pattern in [
        "zx", "^zx", "zx$", "zx|ab", "zx|a", "zx?", "(?:zx)?", "[za]x", "(?i)zx", "(?i)ks", "é",
        "(?i)é", "ø", "ab.*zx", "zx.*ab", "aa", "z", "x.*zx",
    ] {
        let re = regex::Regex::new(pattern).unwrap();
        let mut expected = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "txt")
                && fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .any(|line| re.is_match(line))
            {
                expected.push(PathBuf::from(path.file_name().unwrap()));
            }
        }
        expected.sort();
        let mut query = parse_query(&format!("re:/{pattern}/"));
        query.options.limit = 0;
        let executor = QueryExecutor::new(&reader);
        assert_eq!(
            executor.execute_files_only(&query, 0).unwrap(),
            expected,
            "{pattern}"
        );
        // Candidate liveness and narrowing also apply to ordinary result output.
        let mut result_paths: Vec<_> = executor
            .execute(&query)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        result_paths.sort();
        result_paths.dedup();
        assert_eq!(result_paths, expected, "{pattern}");
    }
}

#[test]
fn byte_pairs_match_full_scan_at_boundaries_and_after_updates() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for (i, text) in [
        "zx", "zxa", "azx", "azxb", "z\nx", "z", "x", "ZX", "é", "É", "ézx", "zxé", "Kſ", "ks",
        "KS", "ø", "other", "ab\nzx", "abzx", "zxab", "aa", "aaa",
    ]
    .iter()
    .enumerate()
    {
        fs::write(root.join(format!("{i:03}.txt")), text).unwrap();
    }
    // Include every printable ASCII continuation, on each side of the pair.
    for byte in 32..=126u8 {
        fs::write(root.join(format!("prefix{byte}.txt")), [byte, b'z', b'x']).unwrap();
        fs::write(root.join(format!("suffix{byte}.txt")), [b'z', b'x', byte]).unwrap();
    }
    build_index_with_options(root, true, true, Some(17)).unwrap();
    check(root);
    fs::write(root.join("000.txt"), "unrelated").unwrap();
    fs::remove_file(root.join("001.txt")).unwrap();
    fs::write(root.join("002.txt"), "zx").unwrap();
    fs::write(root.join("new.txt"), "zxKſ").unwrap();
    update_index(root).unwrap();
    check(root);
    // The same narrowing must work with lean indexes, without token positions.
    fxi::index::build::build_index_with_profile(
        root,
        true,
        true,
        Some(17),
        fxi::index::types::IndexProfile::Lean,
    )
    .unwrap();
    check(root);
    let reader = IndexReader::open(root).unwrap();
    for pair in [*b"zx", *b"qq", *b"aa", [0xc3, 0xa9]] {
        let candidates = reader.get_byte_pair_docs(pair).unwrap();
        for id in reader.valid_doc_ids().iter() {
            let doc = reader.get_document(id).unwrap();
            let bytes = fs::read(root.join(reader.get_path(doc).unwrap())).unwrap();
            assert_eq!(
                candidates.contains(id),
                bytes.len() < 3 || bytes.windows(2).any(|window| window == pair)
            );
        }
    }
    drop(reader);
    // Legacy stop-grams require conservative fallback, even if the remaining
    // postings would give a plausible but incomplete candidate set.
    let index = fxi::utils::app_data::get_index_dir(root).unwrap();
    let meta_path = index.join("meta.json");
    let mut meta: fxi::index::types::IndexMeta =
        serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    meta.stop_grams = fxi::utils::query_trigrams("zxa");
    fs::write(meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    fxi::index::compact::merge_segments(root).unwrap();
    check(root);
    fxi::utils::app_data::remove_index(root).unwrap();
}
