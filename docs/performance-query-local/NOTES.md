# Checked dictionaries and query-local postings experiment

This follows `fad7919`. The default reader remains strict. `FXI_QUERY_LOCAL=1`
opts into an experimental storage and search policy; it is not a blanket speed
claim or a change to default corruption handling.

## Design and correctness boundary

The first `FXIGRAM1` prototype stored a whole-dictionary checksum and one 64-bit
XXH3 checksum per posting. The current `FXIGRAM2` format divides the sorted
vocabulary into pages of at most 512 entries. A checksummed root records the
complete entry count, posting-file length, and every page's first/last key and
checksums for its dictionary records and posting-hash slice. The root's ordered,
disjoint ranges prove outer-range/interpage absence. A key within a page requires
checking that entire page before lookup. Fixed partitions cover every record;
page endpoints, ordering and posting ranges are validated before use.

Publication creates evidence only after validating the staging segment with the
strict reader. Existing inherited evidence is validated, never refreshed to bless
changed bytes. All new files use the existing sync-before-CURRENT protocol.
Version 1 remains readable; inherited v1 segments keep the whole-dictionary cost.
A forced rebuild generates v2 pages for every segment.

Experimental readers validate the root at open. Optional `grams.bloom-check`
evidence binds that checked root digest to an XXH3 digest of a coverage-checked
Bloom filter's ordered words, probe count and length. Publication verifies every dictionary key is represented before
issuing this proof. Readers hash the actual root and loaded filter, without file
stamps; absent, damaged or mismatched proof disables Bloom pruning and falls back
to checked pages. This adds 32 bytes per segment and retains safe fallback for
legacy filters. Each
accessed page and posting is checksummed; postings are fully validated for count,
ordering and document membership before decoding, including before an intersection
can stop decoding early. Successful checks are cached within that immutable
reader using one byte per posting plus small page state; failures remain errors.
Missing sidecars use the legacy eager path; malformed roots fail at open and
malformed dependent pages fail on use. Public eager opening and compaction
validate every page and posting, including checksums when evidence exists.

An unrelated damaged posting need not fail a query that does not depend on it.
`fxi stats PATH` uses the eager reader and checks all gram postings and full-profile
token evidence; it is not a source-to-index completeness audit or a check of every
lazy optional artifact. Invalid evidence never becomes an empty result. Setting
both experimental flags disables timestamp-based `FXI_NEGATIVE_ROUTING` preflight,
so that it cannot bypass the checked reader.

Checksums detect accidental damage relative to publication bytes; they do not
provide authentication against coordinated rewriting. Document/path/metadata
validation retains its existing structural policy. Immutable published files
remain a reader-lifetime requirement. This experiment does not claim to discover
unindexed source changes.

The public Rust `get_trigram_docs` and `get_trigram_docs_with_bloom` APIs now return
`Result<RoaringBitmap>` so errors propagate through every gram execution path.
CLI syntax and output are unchanged. Library consumers must handle the result;
the repository examples have been updated.

## Usage

Build experimental evidence explicitly:

```sh
FXI_QUERY_LOCAL=1 fxi index --force --profile lean PATH
FXI_QUERY_LOCAL=1 fxi -l 're:/selective_symbol/' -p PATH
```

Use the flag on incremental indexing and compaction to generate evidence for new
segments. Mixed generations remain readable: a segment without a sidecar is
validated eagerly. Omit the flag for strict searches. Existing formats and
posting encodings are retained; the sidecar is additive.

`FXI_DEBUG=1` reports metadata/pinning, document/path loading, membership setup,
segment loading, and final setup, plus per-segment mapping/checksum and validation
times. Parallel segment times overlap and must not be added as elapsed time.

## Validation

Tests cover every single-byte mutation and every truncation of a small dictionary
and sidecar through opening or dependent lookup, empty/512/513-entry page boundaries,
partial final pages and interpage gaps, later-page dictionary/hash corruption, truncated posting files, structurally valid posting damage, damage
beyond filtered decoding's early exit, repeated errors, eager fallback without
evidence, independent queries against unrelated damage, and strict rejection.
CLI comparisons exercise full/lean multisegment indexes, positive/absent and
compound queries, case folding, Unicode, filters, files/counts/content output,
edits/deletions and compaction. Damaged inherited evidence prevents publication
and preserves CURRENT. Both experiment flags together cannot bypass corruption.

## Measurements

Measurements use the existing controlled Linux-source corpus on the Apple M2 Max,
64 GiB, macOS, warm filesystem. Builds alternate three times, use lean compressed
source packs, and verify corpus manifests before/after plus exact ripgrep result
sets. Query campaigns compare every complete result against ripgrep outside the
timed process interval. No compilation runs during timed measurements.

Raw build results: [builds.json](builds.json). Raw diagnostic trace:
[initial-trace.json](initial-trace.json). The trace was collected with test work
running and is diagnostic only, not a speed comparison.

The first whole-dictionary prototype measured 34.71 → 21.67 ms for absence,
36.38 → 23.55 ms for a selective symbol, and 44.97 → 32.28 ms for a phrase
(21 paired samples each). Broad query controls also improved. See
[whole-dictionary query samples](queries-checked-dictionary.json).

Build medians were 3.578 → 3.735 s; the first candidate sample was 7.381 s,
followed by 3.735 and 3.645 s. That outlier is retained, not attributed to a
specific cause. Packed index size increased from 941,356,724 to 975,095,412 bytes
(+33,738,688 bytes, about 3.6%). These results motivate paging the dictionary
rather than treating the remaining whole-dictionary scan as solved.


## Legacy Bloom checksum correction

Review found that the existing rotating-XOR Bloom checksum permits swaps of
words 64 positions apart without changing the checksum. Such a swap can remove
a required probe bit. Strict readers now corroborate Bloom negatives against
the fully validated dictionary before excluding a segment. Experimental proofs
use an independent ordered-content XXH3 digest, so a reordered filter disables
pruning and falls back to checked pages. A regression deliberately preserves the
legacy checksum while removing a match's probe bit and checks both reader modes.
The legacy on-disk Bloom format remains readable.
