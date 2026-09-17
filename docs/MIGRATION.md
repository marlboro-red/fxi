# Updating from the pre-audit CLI

The correctness/UX fixes intentionally tighten several contracts. Rebuild and
restart the daemon when updating the executable so clients use the new behavior.

| Previous behavior | Current behavior / action |
|---|---|
| `fxi "fn main"` was described as literal text | It is file-level AND. Use `fxi -F 'fn main'` or `fxi '\"fn main\"'` for a phrase. |
| `-p subdir` selected the owning repository but searched outside the path | It now restricts results to that subtree/file. Use the repository root for a whole-project query. |
| Adding `-e` or `-w` could change case behavior | Both preserve the selected pattern mode. Use `--regex` explicitly for regex arguments. |
| Interior `foo-bar` could be parsed as negation | Interior punctuation is literal. Write `foo -bar` for a negative file predicate. |
| Invalid or ambiguously scoped filters could be ignored/broadened | They return errors. Put global filters outside a grouped expression: `ext:rs (foo \| bar)`. |
| Boolean branches could duplicate a source line/count | Content and counts now use unique matching lines. |
| Piped output switched layout with result count | It consistently includes paths. Use `--heading` explicitly for grouped output. |
| `top:N` in a CLI content query was misleading | Use `-m N`; `top:N` remains a ranked-search option. |
| Starting an already running non-watching daemon with `--watch` could report success | Stop it, then start with `--watch`; mode mismatches now fail. |
| `daemon stop` could automatically kill before pending updates completed | It waits for persistence completion and reports failures. `--force` is explicit and may lose pending work. |
| Index/compact/remove could leave an obsolete daemon reader | Commands now reload or unload the daemon before reporting success. |

No-match exit status remains **0**. Invalid input and operation failures return
nonzero; automation must check the exit status. For unambiguous machine output,
use `--json`, or `-l -0` for paths. Plain text cannot safely delimit every legal
filename.

The index format is not intentionally changed by these repairs. Stricter readers
can now reject previously tolerated corruption; rebuilding from source repairs
such an index. Read/permission failures preserve the published generation rather
than silently excluding unreadable files.

See [semantics](SEMANTICS.md) for the full contract and [the fix record](audit-2026-09-18/FIXES.md)
for coverage and remaining limitations.
