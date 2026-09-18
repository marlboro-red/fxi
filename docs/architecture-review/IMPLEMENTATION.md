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
