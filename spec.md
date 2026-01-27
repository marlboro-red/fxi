

1. Vision & Goals

Build a terminal-first, ultra-fast code search engine capable of indexing and querying:
	•	Very large mono-repos (millions of files, tens of GBs)
	•	Mixed languages and encodings
	•	Frequently changing working trees

With:
	•	Sub-50ms warm query latency
	•	Incremental updates in seconds
	•	Disk-backed, memory-efficient structures
	•	Rich custom search syntax

The system prioritizes:

Dimension	Priority
Query latency	Critical
Index build speed	High
Incremental updates	High
Index size	Medium
RAM usage	Medium
Implementation complexity	Medium


⸻

2. Core Principles
	•	Disk-first design (mmap everything possible)
	•	Immutable index segments + delta updates
	•	Aggressive candidate narrowing before verification
	•	Streaming builds (no large in-memory maps)
	•	Human-predictable performance

⸻

3. Architecture Overview

+------------------+
|      TUI         |
+---------+--------+
          |
+---------v--------+
|   Query Engine   |
|  - Parser        |
|  - Planner       |
|  - Executor      |
+---------+--------+
          |
+---------v--------+
|   Index Reader   |
|  (mmap segments) |
+---------+--------+
          |
+---------v--------+
|  On-Disk Index   |
|  (segments)      |
+------------------+

Parallel:

+------------------+
|   Index Builder  |
|  + Delta Updates |
+------------------+


⸻

4. Indexing Strategy (Hybrid Two-Tier)

4.1 Primary Narrowing Layer — Trigram Presence Index
	•	Each file contributes unique trigrams (3-byte sequences)
	•	Stored as:

trigram -> sorted list of doc_ids

Properties:
	•	Substring search becomes postings intersection
	•	Extremely fast narrowing
	•	Good locality with mmap

Design decisions:
	•	Presence only (not every position)
	•	High-frequency trigrams treated as stop-grams

4.2 Secondary Precision Layer — Direct Content Verification

Once candidate documents are found:
	•	mmap file content
	•	perform:
	•	literal match
	•	phrase match
	•	regex
	•	line proximity checks

This avoids storing huge positional postings.

4.3 Auxiliary Token Index (Optional but default)

Tokenized:
	•	identifiers
	•	snake_case parts
	•	camelCase splits
	•	words

Used for:
	•	short queries (<3 chars)
	•	common code symbol searches

Structure:

token -> sorted doc_ids (+ optional line hits)

Much smaller than trigram index.

⸻

5. On-Disk Layout

.codesearch/
├── meta.json
├── docs.bin
├── paths.fst
├── segments/
│   ├── seg_0001/
│   │   ├── grams.dict
│   │   ├── grams.postings
│   │   ├── tokens.dict
│   │   ├── tokens.postings
│   │   ├── linemap.bin
│   │   └── files.bin
│   ├── seg_0002_delta/
│   └── ...
└── locks/


⸻

6. Core Data Structures

6.1 Document Table (docs.bin)

Per document:

Field	Type	Notes
doc_id	u32	contiguous
path_id	u32	reference into path store
size	u64	bytes
mtime	u64	unix ns
language	u16	enum
flags	u16	generated/vendor/binary
segment_id	u16	origin


⸻

6.2 Path Store (paths.fst)
	•	Minimal prefix compressed structure (FST or trie)
	•	Enables:
	•	fast path lookup
	•	wildcard filtering
	•	prefix scans

⸻

6.3 Trigram Dictionary (grams.dict)

Mapping:

u32 trigram -> (offset, length)

Stored sorted by trigram.

Access:
	•	binary search or MPH (minimal perfect hash)

⸻

6.4 Postings (grams.postings)

For each trigram:
	•	delta-encoded doc_ids
	•	compressed via:
	•	variable byte encoding
	•	or bitpacked blocks

Optimized for sequential scan + intersection.

⸻

6.5 Token Index (same layout as trigram)

Smaller, faster for short queries.

⸻

6.6 Line Map (linemap.bin)

Per document:
	•	list of newline byte offsets
	•	delta encoded u32

Supports:
	•	fast preview extraction
	•	line range filters
	•	near queries

⸻

7. Index Build Process

7.1 File Discovery
	•	Walk filesystem
	•	Respect:
	•	.gitignore
	•	tool ignore file
	•	Heuristics:
	•	skip binary
	•	skip > configurable size
	•	detect minified

⸻

7.2 Streaming Gram & Token Emission

For each file:
	•	compute unique trigrams
	•	extract tokens

Emit to spool:

(trigram, doc_id)
(token, doc_id)

No in-memory accumulation.

⸻

7.3 External Sort
	•	Sort by key then doc_id
	•	Chunked disk sort
	•	Merge phase

⸻

7.4 Reduction
	•	Deduplicate doc_ids per key
	•	Write compact postings
	•	Build dictionary

⸻

7.5 Stop-Gram Detection
	•	Count doc frequency
	•	Mark top N (default 512) as stop-grams
	•	Excluded from postings or stored separately

⸻

8. Incremental Updates

8.1 Segment Model (LSM-style)
	•	Base segment: full build
	•	Delta segments: changes only

Operations:
	•	added files → new doc_ids in delta segment
	•	modified files → new doc entries + old marked stale
	•	deleted files → tombstones

⸻

8.2 Query Across Segments
	•	Each segment queried independently
	•	Results merged
	•	Stale docs filtered via doc table flags

⸻

8.3 Compaction

Triggered when:
	•	delta count > threshold
	•	size ratio exceeded

Compaction merges segments into new base.

⸻

9. Query Language

9.1 Literals

foo bar
"exact phrase"


⸻

9.2 Regex

re:/foo.*bar/

Guarded by narrowing requirement.

⸻

9.3 Boolean

foo bar        -> AND
foo | bar      -> OR
-foo           -> NOT
(foo | bar) baz


⸻

9.4 Fields

Field	Meaning
path:	glob or prefix
ext:	file extensions
lang:	detected language
size:	>, <
mtime:	ranges


⸻

9.5 Location

line:120
line:120-180
near:foo,bar,20


⸻

9.6 Ranking Controls

^foo
+path:src
sort:recency
top:200


⸻

10. Query Execution Strategy

10.1 Planning

AST transformed into operators:
	•	Posting intersections
	•	Unions
	•	Exclusions
	•	Filters

Ordered by:
	•	smallest postings first
	•	stop-grams last

⸻

10.2 Narrowing Phase

Prefer:
	1.	token index
	2.	trigram index
	3.	scope filters

⸻

10.3 Verification Phase

For candidate docs:
	•	mmap content
	•	literal / phrase / regex check
	•	line constraints

⸻

10.4 Ranking

Score factors:
	•	match count
	•	proximity
	•	filename match
	•	directory depth
	•	recency

⸻

10.5 Early Termination

Stop when:
	•	top-K satisfied
	•	score drops below threshold

⸻

11. TUI Requirements

Panels
	•	Query input
	•	Results list
	•	File preview

Keybindings
	•	incremental typing
	•	next/prev result
	•	open in editor
	•	toggle regex/literal
	•	reindex

Visuals
	•	highlighted matches
	•	path dimming
	•	match counters

⸻

12. Performance Targets

Operation	Target
Cold startup	<300ms
Warm query	<50ms
Regex after narrowing	<200ms
Full build (1M files)	<5 min (SSD)
Delta update (100 files)	<1s
RAM usage	<500MB for huge repos


⸻

13. Memory Management
	•	mmap all postings and content
	•	only small dictionaries in RAM
	•	streaming build
	•	bounded candidate lists
	•	avoid storing positions in index

⸻

14. Extensibility Roadmap

Phase 1 (core)
	•	hybrid trigram + token index
	•	TUI
	•	incremental updates

Phase 2
	•	semantic layers (symbols)
	•	AST aware navigation
	•	LSP hooks

Phase 3
	•	multi-repo global index
	•	distributed shards

⸻

15. Failure Handling
	•	atomic segment writes
	•	checksum validation
	•	versioned meta
	•	auto rebuild on corruption

⸻

16. Benchmarking Plan

Measure:
	•	index size vs repo size
	•	query latency distributions
	•	delta update throughput
	•	compaction cost

Include:
	•	Linux kernel
	•	Chromium
	•	large mono-repo sample

⸻

17. Configuration

User configurable:
	•	ignored paths
	•	file size limits
	•	stop-gram count
	•	delta segment thresholds
	•	ranking weights

⸻

18. Security
	•	no execution of indexed content
	•	safe mmap bounds
	•	unicode normalization to avoid spoofing

⸻

19. Comparison Rationale

Tool	Weakness addressed
ripgrep	no index
ctags	symbol only
IDE search	heavy memory
Lucene	overkill, JVM

This tool optimizes specifically for code substrings + TUI workflow.

⸻

If you’d like, next I can:

• Turn this into a GitHub-style SPEC.md with diagrams
• Add a formal query grammar (EBNF)
• Design the compression formats in detail
• Propose a benchmarking harness
• Compare trigram vs 4-gram mathematically for size/speed
• Sketch a milestone roadmap with estimates

Just tell me which direction you want to deepen.
