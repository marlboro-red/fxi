# Prepared phrases and nonbinding file limits

Retained two executor changes: prepare quoted/boosted phrases once per query using
the existing regex/literal verifier, and skip sorting all candidates before
verification when a file limit exceeds the candidate count. Results still receive
the same final path ordering; a potentially binding limit retains preordering.
The phrase path now benefits from the existing exact literal probes and packed
source verification, with their existing freshness checks.

Previously, quoted literals repeatedly used per-line fallback verification, even
though equivalent regex literals already had a prepared fast path. This was a
large syntax-dependent cost. This change does not introduce a new index format,
result cache, or weaker source verification.

## Isolated paired experiment

Apple M2 Max / macOS 26.1, existing common Linux fixture (65,284 files,
1,311,592,608 source bytes). Same immutable index and warm filesystem; two isolated
daemons, compact complete-file native API responses, new connection per query,
including transfer and JSON decode. One warmup followed by 21 interleaved samples
per query/form, randomized before/after request order. No concurrent compilation,
tests or other benchmark. Ordinary desktop background activity remained.

The binaries use one isolated copy of the working source, including concurrent
lazy-reader initialization work; they differ only in `src/query/executor.rs`.
The before binary restores that file from the committed baseline. Encoding source
was checked byte-for-byte against HEAD, excluding the independent encoding
experiment. Their hashes are in the raw data. Startup is not timed here, so these
numbers must not be used to assess reader initialization.

Every warmup and all 504 timed samples matched independent ripgrep complete file
sets exactly, with no duplicates. Fixed patterns were reused; no result caching
was introduced. Regex and quoted forms denote the same case-sensitive literal
in these six cases. The harness also records first requests and daemon RSS as
observations, not controlled cold-cache or memory experiments.

| Literal | Syntax | Files | Before ms | After ms | Before / after |
|---|---|---:|---:|---:|---:|
| `folio_wait_bit_common` | regex | 1 | 0.578 | 0.575 | 1.00× |
| `folio_wait_bit_common` | phrase | 1 | 8.495 | 0.568 | 14.94× |
| `auditNonexistentSymbol94283` | regex | 0 | 0.303 | 0.290 | 1.04× |
| `auditNonexistentSymbol94283` | phrase | 0 | 0.287 | 0.273 | 1.05× |
| `struct file_operations` | regex | 1240 | 5.761 | 4.578 | 1.26× |
| `struct file_operations` | phrase | 1240 | 65.563 | 4.942 | 13.27× |
| `static const` | regex | 21570 | 34.778 | 33.112 | 1.05× |
| `static const` | phrase | 21570 | 90.711 | 33.469 | 2.71× |
| `const struct` | regex | 30667 | 33.071 | 33.325 | 0.99× |
| `const struct` | phrase | 30667 | 104.576 | 36.450 | 2.87× |
| `return` | regex | 44258 | 40.383 | 40.609 | 0.99× |
| `return` | phrase | 44258 | 82.847 | 40.781 | 2.03× |

The measured regex phrase improvement is about 21%; the much larger quoted
phrase improvement fixes a separate, previously unmeasured inefficient syntax
path. The quoted results must not be represented as closing the prior roughly
4× regex gap against Zoekt. This run compares two FXI versions only. Sub-percent
regressions in broad regex controls are below this experiment's noise resolution;
no universal or competitor-winning claim follows.

## Reproduction

```sh
python3 docs/performance-phrase-preparation/compare.py \
  --corpus /private/var/folders/6w/jph_9hyd71gbqw76h25h2zz00000gn/T/fxi-common-indexer-corpus-1td4bkex \
  --indexes /private/var/folders/6w/jph_9hyd71gbqw76h25h2zz00000gn/T/fxi-indexer-comparison-4tmf41uv/fxi \
  --baseline /private/tmp/fxi-phrase-before \
  --candidate /private/tmp/fxi-phrase-after \
  --output docs/performance-phrase-preparation/paired.json \
  --repetitions 21 \
  --patterns folio_wait_bit_common auditNonexistentSymbol94283 \
    'struct file_operations' 'static const' 'const struct' return
```

The source fixture/index provenance and independent corpus manifest verification
are documented in [the preceding comparison](../performance-after-audit/NOTES.md).
See [raw samples](paired.json) and [the exact measured harness](compare-measured.py).
The maintained [reproduction harness](compare.py) removes an unused misleading
`--literal` switch and escapes supplied fixed literals for regex syntax, with a
fixed-string ripgrep oracle. The historical invocation used only the six shown
plain patterns, for which both versions issue identical queries and oracles.
The raw harness hash refers to `compare-measured.py`.

## Correctness coverage

`prepared_phrases_preserve_boundaries_limits_unicode_and_live_verification`
uses 140 files crossing the parallel threshold and the cached-source size threshold,
independent per-line regex expectations, cold/repeated queries, binding/unbinding
limits (0, 1, 17, 140, 10,000,000), case-sensitive/Unicode-insensitive phrases,
empty phrases, LF/CRLF boundaries, and deletion plus same-size edits with restored
mtime. The focused test passed before timing. Existing line-filter logic remains
on the verified per-line path. After the timing window:

- `cargo test --lib query::`: **147 passed**, zero failures.
- `cargo clippy --all-targets -- -D warnings`: passed.

No schema/index migration is required.
