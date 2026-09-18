# Working on FXI

Use the current [README](README.md) for supported CLI syntax and search semantics.
Performance varies with corpus, query, output mode, cache state and platform;
avoid blanket speedup claims. See the [evidence guide](docs/README.md).

## Searching this repository

Check that the repository is indexed before relying on indexed searches:

```sh
fxi stats .
fxi index .                 # Build if missing; otherwise reconcile changes
fxi -l -F 'RoaringBitmap' .
```

A daemon is optional. For repeated searches, `fxi daemon start` reuses loaded
readers; `fxi daemon start --watch` additionally requests filesystem watching.
Use `fxi daemon status` to inspect the running service. An index reflects the
indexed snapshot; verify coverage/freshness when using search to audit code.

If FXI is unavailable or its index is missing, use `rg`. For correctness tests and
benchmarks, use an independent oracle such as ripgrep; FXI must not certify its
own results. Filesystem enumeration and direct reads remain appropriate tools.

```sh
fxi -F 'fn main'            # Literal text, including the space
fxi 'class Foo'             # Both terms must occur in the file
fxi 're:/fn\s+\w+/'         # Regex
fxi -i -F 'error'           # Case-insensitive literal
fxi -l 'ext:rs RoaringBitmap'
fxi -c 're:/return.*0/'
fxi -C 3 -F 'needle'
```

## Test storage

Tests must not create indexes in the user's application-data directory.

- Unit-test builds automatically use private process storage.
- Integration tests or Criterion fixtures calling the library must call
  `fxi::utils::app_data::isolate_test_storage()` before index operations.
- CLI subprocess fixtures must pass their own `FXI_INDEXES` and socket/runtime
  settings. `tests/support/mod.rs` provides a helper for this.
- Do not mutate process-wide environment variables from parallel tests.
- Normal process exit cleans process-owned test storage. Explicit fixture-owned
  index directories remain the fixture's responsibility.

CI snapshots real application data before tests and rejects changes afterward.
The dedicated storage regression checks concurrent initialization, cleanup and
explicit override handling.

## Performance work

Retain raw samples, binary hashes, corpus identity and exact-result checks. Keep
compilation and unrelated workloads out of timed runs. Separate standalone CLI,
resident CLI and direct API measurements. Report build/storage/update costs and
losing workloads alongside search improvements. Experimental formats and changed
integrity policies must be named explicitly rather than presented as defaults.
