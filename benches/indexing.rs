//! End-to-end indexing benchmarks using real repositories.
//!
//! Run with: `cargo bench`
//! Save baseline: `cargo bench -- --save-baseline main`
//! Compare: `cargo bench -- --baseline main`
//!
//! Requires repos at:
//!   - C:\git\wtg\Glow (~38k files)
//!   - C:\git\GitHub\WiseTechGlobal\CargoWise (~236k files)

use criterion::{criterion_group, criterion_main, Criterion};
use std::process::Command;
use std::path::Path;
use std::time::Duration;

const GLOW_PATH: &str = r"C:\git\GitHub\WiseTechGlobal\Glow";
const CARGOWISE_PATH: &str = r"C:\git\GitHub\WiseTechGlobal\CargoWise";

fn clear_index(repo_name: &str) {
    let index_pattern = format!(
        r"{}\fxi\indexes\{}-*",
        std::env::var("LOCALAPPDATA").unwrap(),
        repo_name
    );
    // Use PowerShell to remove index
    let _ = Command::new("powershell")
        .args(["-Command", &format!("Remove-Item -Recurse -Force '{}' -ErrorAction SilentlyContinue", index_pattern)])
        .output();
}

fn run_index(repo_path: &str) -> Duration {
    let start = std::time::Instant::now();
    
    let output = Command::new("vfp9")
        .args(["index", repo_path])
        .output()
        .expect("Failed to run vfp9 index");
    
    let elapsed = start.elapsed();
    
    if !output.status.success() {
        eprintln!("vfp9 index failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    
    elapsed
}

fn bench_glow(c: &mut Criterion) {
    if !Path::new(GLOW_PATH).exists() {
        eprintln!("Skipping Glow benchmark - path not found: {}", GLOW_PATH);
        return;
    }

    let mut group = c.benchmark_group("indexing");
    group.sample_size(10); // Fewer samples since each run is slow
    group.measurement_time(Duration::from_secs(300)); // Allow up to 5 min
    
    group.bench_function("glow_38k_files", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                clear_index("Glow");
                total += run_index(GLOW_PATH);
            }
            total
        })
    });
    
    group.finish();
}

fn bench_cargowise(c: &mut Criterion) {
    if !Path::new(CARGOWISE_PATH).exists() {
        eprintln!("Skipping CargoWise benchmark - path not found: {}", CARGOWISE_PATH);
        return;
    }

    let mut group = c.benchmark_group("indexing");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(600)); // Allow up to 10 min
    
    group.bench_function("cargowise_236k_files", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                clear_index("CargoWise");
                total += run_index(CARGOWISE_PATH);
            }
            total
        })
    });
    
    group.finish();
}

criterion_group!(benches, bench_glow, bench_cargowise);
criterion_main!(benches);
