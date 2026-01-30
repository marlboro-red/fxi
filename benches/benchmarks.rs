//! Performance benchmarks for FXI
//!
//! Run with: cargo bench

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Create a test directory with sample files for benchmarking
fn create_benchmark_fixtures() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let root_path = temp_dir.path().to_path_buf();

    // Initialize git repo
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&root_path)
        .output()
        .expect("Failed to init git repo");

    // Create multiple test files of varying sizes
    for i in 0..50 {
        let content = format!(
            r#"// File {i}
fn function_{i}() {{
    println!("Hello from function {i}");
    let x = {i} * 2;
    let y = x + 1;
}}

struct Struct{i} {{
    field: i32,
    name: String,
}}

impl Struct{i} {{
    fn new() -> Self {{
        Self {{ field: {i}, name: "test".to_string() }}
    }}
}}
"#,
            i = i
        );
        fs::write(root_path.join(format!("file_{}.rs", i)), content)
            .expect("Failed to write file");
    }

    // Build the index
    fxi::index::build::build_index(&root_path, false).expect("Failed to build index");

    (temp_dir, root_path)
}

fn bench_query_parsing(c: &mut Criterion) {
    let queries = vec![
        "simple",
        "two words",
        "\"exact phrase\"",
        "ext:rs fn",
        "lang:rust struct",
        "path:src/*.rs impl",
        "re:/\\d+/",
        "complex AND (query OR search) -exclude",
    ];

    let mut group = c.benchmark_group("query_parsing");
    for query in queries {
        group.bench_with_input(
            BenchmarkId::from_parameter(query),
            &query,
            |b, &q| {
                b.iter(|| fxi::query::parse_query(black_box(q)))
            },
        );
    }
    group.finish();
}

fn bench_trigram_extraction(c: &mut Criterion) {
    let small_content = b"fn main() { println!(\"hello\"); }";
    let medium_content = small_content.repeat(100);
    let large_content = small_content.repeat(1000);

    let mut group = c.benchmark_group("trigram_extraction");

    group.bench_function("small_32b", |b| {
        b.iter(|| fxi::utils::extract_trigrams(black_box(small_content)))
    });

    group.bench_function("medium_3kb", |b| {
        b.iter(|| fxi::utils::extract_trigrams(black_box(&medium_content)))
    });

    group.bench_function("large_32kb", |b| {
        b.iter(|| fxi::utils::extract_trigrams(black_box(&large_content)))
    });

    group.finish();
}

fn bench_token_extraction(c: &mut Criterion) {
    let code = r#"
        fn getUserById(userId: i32) -> Option<User> {
            let user_name = "test_user";
            let HTTPResponseCode = 200;
            some_function_call(arg1, arg2);
        }
    "#;

    c.bench_function("token_extraction", |b| {
        b.iter(|| fxi::utils::extract_tokens(black_box(code)))
    });
}

fn bench_search(c: &mut Criterion) {
    let (_temp_dir, root_path) = create_benchmark_fixtures();
    let reader = fxi::index::reader::IndexReader::open(&root_path)
        .expect("Failed to open index");

    let mut group = c.benchmark_group("search");

    // Simple single-word search
    group.bench_function("simple_word", |b| {
        let query = fxi::query::parse_query("function");
        b.iter(|| {
            let executor = fxi::query::QueryExecutor::new(&reader);
            executor.execute(black_box(&query))
        })
    });

    // Phrase search
    group.bench_function("phrase", |b| {
        let query = fxi::query::parse_query("\"Hello from\"");
        b.iter(|| {
            let executor = fxi::query::QueryExecutor::new(&reader);
            executor.execute(black_box(&query))
        })
    });

    // Extension filter
    group.bench_function("ext_filter", |b| {
        let query = fxi::query::parse_query("ext:rs struct");
        b.iter(|| {
            let executor = fxi::query::QueryExecutor::new(&reader);
            executor.execute(black_box(&query))
        })
    });

    // Regex search
    group.bench_function("regex", |b| {
        let query = fxi::query::parse_query("re:/Struct\\d+/");
        b.iter(|| {
            let executor = fxi::query::QueryExecutor::new(&reader);
            executor.execute(black_box(&query))
        })
    });

    group.finish();
}

fn bench_index_reading(c: &mut Criterion) {
    let (_temp_dir, root_path) = create_benchmark_fixtures();

    c.bench_function("index_open", |b| {
        b.iter(|| {
            fxi::index::reader::IndexReader::open(black_box(&root_path))
        })
    });
}

criterion_group!(
    benches,
    bench_query_parsing,
    bench_trigram_extraction,
    bench_token_extraction,
    bench_search,
    bench_index_reading,
);

criterion_main!(benches);
