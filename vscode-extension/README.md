# FXI for VS Code

Search a local FXI index from a sidebar and open matching files at their source
lines. The extension uses the FXI daemon; it does not bundle the Rust executable
or search a repository before it has been indexed.

## Build and install

Build/install FXI from the repository root first:

```sh
cargo install --path .
```

On macOS, keep the installed `fxi` and `fxid` executables together. The extension
continues to use `fxi`; that command locates its sibling daemon helper.

Then package the extension:

```sh
cd vscode-extension
npm ci
npm run build
npx @vscode/vsce package
code --install-extension fxi-0.1.0.vsix
```

Set `fxi.binaryPath` to the executable's full path if VS Code cannot find `fxi`.
The setting is an executable path, not a shell command with arguments.

## First search

1. Open a workspace folder.
2. Run **FXI: Build Index** from the Command Palette and wait for the task to finish.
3. Run **FXI: Start Daemon**. This requests a daemon with filesystem watching.
4. Open **FXI: Search** (`Ctrl+Alt+F`; `Cmd+Alt+F` on macOS).
5. Type a query and press Enter, or click Search.

An already running daemon keeps its existing watch configuration. If it was
started without watching, stop it and start it again to enable watching.
The extension currently searches the **first workspace folder** in a multi-root
workspace. Its daemon resolves the containing indexed codebase and returns paths
relative to that root; clicking a result opens the resolved file.

## Queries

Enter these directly into the panel; shell quotation rules do not apply:

| Query | Meaning |
|---|---|
| `error` | Case-insensitive literal substring. |
| `fn main` | Both terms anywhere in the same file. |
| `"fn main"` | Case-sensitive adjacent phrase. |
| `re:/fn\s+main/` | Regular expression. |
| `ext:rs error` | Search Rust-extension files. |
| `file:config*` | Basename glob. Use Files only for file discovery. |
| `foo \| bar` | Either term. |
| `foo -bar` | Files matching foo, excluding files matching bar. |

The panel performs content search, not the ranked TUI's filename-assisted search.
Use `file:` explicitly when looking for a filename. Malformed queries report an
error. Full syntax and coverage are in [the main README](../README.md#query-language)
and [SEMANTICS.md](../docs/SEMANTICS.md).

Use **Files only** to return one entry per matching file. The limit applies to
results, with `0` requesting no user limit; daemon safety limits still apply.
Context lines show surrounding source. Click a match to open its line. Unicode
highlighting respects the protocol's UTF-8 offsets.

## Commands and settings

| Command | Purpose |
|---|---|
| FXI: Search | Focus the sidebar input. |
| FXI: Build Index | Force-rebuild the first workspace folder's detected index; reload after success. |
| FXI: Reload Index | Reload the saved index in the daemon. It does not rebuild source changes. |
| FXI: Start Daemon | Start `fxi daemon start --watch` and connect. |
| FXI: Stop Daemon | Request graceful shutdown. |
| FXI: Daemon Status | Show daemon status and loaded roots. |

| Setting | Default | Meaning |
|---|---|---|
| `fxi.binaryPath` | `fxi` | Executable name or path, including paths containing spaces. |
| `fxi.defaultLimit` | `200` | Default output limit (0–10000; 0 requests unlimited). |
| `fxi.defaultContextLines` | `2` | Context on either side of a match (0–20). |

The status bar shows connection state. The extension reconnects after a daemon
disconnect. An old connection or completed earlier query cannot replace the
current query's results.

## Saved changes and limitations

Watching follows saved files, not unsaved editor buffers. Small updates can be
searchable in daemon memory before they are persisted to the on-disk index.
A graceful stop drains pending work. Filesystem notification delivery, publication,
compaction and bulk changes can still delay results; the
[freshness contract](../docs/SEMANTICS.md#freshness) explains this distinction.

Ignore rules and file eligibility apply, including exclusions for binary content,
large files, hidden paths and symlinks. This panel is not a replacement for an
unrestricted filesystem scan. Queries run when submitted; results are not
continuously rerun on each keystroke or external file change.

The daemon must be reachable from the extension host: Unix sockets on Linux/macOS
and named pipes on Windows. Remote-development environments require the binary,
index and daemon in the extension host's environment. Set `FXI_SOCKET` consistently
when using a custom endpoint; `fxi daemon socket-path` reports the CLI endpoint.

If startup fails, verify `fxi.binaryPath` and run `fxi daemon foreground --watch`
in that environment for diagnostics. If the panel reports no index, run Build
Index. If saved changes are missing, verify that the daemon was started with
`--watch` or run `fxi index` explicitly.

## Development

```sh
npm ci
npm test
npx tsc --noEmit
npm run build
```

Tests cover socket lifecycle and framing, task lifecycle, Unicode highlights,
webview messages, workspace handling and result ordering. Native platform behavior
still requires the corresponding operating system.
