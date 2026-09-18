# Windows source packs: research and proposed implementation

Research date: 2026-09-18. Implementation inspected at `02b74de`.
This is a design proposal, not an implemented feature or a Windows benchmark.

## Recommendation

Prototype a bounded, oplock-backed source cache on native Windows first. Reuse
that coherence mechanism for source packs after demonstrating correctness and
a useful performance gain. Do not enable Windows packs by simply replacing the
Unix stamp with size and Windows timestamps.

The promising gain is avoiding repeated live reads in a running daemon. Cheap,
correct reuse of a persisted pack on the first search after process startup is
a separate, unresolved problem. No speedup is established by this research.

## What the current implementation requires

- `src/index/source_pack.rs`: pack creation, opening and metadata stamps are
  Unix-only. The stamp includes device, inode, size, mtime and ctime. Pack data
  is immutable and checksummed; the mmap itself is not the portability barrier.
- `src/index/reader.rs`: `should_use_source_pack` selects packs for uncached
  searches with at least 128 candidates. The warm content cache currently uses
  a different path. On non-Unix systems, even a matching cache stamp triggers a
  live byte comparison in `revalidate_cached_bytes`.
- `tests/source_pack.rs`: integration coverage is currently Unix-only.
- Pack construction rereads sources after tokenization. A pack is not proof of
  the exact bytes used to construct postings. This proposal does not change
  stale-index candidate semantics or make a whole-repository snapshot promise.

Consequently, a daemon cache experiment is an architecture change, not merely
removing a platform guard. A metadata-only Windows shortcut would weaken the
existing live-byte verification behavior.

## Available evidence and its limits

| Mechanism | Useful role | Why it cannot stand alone |
| --- | --- | --- |
| Volume identity + 128-bit file ID | Detect a different file behind a pathname | Identity is not a content version |
| Size, last-write time, change time | Cheap rejection of stale entries | Timestamp updates and preservation need special care |
| USN change journal | Find potentially changed files; recover missed watcher events | Repeated writes can be coalesced; history can be discarded |
| Directory notifications | Trigger invalidation and maintenance | Overflow requires recovery; delivery is not a synchronous freshness barrier |
| Read oplock | Maintain an already validated cache while the lock remains valid | May be denied or broken; does not establish historical pack freshness |
| Live byte comparison | Establish whether current bytes equal packed bytes | Reads the source and can erase the intended I/O saving |

Use `GetFileInformationByHandleEx` with `FILE_ID_INFO` for volume and file ID,
and `FILE_BASIC_INFO` for last-write and change time. Obtain length and file
type from the same open handle. Windows creation time is not Unix ctime.
Microsoft documents the identity pair for comparing open files and distinguishes
data-write time from metadata-change time.
[File identity](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info),
[basic information](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_basic_info).

Windows can defer timestamp updates until modifying handles close. Native
file-information operations also support suppressing timestamp updates on a
handle, including change time. Therefore, adding change time is useful evidence
but not a general substitute for byte verification.
[File times](https://learn.microsoft.com/en-us/windows/win32/sysinfo/file-times),
[native basic information](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/ns-wdm-_file_basic_information).

The journal may record the first occurrence of a write reason while subsequent
writes under the same open handle contribute no distinct record. A last-USN
comparison must not be treated as a universal per-write content version.
Directory notification overflow also requires enumeration/recovery.
[Journal records](https://learn.microsoft.com/en-us/windows/win32/fileio/change-journal-records),
[ReadDirectoryChangesW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw).

Rust's documented Windows metadata extensions expose some needed fields only
through unstable APIs. Preserve our Rust 1.88 baseline using a small Windows API
binding module with owned handles; verify any binding dependency's MSRV before
choosing its version.
[Rust MetadataExt](https://doc.rust-lang.org/std/os/windows/fs/trait.MetadataExt.html).

## Proposed cache lifecycle

The following is an implementation hypothesis requiring native concurrency
tests and review of its synchronization rules before shipping.

1. Open the current source pathname for reading with shared read/write/delete
   access and overlapped I/O. Query identity from that handle. Prototype on local
   NTFS first; unsupported or denied requests retain the live-reader path.
2. Request a modern read oplock (`OPLOCK_LEVEL_CACHE_READ`). A successful request
   stays pending until a break; `ERROR_IO_PENDING` denotes a grant in this API.
   Direct requests on remote servers are unsupported, so SMB remains fallback.
   [Request API](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_request_oplock).
3. After grant, capture and validate live bytes. If using an existing pack,
   establish exact equality before trusting its range. Reject admission if the
   lock broke during validation. Granting a new lock does not prove the file
   has remained unchanged since the pack was written.
4. Bind each admitted entry to its owned handle, file identity, immutable source
   snapshot, generation lease and local cache epoch. Bound both bytes and open
   handles; do not allocate a thread per file.
5. Before serving a hit, reopen the current pathname with the required read
   access and compare its handle identity. This also avoids relying only on an
   old handle after access restrictions change. A file-content lock alone is
   insufficient evidence about replacement files or renamed parent directories.
6. Check the actual outstanding request's completion state around cached
   verification, together with synchronized invalidation state. Define the
   accepted snapshot's linearization point explicitly. A delayed completion
   worker's Boolean alone cannot establish freshness. If a break overlaps the
   attempt, discard the attempted cached result and retry through the live
   reader; bound retries for continually changing files.
7. On break, eviction or shutdown, invalidate before releasing state. Keep
   overlapped buffers alive until cancellation/completion is drained. Never
   couple break handling to a slow query or hold cache locks across source I/O.

Read oplocks do not stop writers while an application handles a break.
Read-handle oplocks also do not require writes to wait for acknowledgement.
Writable section creation breaks modern oplocks without waiting. This is why
the prototype must test kernel completion visibility and concurrent query
validation, rather than treating callbacks as a mutex around the file.
[Write behavior](https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/irp-mj-write2),
[writable mappings](https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/fs-filter-acquire-for-section-synchronization2).

Grant failures are normal, including incompatible existing activity. Start
with ordinary shared opens and fallback, avoiding the additional constraints of
atomic open-with-oplock until measurements justify it.
[Granting rules](https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/granting-oplocks).

## Pack format and rollout

First implement the coherence primitive independently of pack serialization and
use it experimentally in the Windows content cache. This isolates whether
avoiding byte rereads pays for handle opens, identity queries and lease checks.

If that succeeds, introduce an explicitly versioned pack format with a platform
and evidence-kind tag. Preserve reading existing Unix `FXISRC02` packs. Store
Windows identity and metadata as rejection filters, never as a persisted oplock.
Retain bounds checks, whole/block checksums, immutable generation lifetimes and
live fallback. Map only immutable pack files, not mutable source files.

A restarted daemon must revalidate contents before admitting a saved pack;
there is no surviving process lease. A conservative Windows pack reader can
compare all live bytes, but that alone provides no compelling speed argument.
Keep this behind an experimental setting until native measurements justify its
disk footprint. Test ReFS separately before expanding the initial NTFS scope;
retain fallback for remote filesystems, unavailable identities and unsupported
locking configurations.

## Required correctness experiments

Use deterministic barriers and native Windows processes, not sleep-only races.
Expose test counters for grants, breaks, actual cache/pack hits and fallbacks so
passing tests cannot merely demonstrate that the optimized path was unused.

- Same-length rewrites with restored last-write time; timestamp suppression;
  repeated writes while the writer handle remains open.
- Writable mappings established before grant and created after grant.
- Atomic replacement, deletion/recreation, hard-link edits, file and parent
  renames, and applicable junction/path-resolution cases.
- A break with completion processing deliberately paused while another thread
  attempts a cache hit. Check both positive and negative search results.
- Access removed after admission, unreadable files, invalid UTF-8, empty files,
  and unsupported/denied oplocks.
- Pack corruption and generation replacement while queries retain old leases.
- Cache eviction, cancellation, shutdown and restart; no stale persisted lease
  or outstanding I/O referencing freed memory.
- Existing literal, regex, case, count, context and output semantics using an
  independent live-source oracle on controlled stable snapshots. During edits,
  assert the explicitly defined per-file snapshot contract.

Do not enable the optimization if the kernel-completion and pathname checks
cannot establish the intended freshness contract. Ordinary live reads remain
the fallback, with their existing concurrent-edit behavior.

## Benchmark and decision gates

Compare the current Windows reader, the experimental oplock content cache, and
then oplock-backed packs on identical native Windows hardware and corpora.
Record corpus and tool revisions; use depth-1 clones for public benchmarks.

Separate first-process/first-query validation from repeated daemon queries and
distinguish OS-cache-warm from genuinely cold storage measurements. Include
absent terms, selective literals, common literals, phrases, regex, broad counts
and context output. Check exact result parity before accepting timings.

Measure median and tail latency, source bytes read, handle count, resident
memory, pack size/build cost, grant/hit/fallback rates and invalidation cost.
Include active editing and measure editor save latency: shifting query cost
onto writers is not an acceptable unexplained win. Keep normal Defender
settings recorded and consistent. Use CI for native correctness; use a stable
machine for performance conclusions.

Proceed to pack integration only if the simpler coherent content cache delivers
a repeatable gain at acceptable resource cost. If path opens dominate, research
directory/namespace coherence separately rather than deleting identity checks.
If cold searches remain slower, report that limitation explicitly. The current
macOS source-pack measurements cannot establish a Windows advantage.
