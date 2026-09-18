# Generated correctness testing

These suites supplement the example-based tests. They are regression evidence,
not a proof that every query, filesystem race or configuration is correct.
All default suites run in ordinary `cargo test --all-targets` on native Linux,
macOS and Windows CI. Failed query-oracle runs save minimized JSON fixtures;
CI uploads those fixtures for replay.

## Independent search reference

`tests/generated_differential.rs` generates source documents and its own expression
trees, renders query text, and compares FXI with an exhaustive reference evaluator.
The reference does not use FXI's parsed AST, planner, postings, verifier, line maps
or scorer. Boolean truth and positive line evidence are evaluated separately.
Proximity uses exhaustive line windows rather than the production sweep algorithm.

Coverage includes:

- Bare terms, phrases, regexes, boosts, proximity, nested AND/OR/NOT.
- Unicode, case folding, word boundaries, empty matches, tabs, LF/CRLF and missing
  final newlines; explicit rejection of unsupported word/boost/proximity mixtures.
- Global extension, language, path, filename, byte-size, timestamp and line filters;
  file and component-boundary subtree scopes.
- Exact ordered file lists, unique-line counts, global limits, source text,
  UTF-8 highlight offsets and context.
- Independently expected ranked membership/path order; finite scores,
  score/recency ordering and exact top-k prefix properties. The scoring formula
  is deliberately not duplicated in the oracle.
- Cached and uncached readers. A forced broad query exercises parallel scans;
  larger source-pack campaigns exercise packed verification.

Default: three seeded 72-document corpora, 120 generated cases and one forced
broad scan per corpus, plus saved minimized regressions. Cases include rejected
option combinations; case counts are not counts of independent semantic rules.
Every fixture asserts that all generated documents were actually indexed.

On failure, the reducer removes expression branches, filters/options, documents
and source lines while preserving the failing output category. It writes a JSON
fixture and prints its seed, case and replay command. This is deterministic
delta reduction, not a guarantee of the globally smallest counterexample.

```sh
cargo test --test generated_differential
FXI_DIFF_SEED=51966 FXI_DIFF_CASES=3000 cargo test --test generated_differential
FXI_SOURCE_PACK=1 FXI_DIFF_DOCUMENTS=180 FXI_DIFF_CASES=400 \
  cargo test --test generated_differential
FXI_DIFF_REPLAY=/path/to/failure.json cargo test --test generated_differential
```

`FXI_DIFF_FAILURE_DIR` selects where failure JSON is saved (system temporary
directory by default). Commands use POSIX environment syntax; PowerShell users
can set the same variables through `$env:NAME`.

## Other generated suites

| Suite | Default coverage | Replay |
|---|---|---|
| `cli_generated` | 128 CLI cases / 672 search checks: fixed/regex, `-i`, `-w`, repeated `-e`, scopes, limits, context, JSON/count/text/NUL; unusual paths and Unicode | `FXI_CLI_SEED`, optional `FXI_CLI_CASE` |
| `stateful_differential` | Four seeds, 192 generated mutations plus fixed same-length edit/revert/no-op sequences; update, reopen, compaction and rebuild checkpoints | `FXI_STATEFUL_SEED` |
| `parser_generated` | 512 seeds × seven Unicode/syntax cases, plus byte/node/depth and calendar/numeric boundaries | `FXI_PARSER_SEED` |
| `index_generated` | 1,760 bounded index-component mutations; 20,000 generated varint values plus independent decoding checks over truncated/random bytes and overflow boundaries | Printed mutation bytes / deterministic loops |

The CLI oracle exhaustively scans source; exact case-sensitive literals use
`str::find`. The stateful oracle recursively scans current files at durable
checkpoints and checks file lists/counts. It also verifies that pinned readers'
stored postings/line maps survive later generations. It does not pretend that
an old reader freezes editable source content. Stateful failures print a reduced
operation sequence and a seed; CLI failures print the seed/case/arguments/source.
Automatic shrinking is implemented for query fixtures and stateful sequences,
not the CLI/parser mutation grids.

## Fuzz targets and limits

The parser fuzz target shares the parser contract checker; `fuzz_index_reader`
shares the tiny bounded index mutation fixture. Both compile in CI using the
separate checked-in fuzz lockfile. Generated build/corpus/artifact directories
are ignored; minimized search regressions are checked in explicitly.

The index target caps mutation inputs at 256 bytes and works on tiny local
fixture components. A changed component may encode another valid index, so the
property is controlled rejection or bounded successful operation—not that every
mutation must fail or preserve the original answer. The deterministic decoder
test uses a separate arithmetic reference.

```sh
cargo test --test parser_generated --test index_generated
cargo check --manifest-path fuzz/Cargo.toml --locked
```

No coverage-guided libFuzzer campaign is claimed for this pass: automated review
blocked that optional run. Deterministic generated/mutation tests ran, and the
fuzz targets compiled. They are different kinds of evidence.

## Findings and executed campaigns

The first query campaign found an actual output discrepancy: an empty
case-insensitive phrase could invent a line after a final newline. The ASCII
whole-file finder visited EOF, whereas line-based regex/count verification did
not. Empty literal verification now explicitly iterates real source lines. The
fix has a hand-checked LF/CRLF/no-final-newline regression and a saved minimized
fixture in `tests/fixtures/differential/`.

After that fix:

- A 9,000-case campaign passed files, counts, content/context and path-ranked
  comparisons on cached readers (before adding the later ranking/reader checks).
- A 1,200-case, 180-document source-pack campaign passed the expanded cached and
  uncached suite including score/recency/top-k properties. A subsequent run
  includes the guaranteed broad packed scan.
- The combined default suite passed 897 Rust test executions locally, including
  duplicated library/binary unit tests. Strict Clippy, Rust 1.88 and Windows
  cross-target checks also passed; native CI is a separate final check.

## Remaining uncertainty

The reference shares the `regex` crate's matching semantics. It independently
tests query composition/planning/verification/output, not that regex engine.
Generated patterns and filters are bounded subsets, not the whole regex/glob
language. Ranking properties do not prove the desirability of relevance scores.
The stateful campaign checks durable checkpoints, not all daemon watcher races,
multi-client interleavings, partial I/O failures or power-loss durability.
Very large files/corpora, sustained memory pressure, network filesystems and
coverage-guided exploration remain separate work. A stale index may miss new
matches until an update, as documented in `SEMANTICS.md`.
