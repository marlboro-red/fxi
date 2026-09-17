# Query correctness audit — 2026-09-18

Read-only production audit of the implementation following `aaf0b14`. Findings below are independently reproduced against the release executable, except the explicitly marked debug-parser overflow probe. No production changes were made for this report. Severity denotes impact, not estimated repair effort.

## Reproduction and scope

```sh
python3 docs/audit-2026-09-18/reproduce-query-audit.py --daemon-probes \
  > docs/audit-2026-09-18/query-probes.json
```

The script creates isolated temporary source trees, indexes and a Unix socket; it never connects to the normal daemon. The optional daemon probes deliberately crash **only the disposable daemon**, with core dumps disabled. JSON records the executable hash, exact queries, outputs and daemon log. It also performs 82 files-only regex comparisons against independent `rg` results on 80 deterministic randomized files; **all 82 comparisons passed**. Coverage includes Unicode case equivalents, bounded/unbounded repetition, alternation, short branches, anchors, word boundaries, optional components and line-sensitive expressions. These passes are evidence for those cases, not a proof of planner completeness. No performance conclusions follow from these untimed probes.

`parser-probes.json` records the standalone debug-parser probe source, exact `rustc` command and five input/output/exit-code records, including the timestamp overflow and escaped-quote AST. That probe imports only `src/query/parser.rs`; it does not rebuild the project.

Inspected parser, conservative HIR planner, candidate execution, content/count/files-only/ranked execution, source verification/cache paths and daemon query entry points. Full repository tests were not rerun by this audit worker.

## Confirmed correctness defects

### Q1 — High: bounded query can abort the entire daemon

`src/query/parser.rs:130–170,226–235` recursively parses parenthesized expressions without a nesting/node budget. A 20,003-byte query, `"(" * 10000 + "foo" + ")" * 10000`, sent as a normal `Search` request to the release daemon closes the connection and exits the daemon with SIGABRT (`-6`). Its log says `has overflowed its stack`. The framed request is far below the protocol message-size ceiling. Other clients and watched roots lose service together; panic-catching does not recover Rust stack-overflow aborts.

**Repair:** a fallible parser with bounded bytes, nesting and AST complexity before recursion; bounded planner/executor traversal too. Add subprocess regression tests asserting that an error response is returned and a subsequent Ping/search succeeds. Also exercise deeply nested NOT/group combinations and very wide Boolean expressions. Iterative parsing alone is insufficient if AST planning/destruction remains unbounded.

### Q2 — High: non-finite boost produces an invalid typed response

`src/query/parser.rs:178–212` accepts overflowing decimal boosts as `f32::INFINITY`; `src/query/scorer.rs:100–102` propagates infinity. Sending `^99999999999999999999999999999999999999999999999999999:foo` returns JSON `"score": null` from a normal ranked `Search` response. `SearchMatchData.score` is a required `f32`, so a typed client cannot decode the response the server generated. This is a concrete protocol contract violation, not just a ranking preference.

**Repair:** reject non-finite/out-of-range boosts and keep scorer results finite even for public API inputs. Cover JSON round trips for extreme inputs. Literal `NaN`/negative boost syntax is not accepted by the numeric lexer; no claim is made that it currently parses NaN. Extremely large finite scores multiplied together also deserve a finite-output guard.

### Q3 — High: NOT contributes a fabricated matching line

`src/query/executor.rs:1604–1608` represents a successful negative predicate as a synthetic line-1 hit. AND concatenates that hit into real matches. For `negative.txt = "irrelevant\nfoo\n"`, `foo -absent` prints line 1 (`irrelevant`) as well as line 2 (`foo`). `foo -absent line:1` reports only the unrelated first line; counts and limits include the synthetic hit. This is independent of how duplicate positive spans ought to be highlighted.

**Repair:** separate document-level truth from positive matching spans. A successful NOT should satisfy Boolean evaluation without inventing content. Define pure-negative/file-filter result rendering separately, including line-range semantics; test negative-only, nested NOT, AND/OR with NOT, and all output modes.

### Q4 — Medium: Boolean queries duplicate matching lines and inflate counts

`src/query/executor.rs:1576–1602` concatenates branch hit vectors. A one-line `foo bar` file prints the same line twice for both `foo bar` and `foo | bar`; `-c` reports 2, whereas a single regex alternation reports 1. `foo | foo` is similarly non-idempotent. Ranking uses the inflated `file_matches.len()`; `-m` can discard distinct later lines because duplicates consume the limit.

The executor and README describe line matching/counting, and each individual literal/regex already emits only its first hit per line. If a per-term-span model is intended instead, it must become an explicit, consistent contract rather than differing by query syntax.

**Repair:** unify document truth, unique matching lines and highlight spans; deduplicate lines without losing multiple highlight ranges. Add Boolean idempotence and count/content/file consistency properties.

### Q5 — High UX/correctness: punctuation silently changes literal queries

`src/query/parser.rs:270–305` stops its first word scan at punctuation, then reparses the remainder as new terms/operators. `foo-bar` becomes `foo AND NOT bar`, excludes a file actually containing `foo-bar`, and returns `foo`-only files. `foo.bar` becomes `foo AND .bar`, returning unrelated `foo` lines once `.bar` occurs elsewhere in the file. This conflicts with the advertised case-insensitive substring behavior for single bare tokens and is especially troublesome for code identifiers, paths and flags.

**Repair:** lexical boundaries must distinguish whitespace-delimited operators from interior literal punctuation. Document an escape/literal mode and test hyphens, dots, C++ `::`, paths, calls, quoted text, Unicode punctuation and leading minus. Parent CLI audit covers `-e`/`-w` interactions separately.

### Q6 — Medium: escaped regex and phrase delimiters are not supported correctly

`src/query/parser.rs:254–265` ends regexes at any slash, including escaped slashes and slashes in character classes. `re:/foo\/bar/` fails with `incomplete escape sequence`; literal slash workarounds such as `\x2f` exist, but the natural spelling fails. `parse_phrase` at `238–251` similarly ends at an escaped quote: `"a\"b"` becomes multiple query nodes rather than the intended phrase.

**Repair:** a lexer with explicit delimiter escaping, character-class tracking where appropriate, precise source spans and unterminated-literal diagnostics. Define whether regex delimiter escaping is stripped or preserved before passing to the regex compiler.

### Q7 — Medium: malformed syntax is silently truncated or broadened

`QueryParser::parse` never verifies that all input was consumed. `foo) bar` silently becomes `foo`, admitting files without `bar`. Unclosed groups/quotes and malformed filters also silently degrade; existing tests deliberately accept some of this behavior, so strict validation is a compatibility decision as well as a fix. Interactive partial-query tolerance should not silently apply to scripting.

**Repair:** `Result<Query, QueryError>` with complete-input validation; optionally expose a separate tolerant parser for the TUI. Unknown fields, malformed known fields and genuinely literal colon expressions need an explicit grammar policy.

### Q8 — Medium: filters do not participate in Boolean scope

`src/query/parser.rs:308–370` mutates one global `QueryFilters` value and replaces filter nodes with `Empty`. `ext:rs | ext:py` returns **only Python files**, and `ext:rs foo | ext:py bar` applies `ext:py` to both branches. `-ext:rs` returns nothing rather than non-Rust files. Repeated filter fields overwrite earlier values.

If filters are intentionally global directives, Boolean filter expressions must be rejected clearly. Present syntax accepts them and produces surprising answers. This is an ambiguous language-design contract; unlike Q1–Q3, the exact replacement semantics require an explicit decision.

**Repair options:** AST-local filter predicates, or a clearly separated global-filter syntax with validation against nesting/negation and duplicate conflicting filters.

### Q9 — Medium: proximity is anchored to first term, not a shared window

`src/query/executor.rs:1650–1662` independently accepts every other term within ±distance of the first term. A document with beta on line 1, alpha on line 3 and gamma on line 5 matches `near:alpha,beta,gamma,2`, although the terms span 4 lines. Reordering terms changes whether it matches. README describes terms “within ... lines of each other,” and the implementation docstring says all terms must be within distance of each other.

**Repair:** require a common window with `max(line)-min(line) <= distance` using sorted occurrence lists/sliding windows. Define repeated-term behavior and which lines are displayed. Test permutation invariance and adversarial occurrences on both sides of the first term.

### Q10 — Medium: numeric and calendar boundaries are incorrect

`src/query/executor.rs:1168–1188` implements `size:>N` and `size:<N` inclusively; an exactly 8-byte file matches **both** `size:>8` and `size:<8`. Timestamp comparisons have the same strict/inclusive mismatch. A date query's upper bound is inclusive, so `mtime:2026-09-18` includes midnight at the start of September 19.

`src/query/parser.rs:431–446` accepts invalid dates (`2026-02-31` matches March 3) and counts leap years without century corrections (`2101-03-01` returns the March 2 file). The comment about leap seconds does not explain the century error. At `418`, `mtime:18446744073709551615` overflows when adding a day: a standalone debug build of the parser panics; release wraps and returns no results.

**Repair:** validated Gregorian dates, checked conversions and explicitly represented inclusive/exclusive bounds. Date windows should be `[start, next_day)`; reject unsupported timestamps and malformed ranges instead of silently dropping constraints.

### Q11 — Medium: ranked wire limits are silently capped by another default

`src/server/daemon_core.rs:701–712` leaves parsed `QueryOptions.limit` at its default 100 before calling `execute`, then applies the request limit afterwards. A fixture with 150 matching lines returns 100 for wire `limit:0` **and** `limit:150`; adding `top:0` returns all 150. The request schema says maximum results, but does not expose that another implicit cap takes precedence.

**Repair:** define precedence between wire limits and query `top:` once, set the executor's effective limit before executing, and test 0/1/100/>100 and explicit `top:`. If 100 is intentionally a default ranked cap, distinguish omitted request limits from explicit unlimited requests in the API.

## Further improvement opportunities, not independently proven bugs

- **Immutable query snapshots:** candidate verification captures line text, but context rendering can re-read the file later (`executor.rs:368–382`). Concurrent writes can mix matching text from one version with surrounding lines from another. Keep the same source snapshot through rendering; add a controlled mutation test rather than relying on timing.
- **Plan/verification cost:** compound content matching repeatedly scans/lowercases a file and clones line strings for every AST leaf, then concatenates vectors. Prepare matchers once per query, evaluate file predicates separately, collect each line once and reuse compiled automata. Benchmark broad AND/OR/NOT and Unicode cases as well as simple literals.
- **Memory bounds:** content execution collects all verified records and sorts them before applying CLI/wire limits; syntactically wide OR can multiply retained duplicate strings. Stream path-ordered content/count results with cancellation and use bounded top-k ranked heaps where score bounds preserve correctness.
- **Source diagnostics:** unreadable/disappearing/invalid-UTF8 candidates are silently skipped through Option-returning readers. A strict/diagnostic mode could report incompleteness without breaking normal editor workflows.
- **Semantic observability:** expose parsed query/plan inspection so users can see whether a query is a phrase, Boolean combination, filter or regex. This would have made several silent-grammar failures apparent.
- **Property testing:** use Boolean idempotence/permutation invariance, count/content line-set parity, parser round trips and independent regex oracles. Existing tests often compare two FXI output paths sharing the same verifier, which can preserve the same bug in both.
- **Fail-closed malformed filters:** typoed size/mtime/line/near values currently often remove constraints. Scripting benefits from specific errors; editor typing can use a separate tolerant mode.
- **Truth/rows/highlights/ranking:** formalize these as separate representations. Several defects above share the present conflation of “file satisfies predicate” and “vector is nonempty.”

No new regex candidate-recall defect was found in the 82-case grid. Current conservative HIR fallback, liveness masking before verification, immutable generation ownership, and metadata-validated source cache are sound choices in the inspected paths; broader randomized/index-mutation coverage is still needed before declaring correctness complete.
