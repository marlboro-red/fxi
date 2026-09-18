# Longer-gram precision experiment

This offline probe asks how much candidate verification a more selective index
could avoid. It does **not** implement an index or measure a faster search path.
The existing immutable common Linux fixture and FXI index are reused. Sources
must match that index. Every candidate result contains the complete independent
ripgrep fixed-string file set; any missing true match aborts the experiment.

The lab intersects exact source presence of all 4-, 5-, 6- and 8-byte grams from
an ASCII literal. Source and query bytes are ASCII-lowercased, giving a safe
superset for exact case-sensitive matching. A second intersection uses only
**boundary-crossing grams**: windows containing both an ASCII alphanumeric byte
and a delimiter. Underscores count as delimiters. This is a selection criterion,
not tokenization: token length, partial token endpoints and camel-case rules do
not affect the substring proof. Unicode-insensitive matching is out of scope.

| Literal | Current candidates | True files | Width | All grams | Boundary-crossing only |
|---|---:|---:|---:|---:|---:|
| `struct file_operations` | 3,787 | 1,240 | 4 | 1,583 | 1,605 |
| `struct file_operations` | 3,787 | 1,240 | 5 | 1,501 | 1,501 |
| `struct file_operations` | 3,787 | 1,240 | 6 | 1,318 | 1,318 |
| `struct file_operations` | 3,787 | 1,240 | 8 | 1,240 | 1,240 |
| `static const` | 23,547 | 21,570 | 4 | 22,171 | 22,260 |
| `static const` | 23,547 | 21,570 | 5 | 21,607 | 21,609 |
| `static const` | 23,547 | 21,570 | 6 | 21,572 | 21,572 |
| `static const` | 23,547 | 21,570 | 8 | 21,572 | 21,572 |
| `const struct` | 36,769 | 30,667 | 4 | 31,269 | 31,321 |
| `const struct` | 36,769 | 30,667 | 5 | 30,701 | 30,701 |
| `const struct` | 36,769 | 30,667 | 6 | 30,669 | 30,669 |
| `const struct` | 36,769 | 30,667 | 8 | 30,668 | 30,668 |

For `struct file_operations`, six-byte boundary-crossing constraints reduce the
candidate set from 3,787 to 1,318, close to the 1,240 actual matching files.
Eight-byte constraints reach 1,240. The most selective individual eight-byte
window, `t file_o`, alone leaves 1,254 candidates. This is stronger than the
previous complete-interior-token experiment's 2,853 candidates. Broad phrases
have much less false-positive work to remove, so their expected benefit is small.

The baseline candidate set here is 3,787, matching the previous offline token
lab. Earlier historical query profiles reported 3,788 on their fixture/runtime;
these distinct measurements should not be silently conflated.

## What remains unproven

A production implementation needs a persisted, complete posting list for each
selected longer gram, or an explicitly conservative fallback when absent.
Selective storage must never interpret "not stored" as "not present". Proposed
next experiment: measure dictionary cardinality, compressed posting bytes and
build cost for boundary-crossing six/eight-byte grams; then benchmark decoding
and intersection plus unchanged source freshness checks against existing queries.
Keeping all long grams without a budget could substantially enlarge the index.
Per-document Bloom filters also need occupancy/error measurements; small fixed
filters may saturate on large files and erase the precision benefit.

Metadata checks cannot simply be skipped for candidate files: live source changes
must still invalidate source snapshots. Cached previous query results or watching
alone would change the current freshness contract. The existing index's normal
stale-candidate visibility limitations would also apply to any added postings.
No production query/index code changed, no performance result is claimed, and
there is no claim that longer grams or these boundary predicates are novel.

## Reproduce and validation

```sh
cargo test --example phrase_gram_lab
cargo build --release --example phrase_gram_lab
FXI_INDEXES=/private/var/folders/6w/jph_9hyd71gbqw76h25h2zz00000gn/T/fxi-indexer-comparison-4tmf41uv/fxi \
  target/release/examples/phrase_gram_lab \
  /private/var/folders/6w/jph_9hyd71gbqw76h25h2zz00000gn/T/fxi-common-indexer-corpus-1td4bkex \
  'struct file_operations' 'static const' 'const struct' \
  > docs/performance-phrase-grams/precision.json
```

The substring-preservation test passed across literal widths, token fragments,
punctuation and Unicode surrounding context. All 24 candidate sets (three
queries × four widths × two constraint choices) retain the ripgrep matches.
[Raw precision counts](precision.json), [lab](../../examples/phrase_gram_lab.rs).
The lab holds candidate source bytes in memory to perform the offline comparison;
that allocation is not a proposal for production resource use.
