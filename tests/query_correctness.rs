use fxi::index::{build::build_index_with_progress, reader::IndexReader};
use fxi::query::{QueryExecutor, parse_query};
use std::collections::BTreeSet;
use std::fs;

fn check_files(query: &str, expected: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in [
        ("a.txt", "needle\n"),
        ("b.txt", "x\n"),
        ("c.txt", "other\n"),
        ("d.txt", "needle x\n"),
    ] {
        fs::write(dir.path().join(name), content).unwrap();
    }
    build_index_with_progress(dir.path(), true, true).unwrap();
    let reader = IndexReader::open(dir.path()).unwrap();
    let executor = QueryExecutor::new(&reader);
    let query = parse_query(query);
    let expected: BTreeSet<_> = expected.iter().map(std::path::PathBuf::from).collect();
    assert_eq!(
        executor
            .execute_files_only(&query, 0)
            .unwrap()
            .into_iter()
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(
        executor
            .execute_with_content(&query, 0, 0)
            .unwrap()
            .into_iter()
            .map(|m| m.path)
            .collect::<BTreeSet<_>>(),
        expected
    );
    drop(reader);
    fxi::utils::remove_index(dir.path()).unwrap();
}

#[test]
fn or_includes_branches_without_index_constraints() {
    for query in [
        "needle | x",
        "x | needle",
        "needle | re:/[x]/",
        "(needle | x) -other",
    ] {
        check_files(query, &["a.txt", "b.txt", "d.txt"]);
    }
}

#[test]
fn files_only_obeys_line_regex_and_filter_semantics() {
    check_files("re:/^needle$/", &["a.txt"]);
    check_files("re:/^x$/", &["b.txt"]);
    check_files("needle line:20-30", &[]);
    check_files("needle line:1-1", &["a.txt", "d.txt"]);
    check_files("needle -re:/^x$/", &["a.txt", "d.txt"]);
}

#[test]
fn invalid_regex_is_an_error_even_with_no_candidates() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "unrelated").unwrap();
    build_index_with_progress(dir.path(), true, true).unwrap();
    let reader = IndexReader::open(dir.path()).unwrap();
    let executor = QueryExecutor::new(&reader);
    for input in ["re:/[/", "absent re:/[/", "absent | re:/[/", "-re:/[/"] {
        let query = parse_query(input);
        assert!(executor.execute(&query).is_err(), "{input}");
        assert!(executor.execute_files_only(&query, 0).is_err(), "{input}");
        assert!(
            executor.execute_with_content(&query, 0, 0).is_err(),
            "{input}"
        );
    }
    drop(reader);
    fxi::utils::remove_index(dir.path()).unwrap();
}

#[test]
fn substring_candidates_are_a_superset_of_unicode_matches() {
    let dir = tempfile::tempdir().unwrap();
    let corpus = [
        "PREFIXNEEDLESUFFIX",
        "foobar bazqux",
        "K foo Σ ς σ",
        "vector::start",
        "needle",
        "unrelated",
    ];
    for (i, text) in corpus.iter().enumerate() {
        fs::write(dir.path().join(format!("f{i}.txt")), text).unwrap();
    }
    build_index_with_progress(dir.path(), true, true).unwrap();
    let reader = IndexReader::open(dir.path()).unwrap();
    let executor = QueryExecutor::new(&reader);
    for literal in ["need", "oo", "k", "σ", "bar baz", "r::st", "foo", "absent"] {
        for phrase in [false, true] {
            // Bare terms with spaces are file-level AND, so test those only as phrases.
            if !phrase && literal.contains(' ') {
                continue;
            }
            for insensitive in [false, true] {
                let pattern = if insensitive || !phrase {
                    format!("(?i:{})", regex::escape(literal))
                } else {
                    regex::escape(literal)
                };
                let oracle = regex::Regex::new(&pattern).unwrap();
                let expected: BTreeSet<_> = corpus
                    .iter()
                    .enumerate()
                    .filter(|(_, text)| oracle.is_match(text))
                    .map(|(i, _)| std::path::PathBuf::from(format!("f{i}.txt")))
                    .collect();
                let mut query = parse_query(&if phrase {
                    format!("\"{literal}\"")
                } else {
                    literal.into()
                });
                query.options.case_insensitive = insensitive;
                let actual: BTreeSet<_> = executor
                    .execute_files_only(&query, 0)
                    .unwrap()
                    .into_iter()
                    .collect();
                assert_eq!(
                    actual, expected,
                    "{literal}, phrase={phrase}, insensitive={insensitive}"
                );
                let actual: BTreeSet<_> = executor
                    .execute_with_content(&query, 0, 0)
                    .unwrap()
                    .into_iter()
                    .map(|hit| hit.path)
                    .collect();
                assert_eq!(actual, expected);
            }
        }
    }
    drop(reader);
    fxi::utils::remove_index(dir.path()).unwrap();
}

#[test]
fn ranked_filename_hits_respect_filters_boolean_terms_and_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in [
        ("needle.md", "other"),
        ("needle.rs", "forbidden"),
        ("needle_other.txt", "extra"),
    ] {
        fs::write(dir.path().join(name), text).unwrap();
    }
    build_index_with_progress(dir.path(), true, true).unwrap();
    let reader = IndexReader::open(dir.path()).unwrap();
    let executor = QueryExecutor::new(&reader);
    for (input, expected) in [
        ("ext:rs needle", vec!["needle.rs"]),
        ("needle -forbidden", vec!["needle.md", "needle_other.txt"]),
        ("needle missing", vec![]),
        ("needle line:2-5", vec![]),
        (
            "needle top:0",
            vec!["needle.md", "needle.rs", "needle_other.txt"],
        ),
    ] {
        let actual: BTreeSet<_> = executor
            .execute(&parse_query(input))
            .unwrap()
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        let expected: BTreeSet<_> = expected.into_iter().map(std::path::PathBuf::from).collect();
        assert_eq!(actual, expected, "{input}");
    }
    drop(reader);
    fxi::utils::remove_index(dir.path()).unwrap();
}

#[test]
fn ranked_limit_is_a_prefix_of_the_complete_ranking() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..30 {
        fs::write(
            dir.path().join(format!("f{i}.txt")),
            "needle\n".repeat(i + 1),
        )
        .unwrap();
    }
    build_index_with_progress(dir.path(), true, true).unwrap();
    let reader = IndexReader::open(dir.path()).unwrap();
    let executor = QueryExecutor::new(&reader);
    for sort in ["score", "path", "recency"] {
        let all = executor
            .execute(&parse_query(&format!("needle top:0 sort:{sort}")))
            .unwrap();
        let limited = executor
            .execute(&parse_query(&format!("needle top:3 sort:{sort}")))
            .unwrap();
        assert_eq!(
            limited
                .iter()
                .map(|m| (&m.path, m.line_number))
                .collect::<Vec<_>>(),
            all.iter()
                .take(3)
                .map(|m| (&m.path, m.line_number))
                .collect::<Vec<_>>()
        );
    }
    drop(reader);
    fxi::utils::remove_index(dir.path()).unwrap();
}
