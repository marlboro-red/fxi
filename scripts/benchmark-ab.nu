#!/usr/bin/env nu
# A/B Benchmark Script for fxi
# Compares indexing performance between two branches
#
# Usage: nu scripts/benchmark-ab.nu --baseline main --test perf2 --repo Glow
#
# Note: Copies release binary to vfp9.exe location to bypass Windows Defender scanning
#
# Tip: Set FXI_INDEXES to a Dev Drive path for faster I/O:
#   $env.FXI_INDEXES = "D:\fxi\indexes"

def main [
    --baseline: string = "main"   # Baseline branch to compare against
    --test: string = "perf2"      # Test branch with changes
    --repo: string = "Glow"       # Repository to benchmark (Glow or CargoWise)
    --iterations: int = 3         # Number of benchmark iterations
] {
    let repos = {
        Glow: 'C:\git\GitHub\WiseTechGlobal\Glow'
        CargoWise: 'C:\git\GitHub\WiseTechGlobal\CargoWise'
    }

    let vfp9_path = (which vfp9 | get 0.path)
    let repo_path = ($repos | get -o $repo)

    if $repo_path == null {
        print $"(ansi red)Unknown repo: ($repo). Available: ($repos | columns | str join ', ')(ansi reset)"
        exit 1
    }

    if not ($repo_path | path exists) {
        print $"(ansi red)Repo path not found: ($repo_path)(ansi reset)"
        exit 1
    }

    print $"(ansi yellow)=== fxi A/B Benchmark ===(ansi reset)"
    print $"Baseline: ($baseline)"
    print $"Test:     ($test)"
    print $"Repo:     ($repo) \(($repo_path)\)"
    print $"Iterations: ($iterations)"

    let original_branch = (git rev-parse --abbrev-ref HEAD | str trim)

    # Run test branch first
    build-and-deploy $test $vfp9_path
    print $"\n(ansi cyan)=== Benchmarking ($test) ===(ansi reset)"
    let test_results = (run-benchmark $test $repo_path $repo $iterations $vfp9_path)

    # Run baseline after
    build-and-deploy $baseline $vfp9_path
    print $"\n(ansi cyan)=== Benchmarking ($baseline) ===(ansi reset)"
    let baseline_results = (run-benchmark $baseline $repo_path $repo $iterations $vfp9_path)

    # Restore original branch
    git checkout $original_branch err+out>| ignore

    # Results
    print $"\n(ansi yellow)=== Results ===(ansi reset)"
    let results = [
        { Branch: $baseline_results.label, Avg: $"($baseline_results.avg | into string -d 2)s", Min: $"($baseline_results.min | into string -d 2)s", Max: $"($baseline_results.max | into string -d 2)s" }
        { Branch: $test_results.label, Avg: $"($test_results.avg | into string -d 2)s", Min: $"($test_results.min | into string -d 2)s", Max: $"($test_results.max | into string -d 2)s" }
    ]
    print ($results | table)

    let diff = $test_results.avg - $baseline_results.avg
    let pct = ($diff / $baseline_results.avg) * 100

    if $diff < 0 {
        let diff_abs = $diff * -1
        let pct_abs = $pct * -1
        print $"\n(ansi green)Improvement: ($diff_abs | into string -d 2)s faster (($pct_abs | into string -d 1)%)(ansi reset)"
    } else {
        print $"\n(ansi red)Regression: ($diff | into string -d 2)s slower (($pct | into string -d 1)%)(ansi reset)"
    }
}

def build-and-deploy [branch: string, vfp9_path: string] {
    print $"\n(ansi cyan)=== Building ($branch) ===(ansi reset)"
    git checkout $branch err+out>| ignore
    if $env.LAST_EXIT_CODE != 0 {
        print $"(ansi red)Failed to checkout ($branch)(ansi reset)"
        exit 1
    }

    cargo build --release err+out>| ignore
    if $env.LAST_EXIT_CODE != 0 {
        print $"(ansi red)Failed to build ($branch)(ansi reset)"
        exit 1
    }

    cp ./target/release/fxi.exe $vfp9_path
    print $"(ansi green)Deployed ($branch) to ($vfp9_path)(ansi reset)"
}

def clear-index [repo_name: string] {
    let indexes_dir = if ($env.FXI_INDEXES? != null) { $env.FXI_INDEXES } else { [$env.LOCALAPPDATA, "fxi", "indexes"] | path join }
    if ($indexes_dir | path exists) {
        ls $indexes_dir | where name =~ $"($repo_name)-" | each { |p| rm -rf $p.name } | ignore
    }
}

def run-benchmark [label: string, repo_path: string, repo_name: string, iterations: int, vfp9_path: string] {
    mut times = []

    for i in 1..$iterations {
        # Clear index for cold start
        clear-index $repo_name

        # Run indexing and measure time
        let start = (date now)
        ^$vfp9_path index $repo_path err+out>| ignore
        let elapsed = ((date now) - $start) | into int | $in / 1_000_000_000

        $times = ($times | append $elapsed)
        print $"  Run ($i)/($iterations)... ($elapsed | into string -d 2)s"
    }

    let avg = ($times | math avg)
    let min = ($times | math min)
    let max = ($times | math max)

    { label: $label, times: $times, avg: $avg, min: $min, max: $max }
}
