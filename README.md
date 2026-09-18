# fxi

FXI is an indexed code-search tool for the terminal, with a persistent daemon,
an interactive terminal UI, and a VS Code extension. It uses immutable index
segments to narrow candidates, then verifies matches against source content.

Build an index once, use the daemon for repeated searches, and enable watching
when you want saved changes to appear automatically. FXI has its own query
language; a plain query is **not automatically a regular expression**.

## Install and start

Requires Rust 1.88 or newer.

```sh
cargo install --path .
fxi index /path/to/project
fxi daemon start --watch
fxi -p /path/to/project 'error'
```

For a local build without installation, run `cargo build --release` and use
`target/release/fxi` (`target/release/fxi.exe` on Windows).

`fxi index` detects the codebase root, usually the nearest Git root. A watched
daemon loads roots as they are searched; starting it does not immediately load
and watch every index on disk. Without the daemon, searches read the saved index.

Updating an existing installation? See [behavior changes and migration](docs/MIGRATION.md)
and the [audit fix record](docs/audit-2026-09-18/FIXES.md).

## Choose what your query means

| Command | Meaning |
|---|---|
| `fxi 'error'` | Case-insensitive literal substring: also matches `Error` and `errors`. |
| `fxi 'fn main'` | Both terms anywhere in the same file, in either order. |
| `fxi 'class Foo'` | The same AND rule: both `class` and `Foo`, ignoring case. |
| `fxi '"fn main"'` | Adjacent, case-sensitive phrase `fn main`. |
| `fxi -F 'fn main'` | Treat the whole argument as case-sensitive literal text. |
| `fxi --regex 'fn\s+main'` | A regular expression. |
| `fxi -i -F 'fn main'` | Literal text, ignoring case. |

Shell quotes group an argument and disappear before FXI receives it. Thus
`fxi "fn main"` and `fxi 'fn main'` both pass `fn main`, which FXI parses as two
terms. `fxi '"fn main"'` keeps the inner double quotes for FXI's phrase parser.
Use `-F` when you want spaces, punctuation, operators, or field names treated as
ordinary text. In the TUI and VS Code search box, type `"fn main"` directly;
there is no shell to quote for.

Queries use file-level Boolean matching: in `foo bar`, the terms can be on
different lines. Content output shows contributing matching lines. Negation
excludes files satisfying its operand; it is not inverse line matching.

## Command-line search

```sh
fxi 'TODO' src                    # Restrict results to src
fxi -p src/parser.rs 'error'      # Restrict results to one file
fxi -l 'ext:rs error'             # Matching filenames only
fxi -c 'error'                    # Count matching lines per file
fxi -C 2 'panic'                  # Two lines of surrounding context
fxi -w -F 'main'                  # Whole-word literal
fxi -e 'TODO' -e 'FIXME' src     # Either query, restricted to src
fxi --regex -e 'warn.*' -e 'error.*'
fxi -F -- '-excluded'             # Literal beginning with a minus
fxi -- stats                     # Search the word "stats", not the subcommand
fxi --json 'error'               # Structured output
fxi -l -0 'error'                # NUL-terminated paths for scripts
```

When `-e`/`--pattern` supplies the alternatives, a lone positional argument is
**the search path**, including when that path does not exist. FXI does not guess
based on whether a file happens to exist. For example, `fxi -e foo -e bar src`
searches `src`, rather than adding `src` as another search term.

To migrate `fxi foo -e bar`, write `fxi -e foo -e bar`. The unambiguous legacy
forms `fxi foo -e bar src` and `fxi foo -e bar -p src` still accept `foo` as an
additional alternative. New scripts should put every alternative after `-e`.
`--regexp` keeps its old selected-mode behavior; use `--regex` when the patterns
are regular expressions.

A search path restricts returned files; the index root is detected separately.
Searching from a subdirectory defaults to that subdirectory's scope. Indexing,
statistics, compaction, and the TUI operate on the detected codebase root.

| Option | Behavior |
|---|---|
| `-F`, `--fixed-strings` | Literal-text mode. |
| `--regex` | Regular-expression mode. |
| `-e PATTERN`, `--pattern PATTERN` | Repeatable alternatives in the selected mode; does not itself switch to regex. `--regexp` remains a compatibility alias. |
| `-i`, `--ignore-case` | Ignore case, including phrases and regexes. Bare terms already ignore case. |
| `-w`, `--word-regexp` | Require word boundaries for search terms. |
| `-l`, `--files-with-matches` | Print matching paths once each. |
| `-c`, `--count` | Print matching-line counts per file. |
| `-m N`, `--max-count N` | Global output limit, not ripgrep's per-file limit; `0` means unlimited. |
| `-A N`, `-B N`, `-C N` | Context after, before, or both. An explicit `-C` takes precedence. |
| `-p PATH`, `--path PATH` | Restrict to a file or directory. A positional search path is also accepted. |
| `--heading`, `--no-heading` | Group under file headings, or always print `path:line:text`. |
| `--json` | Structured output for the selected output mode. |
| `-0`, `--null` | NUL-terminate filenames; requires `-l`. |
| `--color auto\|always\|never` | Control terminal coloring. |

Search flags such as `--json`, `-l`, or `--regex` require a pattern; they do not
launch the interactive UI on their own. Use `fxi` or `fxi search` from a terminal
for interactive search. `fxi search` with redirected input/output reports an
error instead of emitting terminal-control sequences. Search flags cannot be
mixed with subcommands: use `fxi --json -- stats` to search the word `stats`.

Terminal output uses file headings by default; redirected output uses
`path:line:text`. No matches is a successful exit (`0`). Invalid queries and
operation failures are errors. `-v` is explicitly unsupported. This is familiar
grep-style output, not full ripgrep compatibility; see [the matching contract](docs/SEMANTICS.md).

## Query language

The default mode, TUI, and extension accept these expressions:

```text
foo bar                         both terms in a file
foo | bar                       either term
(foo | bar) baz                 grouping
foo -bar                        foo, excluding files matching bar
"exact phrase"                  case-sensitive adjacent text
re:/foo.*bar/                   explicit regex inside a larger query
near:foo,bar,5                  terms near each other by line number
ext:rs foo                      file extension filter
lang:rust foo                   language filter
file:config.rs                  exact basename
file:config*                    basename glob
path:src/**/*.rs foo            relative-path glob
size:>1000 size:<10000 foo       strict byte-size bounds
line:100-200 TODO               inclusive line range
mtime:>2024-01-01 fix           modification-time filter
```

Use `fxi -l 'file:config*'` or `fxi -l 'ext:rs'` for metadata-only file searches.
Filters are global query constraints; they cannot be placed inside Boolean
branches to express branch-local conditions. Invalid syntax, malformed dates,
unknown filters, duplicate directives, and unfinished groups are errors.

The ranked TUI also accepts `^foo`, `^3:foo`, `sort:score`, `sort:recency`,
`sort:path`, and `top:100` (`top:0` means unlimited). Ranked searches can include
filename matches. Ordinary CLI and extension content searches match file content;
use `file:` for filenames. For CLI limits use `-m`, not `top:`.

## Freshness and the daemon

```sh
fxi index                        # Reconcile source files with the stored index
fxi index --force                # Full rebuild, preserving the stored profile
fxi index --profile lean         # Rebuild without token and stored line-map data
fxi daemon start                  # Keep loaded indexes warm
fxi daemon start --watch          # Also watch saved changes
fxi daemon status
fxi daemon reload /path/to/project
fxi daemon stop                   # Graceful shutdown
fxi daemon stop --force           # Explicit forced termination if necessary
fxi daemon foreground --watch    # Show daemon diagnostics
fxi daemon socket-path
```

**Search visibility** means a save appears in daemon queries. **Persistence**
means it has also reached the saved on-disk index. These are separate steps.
Small watched updates become searchable through immutable in-memory index
snapshots before publication. Defaults are a 1 ms quiet debounce and a 100 ms
maximum event age; neither is an end-to-end latency guarantee. Persistence
batches wait for 250 ms of quiet or ten seconds of continuous updates. Larger
updates use the durable path.

Watchers reconcile when a root is loaded and periodically repair missed events.
An explicit `fxi index` reconciles the source tree. Reload refreshes a daemon
reader from the saved index; it is not a substitute for indexing source changes.
Graceful shutdown drains pending work before acknowledging completion; forced
termination can discard unpublished changes. A watched restart reconciles the
source tree. Direct disk readers can lag a daemon's live view.

Watching is triggered by filesystem notifications and bounded update work.
Publication, compaction, large branch switches, and notification delivery can
still delay visibility. See [freshness semantics](docs/SEMANTICS.md#freshness)
for exact bounds, configuration and failure behavior.

## Interactive terminal UI

Run `fxi`, `fxi search`, or `fxi search /path/to/project`. Type a query and press
**Enter**. Searches run in the background; repeated submissions coalesce to the
latest query. F5 rebuilds in the background under the normal index writer lock.
Previews read at most 1 MiB per file and may omit the end of a large file.

| Key | Action |
|---|---|
| `Enter` in search | Submit query. |
| `↑` / `↓`, `Tab` / `Shift+Tab` | Select a result. |
| `Ctrl+p` | Switch between search and preview. |
| `Ctrl+w` / `Ctrl+h` | Delete a word / character from the query. |
| `F1` | Show help. |
| `F5` in search | Rebuild the index. |
| `Esc` in search | Clear the query; exit if already empty. |
| `Ctrl+c` / `Ctrl+q` | Exit. |

After submitting a query, search-mode navigation also accepts `gg`, `G`,
`Ctrl+a/e`, and `Ctrl+d/u`. Typing resumes query editing. In preview mode,
`j/k` scroll, `n/N` select the next/previous result, `gg/G` jump, and
`Enter` or `o` opens the file in `$EDITOR`; `q` or `Esc` returns to search.

`EDITOR` may contain quoted executable paths and arguments, such as
`EDITOR='code --wait'`. FXI passes arguments directly, without shell expansion.
Known editors receive a line-location argument; unknown editors receive the file
path. Launch failures appear in the TUI status.

## VS Code

See the [extension setup and usage guide](vscode-extension/README.md). The sidebar
search uses the same query language, opens matches in the editor, and supports
context and files-only output. Searches run on Enter or the Search button.
The shortcut is **Ctrl+Alt+F**, or **Cmd+Alt+F** on macOS.

## Index management and coverage

```sh
fxi list
fxi stats /path/to/project
fxi compact /path/to/project
fxi remove /path/to/project
```

Indexes live outside the source tree. `FXI_INDEXES` overrides their location.

| Platform | Default index directory |
|---|---|
| Linux | `~/.local/share/fxi/indexes/` (respects `XDG_DATA_HOME`) |
| macOS | `~/Library/Application Support/fxi/indexes/` |
| Windows | `%LOCALAPPDATA%/fxi/indexes/` |

Each root has a container with a `CURRENT` manifest, immutable generations,
segment files, and a writer lock. Older generations stay leased while readers
use them. Do not edit index files manually.

Coverage follows ignore rules and configured eligibility limits. Hidden paths,
symlinks, common generated/dependency directories, known binary types,
non-UTF-8 content, and oversized files can be excluded. Searching an index is not
an exhaustive scan of every byte on disk. [SEMANTICS.md](docs/SEMANTICS.md)
documents these exclusions and what stale indexes can miss.

## Performance, resources, and evidence

Performance depends on query shape, output mode, corpus, cache state and platform.
FXI is not the best tool on every measured workload. The reports retain losing
cases as well as wins:

- [Token dictionary compression](docs/performance-token-dictionary/NOTES.md): 50.4% smaller dictionaries, 8.3% smaller unpacked index and roughly 52 MiB less eager-reader memory on the common Linux fixture, with compatibility tests and measured timing tradeoffs.
- [Core and usability follow-up](docs/performance-core-followup/NOTES.md): measured daemon startup/memory improvements, prepared phrase searches, CLI fixes and remaining structural costs.
- [After-audit comparison](docs/performance-after-audit/NOTES.md): the preceding fixed FXI baseline versus tgrep, csearch, Zoekt and ripgrep, including default/packed reads and warm APIs.
- [Update visibility](docs/performance-round7/NOTES.md): short saves, atomic replacement and bursts on macOS.
- [Source packs and common-tool comparisons](docs/performance-round6/NOTES.md): build cost, disk/RSS, cold-process and warm-server results; remaining phrase and absence gaps.
- [Earlier verification and cache experiments](docs/performance-round5/NOTES.md).
- [Current correctness and UX audit](docs/audit-2026-09-18/AUDIT.md): findings, repairs and remaining work.

Each report identifies its tested binary revisions and configurations; older
reports are historical evidence, not a fresh benchmark of subsequent fixes. “Up to 400×” and million-file
extrapolations from the original benchmarks are not accepted as current evidence.

The daemon shares a content cache across readers. `FXI_CACHE_MIB` sets its retained
text budget (`0` disables it; range 0–4096, default 1024). This is not a total RSS
limit. Index metadata, active results, positions and build/update work add memory.
`FXI_SEARCH_PARALLELISM` adjusts verification task count; benchmark your workload
before tuning it.

Daemon searches also have a process-wide admission cap. Set
`FXI_MAX_ACTIVE_SEARCHES` before starting the daemon (default: available CPUs,
up to 8; accepted range 1–256, out-of-range integers are clamped). The cap includes
responses still being delivered to clients. Excess searches receive an explicit
capacity error immediately; they do not wait in a search queue, and the CLI does
not bypass overload by falling back to a direct search. Ping, status and
other control requests use separate admission capacity. On Unix,
`FXI_MAX_PIPELINED` additionally caps searches per connection (default 32,
clamped to 1–256); response queues are bounded and clients that flood a full queue
are disconnected. Slow/disconnected clients release permits after the transport
write timeout or failure. These limits bound request concurrency, not total memory
or the duration of an executing search; they do not cancel searches on disconnect.

An opt-in lean index (`fxi index --profile lean PATH`) skips token dictionaries,
postings, positions and stored line maps. All ordinary CLI search modes remain
available: they use trigrams and verify source text. The default for new indexes
is still `full`. Updates, compaction and ordinary forced rebuilds preserve the
stored profile; `--profile full` explicitly rebuilds the omitted evidence.
`fxi stats` displays the profile. Lean uses a newer index format that older
binaries reject; rebuild with `--profile full` for older-reader compatibility.
Library token and positional APIs are fallible:
a lean index reports unavailable evidence, rather than an empty match set.
Stored line-map lookup returns no map for lean indexes. Neither profile changes
search freshness guarantees or makes stale source verification unnecessary.

Optional Unix source packs (`FXI_SOURCE_PACK=1 fxi index --force PATH`) trade extra
disk/build work for faster broad one-shot files-only searches. They are not enabled
by default. [Round six](docs/performance-round6/NOTES.md) describes this tradeoff
and the separate experimental negative-routing option.
Experimental compressed packs add `FXI_SOURCE_PACK_COMPRESSION=1` during building.
They trade decompression work for less disk usage; see the
[measurements and limitations](docs/performance-source-compression/NOTES.md).
Pack payloads now come from the same source capture as the index postings.
Compaction preserves that provenance and omits legacy, missing or corrupt pack
evidence; live-source fallback remains available, and a full rebuild restores
pack coverage. [Single-capture measurements](docs/performance-single-capture/NOTES.md)
show substantially lower pack build time and memory, with mixed search timings.


## Development

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
python3 scripts/test_benchmarks.py
```

[Generated correctness testing](docs/GENERATED_CORRECTNESS.md) documents the
independent query oracle, CLI cross-product, stateful update tests, byte-mutation
checks and commands to replay or expand a seeded campaign.

Benchmark tools under `scripts/` validate matching file sets and retain raw
samples. Keep compilation and tests outside timing windows; distinguish warm API
latency from CLI startup and output serialization.

## License

MIT
