# FXI evidence guide

Start with the [project README](../README.md) for current behavior. These reports
answer different questions; historical measurements are not current guarantees.

| Question | Report |
| --- | --- |
| How does FXI perform on Chromium, and how much do coverage and cache conditions change the answer? | [Chromium comparison](performance-chromium/NOTES.md) |
| How does the stable-object experiment survive interrupted publication and sustained updates? | [Lifecycle validation](validation-stable-segments/NOTES.md) |
| Can checked search and immutable storage work together, and where do they lose? | [Stable segment experiment](performance-stable-segments/NOTES.md) |
| What reduced update memory, and which validation-reuse experiment failed? | [Reconciliation and rejected reuse](performance-stable-segments/NOTES.md#retained-classify-reconciliation-changes-during-the-walk) |
| What is the selective/absent-search experiment, and what integrity policy does it use? | [Query-local validation](performance-query-local/NOTES.md) |
| What did the preceding production architecture changes achieve? | [Architecture follow-up](performance-architecture-next/NOTES.md) |
| What structural weaknesses remain? | [Architecture critique](architecture-review/CRITIQUE.md) and [implementation status](architecture-review/IMPLEMENTATION.md) |
| What correctness defects were audited and repaired? | [Audit](audit-2026-09-18/AUDIT.md) and [fixes](audit-2026-09-18/FIXES.md) |
| What are source-pack capture and compression tradeoffs? | [Single-capture measurements](performance-single-capture/NOTES.md) |

Dated and numbered report directories retain experiment provenance, raw samples,
controls and rejected approaches. Keep their numbers scoped to their recorded
binary, corpus, platform and mode. Use the linked summaries to navigate them;
do not infer a universal ranking from one historical result table.

Raw evidence remains versioned so published comparisons can be checked. Superseded
reports are historical references, not parallel product documentation. New work
should update this guide and the current summary instead of adding another
unqualified headline benchmark to the README.
