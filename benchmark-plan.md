# fxi Indexing Performance Work

## Phase 1: Build a Reliable Benchmark (DO THIS FIRST)

### Tasks
- [x] Add `criterion` to Cargo.toml
- [x] Create `benches/indexing.rs` with real repo benchmarks
- [ ] Build release and copy to vfp9: `cargo build --release && copy "target\release\fxi.exe" "C:\Program Files (x86)\Microsoft Visual FoxPro 9\vfp9.exe"`
- [ ] Verify benchmark compiles: `cargo bench --no-run`
- [ ] Run benchmark to establish baseline: `cargo bench -- --save-baseline main`

### Benchmark Design
- Uses real repos: **Glow** (~38k files) and **CargoWise** (~236k files)
- Runs `vfp9 index` end-to-end (full indexing pipeline)
- Clears index between runs for cold-start measurement
- Criterion handles warmup, statistics, and comparison

### Usage
```bash
# Build without running (verify it compiles)
cargo bench --no-run

# Run benchmark and save as baseline
cargo bench -- --save-baseline main

# After making changes, compare to baseline
cargo bench -- --baseline main

# Run only Glow (faster iteration)
cargo bench -- glow
```

---

## Phase 2: Establish Baseline

- [ ] Build release and copy to vfp9: `cargo build --release && copy "target\release\fxi.exe" "C:\Program Files (x86)\Microsoft Visual FoxPro 9\vfp9.exe"`
- [ ] Run `cargo bench -- --save-baseline main` on current code
- [ ] Record baseline numbers in this file
- [ ] Commit baseline

### Baseline Results
_(Run benchmark and fill in)_

| Repo | Files | Time (mean) | Throughput |
|------|-------|-------------|------------|
| Glow | 38k | TBD | TBD |
| CargoWise | 236k | TBD | TBD |

---

## Phase 3: Optimize (ONLY after baseline is established)

### Workflow
For each optimization:
1. Make change
2. `cargo build --release && copy "target\release\fxi.exe" "C:\Program Files (x86)\Microsoft Visual FoxPro 9\vfp9.exe"`
3. Run quick-bench for fast feedback: `.\scripts\quick-bench.ps1` (3 runs)
4. If promising, run full benchmark: `cargo bench -- --baseline main`
5. Keep if statistically significant improvement (criterion will tell you)

### Quick Benchmark vs Criterion
- **Quick iteration**: Use `.\scripts\quick-bench.ps1` (3 runs, fast feedback)
- **Final validation**: Use `cargo bench` (Criterion, 10 samples, statistical comparison)

### Adding Timing Instrumentation
To identify bottlenecks, add timing prints using `std::time::Instant` around major phases:
```rust
let start = std::time::Instant::now();
// ... phase code ...
eprintln!("[TIMING] phase_name: {:?}", start.elapsed());
```

Key phases to instrument:
- File discovery
- File reading
- Token extraction
- Trigram extraction
- Segment writes

### Optimization Loop
1. Run quick-bench with timing output
2. Identify top 2-3 slowest phases
3. Try targeted optimization (parallelization, buffering, algorithm changes)
4. Re-run quick-bench and compare
5. Keep changes that improve, revert those that don't
6. Once satisfied, validate with full Criterion benchmark

### Potential targets (from previous profiling)
- Token extraction (~36s cumulative)
- Segment writes (~33-38s)
- File I/O (~27s cumulative)
- Trigram extraction (~24s cumulative)
- File discovery (~15-17s)
