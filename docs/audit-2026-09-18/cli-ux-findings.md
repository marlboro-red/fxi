# Actual CLI and terminal UX audit

Baseline `aaf0b14`, macOS, release executable. [cli-probe.py](cli-probe.py)
executes 54 command/lifecycle observations in isolated indexes and a private
foreground daemon; [raw output](cli-observations.json) includes arguments,
stdout, stderr and exit codes. [terminal-probe.py](terminal-probe.py) uses a
real pseudo-terminal; [terminal observations](terminal-observations.json) record
terminal flags and screen escapes. This is an executable UX pass, not just a
README review. Production code is unchanged.

## High-priority behavior defects

### U1 — Explicit path selection silently searches outside the selected scope

`main.rs:411` resolves `-p` through `find_codebase_root` (`utils/app_data.rs:87`)
and drops the original file/subdirectory restriction. `fxi -l alpha -p sub`
returns `a.rs`, `b.py`, `upper.rs` and other paths outside `sub`; specifying
`-p a.rs` also searches the entire repository. Help says “Path to search in.”
This is a misleading scope contract even if repository-root selection was the
original intention. Keep index-root resolution separate from the search scope;
intersect a selected file/subtree before matching, or explicitly rename/document
an index-root selector and provide a real path restriction. Relative output must
be defined against the search cwd or declared root consistently.

### U2 — Adding an OR alternative or whole-word flag changes existing matching semantics

`main.rs:594–632` returns a single pattern unchanged but converts multiple `-e`
patterns and `-w` to escaped, case-sensitive regex text. In the probe, `-e alpha`
finds `upper.rs` containing only `ALPHA`; adding `-e unmatchedZZZ` removes it.
An additional OR branch should not remove existing matches. `-w alpha` likewise
silently becomes case-sensitive. `--regexp` is misleading: multiple patterns
are regex-escaped, while a single pattern uses FXI's query language. Wrapping
patterns containing `/` also interacts with the broken delimiter lexer:
`-w foo/bar` misses the exact `foo/bar` source line.

Construct AST alternatives instead of round-tripping through query text, and
preserve each pattern's case/matching mode. Define explicit literal, regex and
query-language modes; make `-e` repeat the selected mode consistently. Test
monotonicity under adding OR alternatives and whole-word narrowing independently
of case behavior.

### U3 — Index management and the live daemon disagree

In the private non-watching daemon fixture:

1. Load the existing index by searching.
2. Create `added.rs`, run successful `fxi index --force`.
3. Searching for its new unique term returns nothing.
4. `fxi daemon reload` makes it appear.
5. `fxi remove ROOT` reports success and `fxi list` is empty, but daemon queries
   still return results from its retained reader.

CLI index/compact/remove branches (`main.rs:203–255`) do not coordinate reader
invalidation with the daemon. Successful commands should establish an explicit
postcondition: reload after publication; unload/stop watching after removal;
never silently retain an obsolete reader as current. Test force/incremental/
compact/remove against both watched and non-watched daemons, plus concurrent
queries and external generation replacement. Merely telling users to reload
manually is a workaround, not coherent command behavior.

### U4 — Overlapping context prints duplicate and out-of-order lines

`output.rs:131–158` renders every match's context independently. `-C 1 alpha`
on adjacent matching lines produces line numbers `1,2,1,2,3,3,4,5`: a matching
line appears first as context, then again as a match, and output moves backwards.
Merge context intervals per file, emit each line once in order, retain match
highlighting when a line belongs to either role, and separate only actual gaps.
Boolean duplicate hits compound this defect; fix the truth/line/span model
before or alongside rendering.

### U5 — TUI startup failure leaves the terminal broken

The PTY probe launches `fxi search DOES_NOT_EXIST`. It exits 1 after enabling
raw mode and entering the alternate screen. Echo/canonical input remain false,
and no leave-alternate-screen sequence is written. `tui/mod.rs:49–76` needs a
terminal guard covering every fallible initialization step, normal exit and
panic cleanup. The probe restores its own disposable PTY after recording the
failure; it does not modify the user's terminal.

## Command contract and discoverability

| Finding | Executed observation / source | Improvement |
|---|---|---|
| No version command | `fxi --version` exits 2 | Add `--version`/`-V`; expose package and optionally build/protocol versions. |
| Help advertises unsupported functionality | `--help` promises `-v` shows non-matching lines; execution rejects it | Mark unsupported clearly or remove it from normal help until implemented. |
| Query grammar invisible in help | No examples explain case-insensitive bare tokens, file-level AND, actual phrase quoting, or `re:/.../` | Add short executable examples and a query-language help topic/`--explain`. Distinguish shell quotes from phrase quotes. |
| Familiar path syntax rejected | `fxi alpha sub` exits 2; only `-p` exists | Accept positional paths consistently, or show the supported form in errors/examples. Avoid ambiguity with management subcommands. |
| Reserved query words/leading minus collide with CLI parsing | `fxi stats` runs statistics; `fxi -absent` errors; `fxi -- -absent` is required | Document `--`/`-e` escape routes prominently; consider explicit `query`/`grep` command aliases. |
| No-argument piped execution gives OS error | `fxi </dev/null` returns `Device not configured` | Detect non-TTY mode and show actionable usage instead of attempting raw-terminal initialization; explicitly document whether stdin searching is supported. |
| New-user unindexed error is useful but minimal | `No index found. Run 'fxi index' first.` | Include resolved root and exact command, especially after `-p`; don't silently scan/build a large repository without explaining the action. |
| Parser errors/ignored directives are inconsistent | Bad regex errors, but `size:bogus`, invalid glob, `sort:nope` silently broaden/ignore; `top:1` does not limit content output | Fallible strict CLI parser; expose ranked-only directives or reject unsupported combinations. Separate tolerant TUI editing from scripting. |
| Result limit terminology is ambiguous | `-c -m 1 alpha` prints only `a.rs:1`, limiting the global collected output, unlike rg's per-file match limit | Name/document global results vs per-file lines vs files; define how count, content, files and ranked limits interact. |

## Piping, automation, and output

- **Exit statuses:** no matches and empty query return 0; invalid regex returns
  1; clap usage errors return 2. Failed `daemon reload` when no daemon exists
  also returns 0 and prints failure to stdout. These are poor scripting signals.
  Define one documented contract; adopting grep's 0/match, 1/no-match, 2/error
  would be a compatibility change, not a silent cosmetic fix. Operational
  failures must have nonzero status and stderr diagnostics regardless.
- **Piped output changes shape with result cardinality:** one file uses
  `path:line:text`, multiple files switch to headings even when stdout is piped
  (`main.rs:545–555`). There is no `--no-heading` override. Prefer stable
  path-prefixed non-TTY output, explicit heading controls, and a documented
  machine-readable mode.
- **No unambiguous path serialization:** `-l` prints a filename containing a
  newline as two paths; no `-0`/`--null` or JSON option exists. Add NUL-delimited
  filenames and structured records for line/count output. Native filename-byte
  fidelity is a separate index-format concern; see the index report.
- **Broken pipes produce noisy errors:** piping a large result into a consumer
  that closes after one line exits 1 with `Error: Broken pipe`. Treat expected
  downstream closure quietly while preserving real write errors.
- **Highlight structure is too narrow:** records have one start/end pair even
  when Boolean terms or several occurrences match a line. Preserve multiple
  spans after deduplicating rows, and define byte-offset units explicitly.
- **Color/accessibility contract:** auto mode only checks stdout TTY, then uses
  forced color internally. Review `NO_COLOR`/`TERM=dumb` behavior and accessibility;
  source-inspected opportunity, not a rendered-terminal color audit.

## Daemon operation and diagnostics

- Starting `daemon start --watch` while a non-watching daemon already runs prints
  only `Daemon is already running`; it does not enable watching. Surface current
  mode and provide an explicit upgrade/restart instruction or supported mode
  transition. Status should report watched roots and watcher health.
- Start/status/reload handlers print several failure paths to stdout and return
  success (`main.rs:277–403`). Startup uses a fixed 500ms sleep rather than a
  readiness handshake; shutdown uses the same timeout before force termination.
  Use bounded readiness/completion handshakes with meaningful errors and state.
- `fxi index` can say “up to date” because pending map count is zero even while
  notifications are in the OS watcher/debouncer/channel. Distinguish known
  pending work from a verified freshness barrier; avoid stronger claims than
  the daemon can establish.
- `daemon status` reports memory as `doc_count*100 + 1MiB/root` and a cache-hit
  rate whose hit counter is never incremented (`daemon_core.rs:936–958`). Label
  measured/accounted categories honestly rather than showing misleading totals.
- `stats` counts all historical/tombstoned documents and languages. In the probe,
  deleting the sole Python file and indexing again still reports Python 1 and
  Document count 7, though only six live files remain. Show live documents,
  tombstones, generations and storage separately (`index/stats.rs:22–44`).
- Expose searchable revision versus durable revision, pending paths, last
  reconciliation, update errors, compaction status and the resolved search root.
  These are actionable UX, not internal trivia: they explain missing matches.

## Interactive/editor UX requiring follow-through

The architecture report covers uncancelled TUI searches, synchronous reindex and
preview work, silent editor-launch failures, daemon-connected initial-query
handling, extension connection ownership, provider disposal and Unicode offset
rendering. These need focused interaction tests rather than only protocol-shape
mocks. A full visual VS Code session, screen-reader accessibility, Windows
console behavior and arbitrary external editor integrations were not exercised.

## Proposed CLI contract to implement deliberately

1. One explicit matching mode per invocation: literal, regex or FXI query; flags
   compose without changing the mode or case behavior unexpectedly.
2. Index root and requested search scope are distinct; show both when diagnosing.
3. File truth, unique matching lines, highlight spans and ranking are separate.
4. Human TTY output can use headings/colors; piped output is stable and supports
   JSON/NUL forms. Expected pipe closure is quiet.
5. Mutation commands coordinate with live readers and report completion/failure.
6. Strict CLI errors identify the token and recovery action; partial-query
   tolerance belongs to interactive editing only.
7. Help examples are executable contract tests, including no-index and no-daemon
   journeys. New parser/exit/scope behavior needs documented migration where it
   intentionally differs from existing releases.
