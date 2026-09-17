# Comparison after the correctness audit

Fresh measurements of the fixed FXI binary against pinned competitor builds on
Apple M2 Max / macOS 26.1, warm filesystem, the same 65,284-file (1,311,592,608-byte)
common Linux-source corpus. This is a measured workload comparison, not a claim
of universal superiority or a survey of latest upstream releases.

FXI binary SHA-256: `cfd35d9848e59b18387fb7e143fd6b796b903699f1580b978f96386aa6c06538`.
It contains production code through `be47cd5`; later commits changed docs/CI.
Zoekt/csearch revisions and build provenance are in [round four](../performance-round4/provenance.json).
tgrep is pinned to `b1d0fc2f6245cc78f1943e5864ceeab812452404`; ripgrep reports 15.2.0.
All executable hashes are recorded in the raw results.

The existing FXI/csearch/Zoekt indexes were reused; tgrep was indexed on this same
fixture before timing. Each tool passed a fresh complete-file coverage check.
Every timed search returned exactly the independent ripgrep file set, without
duplicates. [Independent post-run hashing](corpus-verification.json) confirmed
all source bytes still match the original manifest. Compiler/tests did not run
during timing. Ordinary desktop background activity was present, so small
percentage differences should not be promoted into decisive wins.

## Direct CLI, complete files-only results

Eleven interleaved trials per query after an untimed warmup; medians in ms.
Patterns are case-sensitive regexes. No daemon is used. FXI's two columns reuse
one index: packed reads are explicitly disabled/enabled. This isolates packed
verification, not a separate build/index-size comparison.

| Query | FXI packs off | FXI packs on | tgrep | csearch | Zoekt | ripgrep |
|---|---:|---:|---:|---:|---:|---:|
| `folio_wait_bit_common` | 43.58 | 43.09 | 19.11 | 15.71 | 57.42 | 2427.71 |
| `auditNonexistentSymbol94283` | 40.89 | 41.86 | 17.56 | 3.98 | 56.46 | 2350.82 |
| `struct file_operations` | 84.75 | 54.96 | 113.84 | 380.68 | 61.13 | 2398.45 |
| `return` | 600.67 | 118.61 | 2145.18 | 1520.94 | 142.78 | 2204.10 |
| `folio_wait_bit_common|bpf_prog_select_runtime` | 45.23 | 44.43 | 26.37 | 34.46 | 58.95 | 2430.02 |
| `.*folio_wait_bit_common` | 43.78 | 43.49 | 19.51 | 16.24 | 60.24 | 2378.24 |

[Raw samples and coverage](direct.json), [harness](compare.py).

FXI loses selective, absent, alternation and internal-literal one-shot searches
to tgrep/csearch. Its strict opening validation is now a substantial fixed cost.
Packs help phrase/broad verification but do not remove startup work. Without
packs, Zoekt wins both phrase and broad searches; with packs, FXI is competitive
with Zoekt and much faster than tgrep/csearch on these broad/phrase cases.
Ripgrep has no index construction, persistence, storage or update-lag requirement;
these large indexed-corpus results do not establish a ranking on small trees,
stdin, multiline/inverted output, or the rest of its CLI functionality.

## Warm APIs, complete files-only results

Eleven interleaved requests per query. Connection setup, wire transfer and JSON
decoding are included; CLI startup is excluded. Zoekt paths-only uses the existing
adapter over unmodified Zoekt search internals, matching FXI's compact output.
The stock API carries substantially more metadata, especially for broad results.

| Query | FXI | Zoekt stock API | Zoekt paths-only |
|---|---:|---:|---:|
| `folio_wait_bit_common` | 0.736 | 0.890 | 0.624 |
| `auditNonexistentSymbol94283` | 0.288 | 0.317 | 0.147 |
| `struct file_operations` | 6.157 | 5.617 | 1.542 |
| `return` | 37.947 | 209.619 | 60.267 |
| `folio_wait_bit_common|bpf_prog_select_runtime` | 1.025 | 1.191 | 0.948 |
| `.*folio_wait_bit_common` | 0.658 | 3.799 | 3.436 |

[Raw samples](warm-api.json). The comparable phrase loss is about **4×**.
FXI is about **1.6× faster** for broad results and **5.2× faster** for the
internal-literal case. Selective and alternation differences are small; absence
favors Zoekt. The stock broad-payload ratio is not an engine-only speedup.

## Save-to-search smoke check

Five independent fixtures per tool, 256 probe files, 12 edits spaced 25 ms apart,
default configurations. FXI runs were followed by tgrep runs, not interleaved.
Every run checked exact create/edit/delete results; FXI also checked persistence
after graceful shutdown. Medians in ms:

| Measure | FXI | tgrep |
|---|---:|---:|
| Last save to complete visibility | 17.71 | 42.54 |
| First visibility of a new file during the burst | 23.59 | 52.33 |

[FXI samples](freshness-fxi.json), [tgrep samples](freshness-tgrep.json).
This supports a small-edit advantage in this fixture, not a p99/large-tree or
sustained-publication guarantee. It does not replace the earlier large-corpus
watcher measurements with fresh large-corpus data.

## Storage and unmeasured dimensions

Current logical file sizes of these existing fixture indexes:

| Index | MiB |
|---|---:|
| fxi-packed | 1886.6 |
| csearch | 73.0 |
| zoekt | 3239.8 |
| tgrep | 831.6 |

FXI without source packs previously measured about 627 MiB; that separate
configuration was not rebuilt here. Different indexes store different evidence:
FXI includes tokens/positions, packs add source copies, and csearch is much more
compact. These are logical file totals, not runtime RSS or physical shared-block
usage. Build time/peak build memory were **not** rerun in this comparison.
The [fix validation report](../audit-2026-09-18/VALIDATION.md) documents FXI's
remaining first-open and resident-memory costs from stricter corruption checks.

No fresh ranking is established for line/count/ranked output, cold storage,
concurrent users, multiple roots, long update bursts, Windows/Linux performance,
livegrep, indexed ugrep, OpenGrok, or distributed/Lucene-based services. Passing
native correctness CI on a platform is not a performance measurement there.

## Reproduce

Run commands sequentially after preparing the pinned tools and existing snapshot
indexes. The snapshot JSON records host-local paths; update them for your machine.

```sh
python3 docs/performance-after-audit/compare.py \
  --snapshot docs/performance-round6/linux-packed-common-indexers.json \
  --fxi /path/to/fixed-fxi --tgrep /path/to/tgrep --repetitions 11 --output direct.json
python3 docs/performance-after-audit/verify-corpus.py \
  docs/performance-round6/linux-packed-common-indexers.json corpus-verification.json
python3 scripts/compare-indexer-servers.py \
  --benchmark docs/performance-round6/linux-packed-common-indexers.json \
  --fxi /path/to/fixed-fxi --zoekt-server /path/to/zoekt-webserver \
  --zoekt-path-server /path/to/zoekt-path-server --repetitions 11 --output warm-api.json
python3 scripts/benchmark-freshness.py --tool fxi --binary /path/to/fixed-fxi \
  --repetitions 5 --burst-edits 12 --output freshness-fxi.json
python3 scripts/benchmark-freshness.py --tool tgrep --binary /path/to/tgrep \
  --repetitions 5 --burst-edits 12 --output freshness-tgrep.json
```
