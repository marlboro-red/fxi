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
