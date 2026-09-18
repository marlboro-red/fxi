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

## Cleaning up abandoned registrations

Older test runs could leave registrations for temporary source directories in
the normal application data store. Tests now use isolated storage. Existing
registrations are not deleted automatically; preview and clean them explicitly:

```sh
fxi prune --dry-run
fxi prune --dry-run --verbose    # Inspect individual preserved/eligible entries
fxi prune
```

The command uses `FXI_INDEXES`, or the normal platform storage location (including
`FXI_APP_DATA` when set). Only recognized generation containers whose recorded
absolute source root reports `NotFound` are eligible. Live roots, dangling source
or ancestor symlinks, inaccessible paths, malformed metadata, unknown files, and
unrecognized layouts are preserved. One malformed registration does not stop the
rest of the scan. Filesystem errors are reported and produce a nonzero exit after
the summary; inspect `--verbose` output before retrying.

Prune rechecks the root and metadata under a nonblocking writer lock and takes
exclusive leases on every generation before removal. A daemon or search retaining
any generation makes that index busy, so it is skipped. Stop the daemon with
`fxi daemon stop`, allow in-flight searches/indexers to finish, and retry. Prune
does not disconnect readers or force a daemon shutdown. A dry-run reserves no
candidates; the apply pass checks them again.

Legacy layouts, including containers retaining legacy files after an upgrade,
have no reliable lease for old readers and are preserved by default. For older
test leaks in these layouts, first stop the daemon and **all FXI searches, TUIs,
library readers, and indexers**, then run:

```sh
fxi prune --dry-run --include-legacy
fxi prune --include-legacy
```

`--include-legacy` explicitly acknowledges that the process is offline: older
standalone readers cannot be discovered or excluded by a lease. An existing
daemon's status is checked, and its loaded roots are skipped; if a connected
daemon's status cannot be read, legacy cleanup is skipped. This guard does not
replace stopping all readers. Each legacy table still needs a matching recorded
root/container identity, valid metadata and layout, a missing source root, and
an available writer lock. Mixed containers additionally validate every generation
and require all generation leases. Live, malformed, unknown, and busy entries
remain preserved even with the flag.

Writer lock files beside removed containers are intentionally retained to
preserve lock identity. Reported logical bytes include each hard-link path, so
reclaimed filesystem space may be smaller.
