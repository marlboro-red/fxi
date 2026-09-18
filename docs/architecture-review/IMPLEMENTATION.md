# Architecture implementation progress

## Fallible line maps

Stored line-map failures now remain errors across repeated accesses. The library
APIs changed to `get_line_map -> Result<Option<&Vec<u32>>>` and
`offset_to_line -> Result<Option<u32>>`: `None` means no stored map, and errors
mean unreadable/malformed evidence or an offset the format cannot represent.
Callers must propagate errors and explicitly handle missing evidence. Ordinary
CLI search derives line numbers from verified source and is unaffected.

Validation additionally rejects empty maps, nonzero initial offsets, duplicate
starts and trailing payload. Regression tests cover cached failures, absence,
line boundaries, large offsets and malformed maps. Existing pinned-generation
and stateful differential tests use the fallible API.

Revision binding, publication and streaming compaction remain
separate implementation steps; the critique describes proposals, not completed
features.

## Opt-in lean profile

`fxi index --profile lean PATH` performs a full rebuild without token extraction,
token dictionaries/postings/positions or stored line maps. The `full` profile
remains the new-index default. Metadata persists the selection; updates,
compaction and forced rebuilds preserve it. If metadata cannot be parsed, a
rebuild reports the problem and recovers with full evidence. `fxi stats` shows
the selected profile. Specifying `--profile full` restores optional evidence.

Public token lookup, token-substring lookup and positional phrase methods now
return `Result`; missing lean capabilities are explicit errors, including empty
indexes. Legacy metadata without a profile defaults to full and still requires
its token files. Unknown profile names are rejected by readers. Ordinary parsed
queries do not require these token APIs. Line-map lookup returns `Ok(None)` for
lean readers, including in-memory previews.

The generated CLI oracle runs the full option matrix for both profiles.
Additional tests cover incremental writes/deletes, compaction, rebuild profile
inheritance, explicit conversion, legacy metadata and missing required files,
and preview-only updates. These tests supplement existing corruption and
stateful differential suites; they do not prove every filesystem race absent.

Lean generations use format version 3; full generations retain version 2 and
legacy version 1 remains readable. Readers validate the version/profile pair.
This ensures pre-profile readers reject lean generations before interpreting
missing evidence, rather than allowing old maintenance paths to lose the
capability declaration. Explicit conversion to full rebuilds version 2 data.

## Single-capture source packs

Full builds and incremental processing now stream optional pack payloads from
exactly the owned bytes used for gram/token extraction. Payloads are written
into unpublished storage immediately; only per-file records and raw block hashes
survive the worker. Delta staging holds a generation lease and is removed on
failure or when a preview does not proceed to durable publication. Retained
source/encoding memory scales with extraction workers and the file-size limit,
not total corpus bytes. Existing posting buffers remain a separate memory cost.

The indexing read checks metadata before and after reading. A detected size,
mtime or Unix stamp change aborts the capture and preserves the published index.
Unix stamps include device/inode and ctime; other platforms use size/mtime checks.
This is not a filesystem snapshot or a guarantee of current membership after
publication. Source can still change after capture, and live stamp checks remain
required when reading packs.

New raw/compressed pack headers (`FXISRC04`/`FXISRC05`) distinguish evidence built
from the indexing capture. Legacy packs remain readable accelerators. Compaction
copies only revision-bound, checksum-validated captured content and retains its
original source stamp while remapping document IDs. Missing, corrupt or legacy
pack evidence is omitted instead of reconstructed from current source against
old postings. A full rebuild restores complete optional pack coverage. Inherited
orphan data files remain untouched, including while old readers have them mapped.

Tests cover same-size replacement after extraction but before full/delta
publication, restored mtimes during the metadata/read window, stale captures
through compaction, legacy/current pack mixtures and existing CLI output oracles.
Public writers accepting caller-supplied `ProcessedFile` values no longer reread
live files to manufacture optional packs without provenance.

[Single-capture measurements](../performance-single-capture/NOTES.md) record full
and lean builds, query validation, watched updates and a rejected document-ID
ordering experiment. Compressed live reads validate source freshness before
checksumming descriptors. Selective verification and streaming postings compaction
remain future work.
