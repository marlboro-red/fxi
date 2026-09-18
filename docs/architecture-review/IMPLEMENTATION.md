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

The lean profile, revision binding, publication and streaming compaction remain
separate implementation steps; the critique describes proposals, not completed
features.
