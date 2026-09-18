use fxi::index::{build::build_index_with_progress, reader::IndexReader};
use fxi::query::{QueryExecutor, parse_query};
use std::collections::BTreeSet;
use std::fs;

#[test]
fn empty_insensitive_phrase_does_not_invent_a_line_after_final_newline() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in [
        ("lf.txt", "alpha\n"),
        ("crlf.txt", "alpha\r\n"),
        ("none.txt", "alpha"),
    ] {
        fs::write(dir.path().join(name), text).unwrap();
    }
    build_index_with_progress(dir.path(), true, true).unwrap();
    let reader = IndexReader::open(dir.path()).unwrap();
    let executor = QueryExecutor::new(&reader);
    for insensitive in [false, true] {
        let mut query = parse_query("\"\"");
        query.options.case_insensitive = insensitive;
        let hits = executor.execute_with_content(&query, 1, 1).unwrap();
        assert_eq!(hits.len(), 3);
        for hit in hits {
            assert_eq!(hit.line_number, 1);
            assert_eq!(hit.line_content, "alpha");
            assert_eq!((hit.match_start, hit.match_end), (0, 0));
            assert!(hit.context_before.is_empty() && hit.context_after.is_empty());
        }
        assert!(
            executor
                .execute_match_counts(&query, 0)
                .unwrap()
                .iter()
                .all(|(_, count)| *count == 1)
        );
    }
    drop(reader);
    fxi::utils::remove_index(dir.path()).unwrap();
}

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
    fxi::utils::app_data::isolate_test_storage().unwrap();
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
    fxi::utils::app_data::isolate_test_storage().unwrap();
    check_files("re:/^needle$/", &["a.txt"]);
    check_files("re:/^x$/", &["b.txt"]);
    check_files("needle line:20-30", &[]);
    check_files("needle line:1-1", &["a.txt", "d.txt"]);
    check_files("needle -re:/^x$/", &["a.txt", "d.txt"]);
}

#[test]
fn invalid_regex_is_an_error_even_with_no_candidates() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
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
    fxi::utils::app_data::isolate_test_storage().unwrap();
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
    fxi::utils::app_data::isolate_test_storage().unwrap();
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
    fxi::utils::app_data::isolate_test_storage().unwrap();
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

#[test]
fn hir_candidates_match_brute_force_regex_across_deltas_and_compaction() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut corpus: Vec<String> = [
        "needle",
        "NEEDLE",
        "fooBAR",
        "aBc",
        "xyz",
        "x",
        "",
        "Kelvin",
        "kelvin",
        "Σigma",
        "ςIGMA",
        "σigma",
        "İstanbul",
        "long_identifier_ABC_xyz",
        "aabcc",
        "foo\nbar",
        "needle suffix",
        "prefix other",
        "static VOID",
        "abcdabcd",
    ]
    .iter()
    .map(|s| (*s).into())
    .collect();
    for a in ["a", "A", "K", "k", "σ", "ς", "foo", "needle"] {
        for b in ["bc", "BC", "foo", "bar", "xyz", "other"] {
            corpus.push(format!("prefix {a}{b} suffix\n{b}{a}"));
        }
    }
    for (i, text) in corpus.iter().enumerate() {
        fs::write(dir.path().join(format!("f{i}.txt")), text).unwrap();
    }
    build_index_with_progress(dir.path(), true, true).unwrap();
    let mut patterns: Vec<String> = [
        "needle|other",
        "x|needle",
        ".*needle",
        "needle.*suffix",
        "^foo$",
        "foo$|^bar",
        "(?:needle)?",
        "(?:ab){2}",
        "(?:ab){0,2}c",
        "(?:abc)+",
        "(?:ab|xy)c",
        "[aA]bc",
        "a[bB][cC]",
        "(?i)kelvin",
        "(?i)σigma",
        "(?i)static void",
        "(?i)long_identifier_abc_xyz",
        "(?i)needle(?-i:XYZ)?",
        "\\bneedle\\b",
        "[a-z]+foo",
        "[^x]*bar",
        "(?:foo|bar){2}",
        "(?x) n e e d l e #comment",
        "[[:alpha:]]+",
        "a{1000}",
        "(?i:ab){12}",
        "(?:abc|)",
        "a?bc",
        "food*",
        "(?i)kbc",
        "(?i)σbc",
        "(?:a|b|c|d|e|f){10}",
        "\\x{212a}elvin",
    ]
    .iter()
    .map(|s| (*s).into())
    .collect();
    for atom in ["abc", "[aA]bc", "(?i:foo)", "(?:needle|x)", "[a-z]"] {
        for suffix in ["", "?", "+", "*", "{0,2}", "{2}"] {
            patterns.push(format!("(?:{atom}){suffix}"));
            patterns.push(format!("prefix.*(?:{atom}){suffix}.*suffix"));
        }
    }
    for stage in ["initial", "delta", "compact"] {
        if stage == "delta" {
            corpus[0] = "NEEDLE extra suffix".into();
            fs::write(dir.path().join("f0.txt"), &corpus[0]).unwrap();
            fxi::index::build::update_index(dir.path()).unwrap();
        }
        if stage == "compact" {
            fxi::index::compact::compact_segments(dir.path()).unwrap();
        }
        let reader = IndexReader::open(dir.path()).unwrap();
        let executor = QueryExecutor::new(&reader);
        for pattern in &patterns {
            let oracle = regex::Regex::new(pattern).unwrap();
            let expected: BTreeSet<_> = corpus
                .iter()
                .enumerate()
                .filter(|(_, text)| text.lines().any(|line| oracle.is_match(line)))
                .map(|(i, _)| std::path::PathBuf::from(format!("f{i}.txt")))
                .collect();
            let query = fxi::query::Query {
                root: fxi::query::QueryNode::Regex(pattern.clone()),
                ..parse_query("")
            };
            let actual: BTreeSet<_> = executor
                .execute_files_only(&query, 0)
                .unwrap()
                .into_iter()
                .collect();
            assert_eq!(actual, expected, "{pattern}, stage={stage}");
            let actual: BTreeSet<_> = executor
                .execute_with_content(&query, 0, 0)
                .unwrap()
                .into_iter()
                .map(|m| m.path)
                .collect();
            assert_eq!(actual, expected, "content {pattern}, stage={stage}");
        }
    }
    fxi::utils::remove_index(dir.path()).unwrap();
}

#[test]
fn parallel_cached_verification_observes_rewrites_and_deletions() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for i in 0..128 {
        fs::write(dir.path().join(format!("f{i}.txt")), "cached_needle\n").unwrap();
    }
    build_index_with_progress(dir.path(), true, true).unwrap();
    let reader = IndexReader::open(dir.path()).unwrap();
    let executor = QueryExecutor::new(&reader);
    let query = parse_query("cached_needle");
    for _ in 0..2 {
        assert_eq!(executor.execute_files_only(&query, 0).unwrap().len(), 128);
    }
    for i in 0..128 {
        let path = dir.path().join(format!("f{i}.txt"));
        if i % 2 == 0 {
            fs::remove_file(path).unwrap();
        } else {
            fs::write(path, "other_content\n").unwrap();
        }
    }
    assert!(executor.execute_files_only(&query, 0).unwrap().is_empty());
    assert!(
        executor
            .execute_with_content(&query, 0, 0)
            .unwrap()
            .is_empty()
    );
    drop(reader);
    fxi::utils::remove_index(dir.path()).unwrap();
}

#[test]
fn boosts_preserve_phrase_case_and_only_affect_matching_lines() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "needle\nother\nNEEDLE\n").unwrap();
    build_index_with_progress(dir.path(), true, true).unwrap();
    let reader = IndexReader::open(dir.path()).unwrap();
    let executor = QueryExecutor::new(&reader);
    let phrase = parse_query("^3:\"needle\" top:0");
    let hits = executor.execute_with_content(&phrase, 0, 0).unwrap();
    assert_eq!(
        hits.iter().map(|h| h.line_number).collect::<Vec<_>>(),
        vec![1]
    );
    let baseline = executor
        .execute(&parse_query("needle | other top:0"))
        .unwrap();
    let boosted = executor
        .execute(&parse_query("^3:needle | other top:0"))
        .unwrap();
    let score = |results: &[fxi::index::types::SearchMatch], line| {
        results
            .iter()
            .find(|m| m.line_number == line)
            .unwrap()
            .score
    };
    assert_eq!(score(&baseline, 2), score(&boosted, 2));
    assert!((score(&boosted, 1) / score(&baseline, 1) - 3.0).abs() < 0.001);
    let mut insensitive = phrase;
    insensitive.options.case_insensitive = true;
    assert_eq!(
        executor
            .execute_with_content(&insensitive, 0, 0)
            .unwrap()
            .len(),
        2
    );
    drop(reader);
    fxi::utils::remove_index(dir.path()).unwrap();
}

#[test]
fn compound_regex_candidates_are_sound_across_segment_partitions() {
    fxi::utils::app_data::isolate_test_storage().unwrap();
    let sources = [
        "FoObAr\n",
        "foobaz\n",
        "x\n",
        "other\n",
        "prefixFOOBARsuffix\n",
        "foo bar\n",
        "Kelvin\n",
        "kelvin\n",
        "foobar fooquux\n",
    ];
    let patterns = [
        "(?i)foobar",
        "(?i)foo(bar|baz)",
        "(?:foobar|x?)",
        "foobar|kelvin",
        "(?i)kelvin",
        "foo.*(?:bar|quux)",
        "(?i)foob(a|u)r|kelvin",
    ];
    let dir = tempfile::tempdir().unwrap();
    for (id, source) in sources.iter().enumerate() {
        fs::write(dir.path().join(format!("{id}.txt")), source).unwrap();
    }
    for chunk in [1, 3, 0] {
        fxi::index::build::build_index_with_options(dir.path(), true, true, Some(chunk)).unwrap();
        let reader = IndexReader::open(dir.path()).unwrap();
        for pattern in patterns {
            let regex = regex::Regex::new(pattern).unwrap();
            let expected: BTreeSet<_> = sources
                .iter()
                .enumerate()
                .filter(|(_, source)| source.lines().any(|line| regex.is_match(line)))
                .map(|(id, _)| std::path::PathBuf::from(format!("{id}.txt")))
                .collect();
            let actual = QueryExecutor::new(&reader)
                .execute_files_only(&parse_query(&format!("re:/{pattern}/")), 0)
                .unwrap();
            assert_eq!(
                actual.into_iter().collect::<BTreeSet<_>>(),
                expected,
                "{pattern}, chunk={chunk}"
            );
        }
    }
    let _ = fxi::utils::remove_index(dir.path());
}
