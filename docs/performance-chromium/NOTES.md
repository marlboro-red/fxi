# Chromium checkout benchmark — 2026-09-20

FXI is strong on selective indexed searches, build time, and build memory in this
checkout. It is not an overall winner. The run exposed expensive short-pattern
searches, hard-coded coverage exclusions, and large source-pack sensitivity to
what ran immediately before the query. Turning every opt-in on together is not a
substantiated default policy.

The main comparison interleaves tools, with two private FXI daemons alive for the
resident variants. The follow-up uses only standalone FXI processes. **These are
different cache regimes.** A controlled full-tree-scan experiment reproduced the
large difference without any daemons: a packed query took about 1.6 seconds after
ripgrep, then about 0.12 seconds on immediate repeats. Neither table should be
presented as the single definitive “warm” result.

Raw samples, exact path differences, binary hashes, build logs, source identity,
all follow-up results, and supplementary experiment sources are retained in
[`evidence.json.gz`](evidence.json.gz). The two main harnesses are versioned in
`scripts/`; their hashes are checked against the evidence bundle.

This report measures the main Chromium repository, not a complete `gclient`
dependency checkout. The source was cloned with `--depth 1 --single-branch` from
`https://chromium.googlesource.com/chromium/src.git`. No history expansion or
dependency fetching was performed.

## Reproduction and scope

The harness is [`scripts/benchmark-chromium.py`](../../scripts/benchmark-chromium.py).
The shell script of the same name now delegates to it; the old script compared
different file scopes, accepted count-only agreement, suppressed errors, and
used the user's daemon and index registry.

```sh
git clone --depth 1 --single-branch \
  https://chromium.googlesource.com/chromium/src.git /tmp/fxi-chromium-src
python3 scripts/benchmark-chromium.py \
  --corpus /tmp/fxi-chromium-src \
  --fxi /tmp/fxi-chromium-bench/fxi \
  --fxi-revision 923770d563c23436257841d655c74ed7572f5098 \
  --work /tmp/fxi-chromium-bench/run \
  --output /tmp/fxi-chromium-bench/results.json \
  --repetitions 7
```

The fxi binary was compiled from a clean archive of the recorded commit. The
unfinished Windows source-pack implementation in the working tree was excluded.
The harness records binary hashes, source revision, commands, diagnostics,
individual samples, and exact matching-path differences. Compilation completed
before timing. Existing watcher activity was temporarily suspended and restored
in a `finally` block; benchmark daemons used private sockets and app-data paths.

All search timings include a complete CLI process and complete files-only output.
Each workload has one unmeasured warmup and seven measured repetitions, with tool
order randomized within each repetition. Filesystem caches were not flushed.
Resident fxi calls still include CLI and IPC overhead; they are separate from
standalone calls. Both benchmark daemons remain alive through the primary query
phase, including standalone samples; they do not watch the filesystem. The
competitor measurements use standalone CLIs, not their server APIs. These results
do not establish cold-storage or tail-latency claims.

The default arm uses the full profile with all experimental flags disabled. The
opt-in arm combines the lean profile, compressed source packs, stable segments,
query-local validation, and generation routing. It measures that bundle, not
the independent effect of any one setting. Zoekt uses its stock CLI with ctags
disabled, a 10 MiB file limit, and a raised trigram limit; exact settings are in
the raw build record. csearch and tgrep retain their stock build policies.

The checkout deliberately retains Chromium's binary, malformed-encoding, empty,
and oversized test fixtures. Tools have different ingestion and search policies.
The `^` probe is a search-result probe, not a definitive inventory of ingested
content: tools differ on empty inputs, BOMs, binary handling, and ignored files.
The probe and every timed workload compare exact path sets against
stock ripgrep. Mismatched timings are diagnostic only, not equivalent-work wins.
Even equal results on an individual query do not imply equal whole-corpus
coverage. Build time and index size therefore describe each tool's actual
configuration and scope, not equal-coverage construction throughput.



## Machine, source, and tools

- Apple M2 Max, 12 CPUs, 64 GiB RAM, macOS 26.1, arm64 release binaries.
- Chromium commit `22d582cd964f97f3a43844c9980656b4c0d618fa`, one reachable commit:
  507,289 Git blobs, 4,328,362,526 logical bytes (4.03 GiB). The additional 273
  tracked entries are dependency gitlinks; their repositories were not fetched.
  Git tracked-source checks were clean before and after measurement.
- FXI `923770d563c23436257841d655c74ed7572f5098`.
- tgrep `b1d0fc2f6245cc78f1943e5864ceeab812452404`, clean source checkout.
- Sourcegraph Zoekt `153817f643cde8b229ee388c1dddbcf07f4798af` and Google
  codesearch `74a12a911a79b901d1158c48d011b2da1b090fc9`; embedded Go build metadata
  records unmodified sources. These were existing pinned binaries, not a claim
  to have freshly fetched every tool's latest revision.
- ripgrep 15.2.0 (`e89fff89ac`).

## Construction and reconciliation

One build per configuration, fixed order, without deliberately flushing caches.
Sizes are logical file sizes; scopes and stored information differ. These are
observed resource costs, not equal-coverage build-throughput rankings.

| Configuration | Build seconds | Index MiB | Peak build RSS MiB |
| --- | ---: | ---: | ---: |
| FXI default/full | 26.12 | 2,188.0 | 502.0 |
| FXI opt-in/lean + compressed packs | 28.24 | 3,472.7 | 551.3 |
| tgrep | 39.60 | 2,836.0 | 1,291.0 |
| Zoekt | 105.53 | 7,827.8 | 1,923.5 |
| csearch | 76.09 | 235.0 | 624.9 |

Three no-change `fxi index` calls produced median complete-CLI times of
**2.666 s** (default) and **2.563 s** (opt-in). These include filesystem
reconciliation. They do not measure event-driven watcher latency or changed-file
update throughput.

## Primary interleaved standalone search results

Median milliseconds, seven samples each. `†` means the exact path set differs
from ripgrep: the time is retained for diagnosis and is **not an equivalent-result
performance win**. FXI default and opt-in return identical sets on all twelve
workloads, including the three workloads where both differ from ripgrep.

| Workload | Files in rg result | FXI default | FXI opt-in | tgrep | Zoekt | csearch | ripgrep |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| rare | 11 | 189.42 | 99.49 | 204.93 | 263.14 | 79.08 | 9,827.64 |
| absent | 0 | 187.67 | 92.14 | 186.06 | 246.97 | 28.84 | 9,801.52 |
| phrase | 3,126 | 240.72 | 267.87 | 420.88 | 364.55 | 286.46† | 9,904.35 |
| namespace | 9,179 | 729.38 | 1,129.64 | 2,730.13 | 833.48 | 4,063.75† | 9,830.33 |
| common | 45,119 | 632.96 | 1,023.79 | 4,770.38 | 836.53† | 1,471.11† | 9,993.27 |
| very-common | 139,972 | 1,783.20† | 1,585.68† | 12,803.47† | 1,235.56† | 4,713.67† | 10,788.67 |
| punctuation | 18,004 | 361.37 | 677.81 | 2,675.07 | 698.20 | 681.00† | 9,963.30 |
| case-insensitive | 33,039 | 525.79† | 981.89† | 4,339.57 | 917.96† | 1,376.95† | 9,914.10 |
| short | 1,157 | 15,395.54† | 3,504.54† | 61,264.07 | 1,325.99† | 39,294.74† | 10,143.13 |
| alternation | 793 | 203.82 | 197.28 | 272.45 | 391.96 | 151.12† | 9,899.07 |
| regex | 3,722 | 375.42 | 775.01 | 2,103.89 | 694.82 | 1,092.78† | 9,917.05 |
| regex-no-prefix | 716 | 203.31 | 162.00 | 350.12 | 942.29 | 124.23† | 10,015.66 |

The rare query is `RenderFrameHostImpl::DidCommitNavigation`; absent is
`fxiChromiumAbsentSymbol8f692a4d`; phrase is `class Browser`; namespace is
`namespace content`; common is `std::unique_ptr`; very-common is `return`;
punctuation is `DCHECK(`; case-insensitive is literal `todo` with `-i`; short is
literal `zx`. The last three rows are regexes `RenderFrameHostImpl|BrowserMainLoop`,
`class [A-Za-z_]+Browser`, and `.*RenderFrameHostImpl` respectively. All other
queries use explicit literal mode, case-sensitive unless indicated.

Default FXI trails csearch on the rare and absent queries, where all tools agree.
The opt-in bundle improves those calls but still loses to csearch. Default FXI is
fastest among equivalent-result standalone configurations on several phrases and
common source literals in this interleaved session. The short query exposes a
large scan fallback: default FXI takes 15.4 seconds despite returning fewer paths
than ripgrep's 10.1-second scan. The packed configuration reduces FXI's own time
by about 4.4× with the same FXI results. That remains a materially different
coverage policy from ripgrep.

## Resident FXI results

The same primary session, complete CLI + IPC latency, not raw server time. These
are not server-to-server comparisons with the other tools.

| Workload | Default resident ms | Opt-in resident ms |
| --- | ---: | ---: |
| rare | 13.79 | 12.83 |
| absent | 13.03 | 11.72 |
| phrase | 17.87 | 17.50 |
| namespace | 409.95 | 386.03 |
| common | 104.90 | 104.39 |
| very-common | 1,205.88† | 1,258.12† |
| punctuation | 47.46 | 50.28 |
| case-insensitive | 83.76† | 83.98† |
| short | 12,782.20† | 13,049.41† |
| alternation | 13.15 | 13.42 |
| regex | 35.59 | 35.44 |
| regex-no-prefix | 13.45 | 12.71 |

Resident execution helps selective searches substantially but does not solve the
short-pattern fallback. In this measured Unix revision, resident queries use the
content cache rather than the optional source-pack path; merely starting a
packed-index daemon does not imply packed verification is being used.

## Prepared-index read-mode comparison

Seven samples per cell, interleaving only these FXI variants, no benchmark
daemons. All results exactly match ripgrep on these four workloads. This is a
separate session and **not a head-to-head ranking against the primary competitor
numbers**.

The full checked index was built with `FXI_QUERY_LOCAL=1` and
`FXI_GENERATION_ROUTING=1`, full profile, packs and stable segments disabled.
Its extra build is recorded in the evidence. An initial runtime-flags-only
control on the unprepared default index fell back to ordinary validation and
showed no improvement; that control and its exact script are retained too.
Enabling read flags without preparing their supporting data is not equivalent
to enabling the full experiment.

| Workload | Full/default | Full/query-local | Full/query-local + routing | Lean/stable, live source | Lean/stable, packs |
| --- | ---: | ---: | ---: | ---: | ---: |
| rare | 159.01 | 43.63 | 43.60 | 41.82 | 41.79 |
| absent | 157.84 | 37.53 | 38.35 | 36.78 | 36.16 |
| namespace | 681.73 | 580.97 | 577.32 | 579.02 | 142.21 |
| punctuation | 331.49 | 208.79 | 216.76 | 212.98 | 90.73 |

Query-local validation is a substantial benefit on prepared data. Adding routing
on top produced little improvement on these four queries in this session. Packs
help the repeated namespace and punctuation queries substantially here, despite
regressing in the mixed-tool session. Lean/stable/live and full/checked/live are
close on these workloads; this does not establish that full and lean profiles
are equivalent for token-position queries or other capabilities.

Query-local validation changes when unused index data is checked; see the
[integrity-policy description](../performance-query-local/NOTES.md). The
experiment should not be described as the default eager integrity policy at a
lower price.

To repeat the read-mode test, first build the full checked index in isolated
storage, then run:

```sh
python3 scripts/compare-chromium-modes.py \
  --snapshot /tmp/fxi-chromium-bench/results.json \
  --checked-indexes /tmp/fxi-chromium-bench/run/full-checked \
  --output /tmp/fxi-chromium-bench/modes.json --repetitions 7
```

## Cache-preconditioning control

Three trials, no daemons, unchanged binary/index/query. Each trial scans the
whole checkout with ripgrep for `namespace content`, then runs the packed FXI
CLI three times consecutively. Every FXI result exactly matches that scan.
Milliseconds:

| Trial | Packed immediately after rg scan | Immediate repeat | Second repeat |
| --- | ---: | ---: | ---: |
| 1 | 1,553.73 | 123.90 | 117.80 |
| 2 | 1,612.56 | 121.37 | 117.91 |
| 3 | 1,757.13 | 118.95 | 118.99 |

This reproduces strong preconditioning sensitivity without concurrent FXI
daemons. It does not identify the responsible kernel caches or prove that daemon
memory had zero effect in the primary session. It does prove that “warm
filesystem” alone is not a sufficient benchmark description for this workload.
Keep both interleaved and focused-repeat results when evaluating pack defaults.

## Coverage findings

The `^` probe returns 460,391 paths in ripgrep and 458,345 in FXI: 2,087 missing
and 41 extra relative to ripgrep. All FXI missing paths fit existing ingestion
rules; none remain unexplained by this classification:

| First applicable exclusion | Files |
| --- | ---: |
| known binary extension | 206 |
| binary heuristic | 272 |
| invalid UTF-8 | 614 |
| excluded path component | 990 |
| over 10 MiB | 5 |

The path-component exclusions comprise **868 paths under `node_modules` and
122 under `target`**. They are tracked Chromium test/vendor content, not merely
untracked build junk. For example,
`chrome/test/data/extensions/api_test/service_worker/messaging/connect_external/target/manifest.json`
is excluded by its directory name. An unconditional directory-name policy is a
real coverage/usability limitation; it should not be hidden behind fast timing.

Forty of FXI's 41 extra `^` paths contain NUL bytes; the remaining path is a file
containing only a UTF-8 BOM, which ripgrep strips. The additional protobuf fixture
returned by FXI for `return` and `todo` also contains NULs. FXI's low-density
binary heuristic and ripgrep's binary/BOM handling differ. Across all twelve
queries, no FXI missing result came from a file included by its `^` probe. This
supports consistency on the tested indexed files; it is not a comprehensive
correctness proof or an endorsement of the default exclusion policy.

| Tool/configuration | `^` paths | Missing vs rg | Extra vs rg | Exact timed workloads / 12 |
| --- | ---: | ---: | ---: | ---: |
| FXI default/full | 458,345 | 2,087 | 41 | 9 |
| FXI opt-in/lean + compressed packs | 458,345 | 2,087 | 41 | 9 |
| tgrep | 459,901 | 492 | 2 | 11 |
| Zoekt | 507,289 | 0 | 46,898 | 8 |
| csearch | 458,154 | 2,554 | 317 | 2 |

Zoekt's `^` result count equals the Git blob count, including paths for empty or
binary content. Do not interpret that probe as proof that every byte of every
blob is searchable. The per-query comparisons are the stronger evidence here.
Exact differences for every tool/query are preserved in the compressed bundle.

## Work this measurement justifies

1. Make indexing exclusions explicit and configurable. Tracked directories named
   `target` and `node_modules` can contain wanted source and fixtures.
2. Investigate short-pattern candidate filtering and verification. Default and
   resident FXI are expensive here; packed execution helps but is not a universal
   solution, and coverage differs from stock ripgrep.
3. Profile standalone opening on this scale, including global metadata. Even
   prepared query-local execution retains a meaningful startup floor, and csearch
   wins the two selective primary workloads where its results are equivalent.
4. Evaluate pack selection across cache regimes and resident-cache working-set
   sizes. Fixed opt-in bundle promotion is not justified by one warm-run number.
5. Validate changed-file updates, cold-storage behavior, other operating systems,
   and competitor server modes separately. None were established by this run.

Eight benchmark-harness regression tests pass. The harness was smoke-tested with
all comparison tools on a small Git fixture before the large run. Benchmark
storage and sockets are private; the user's existing watcher was restored. No
production implementation changes are part of this benchmark commit.
