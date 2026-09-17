//! Offline feasibility experiment for selective byte-position sidecars.
//!
//! FXI_INDEXES=... cargo run --release --example position_lab -- ROOT [LITERAL ...]
//! Reads one source file at a time; never modifies the index. The corpus must be
//! immutable and match the existing index. This measures recall/filter quality
//! and a precisely specified hypothetical encoding, NOT production latency.
use ahash::AHashMap;
use anyhow::{Context, Result, ensure};
use fxi::index::reader::IndexReader;
use fxi::index::types::{DocId, Trigram, bytes_to_trigram};
use roaring::RoaringBitmap;
use serde::Serialize;
use std::path::Path;

const MAX_CAP: usize = 16;
const CAPS: [usize; 3] = [1, 4, MAX_CAP];
const DEFAULT_LITERALS: [&str; 12] = [
    "struct file_operations",
    "const struct file_operations",
    "static const struct",
    "static inline",
    "return -EINVAL",
    "unsigned long flags",
    "void __iomem",
    "if (unlikely(",
    "MODULE_DESCRIPTION(",
    "folio_wait_bit_common",
    "return",
    "auditNonexistentSymbol94283",
];

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Selection {
    HashEighth,
    HashQuarter,
    Boundary,
    All,
}
impl Selection {
    fn includes(self, gram: Trigram) -> bool {
        match self {
            Self::HashEighth => mix(gram) & 7 == 0,
            Self::HashQuarter => mix(gram) & 3 == 0,
            Self::Boundary => {
                let bytes = [(gram >> 16) as u8, (gram >> 8) as u8, gram as u8];
                bytes
                    .iter()
                    .any(|b| b.is_ascii() && !b.is_ascii_alphanumeric())
            }
            Self::All => true,
        }
    }
}

// Fixed, corpus/query-independent integer mixer. Sampling never decides whether
// a file matches: absent sampled evidence is always Unknown.
fn mix(mut value: u32) -> u32 {
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^ (value >> 16)
}

#[derive(Clone, Copy, Debug)]
struct Occurrences {
    offsets: [u32; MAX_CAP],
    // MAX_CAP + 1 means overflow; no prefix may be treated as complete.
    count: usize,
    // Complete residue summary: continues collecting after exact-list overflow.
    residues: [u64; 4],
}
impl Default for Occurrences {
    fn default() -> Self {
        Self {
            offsets: [0; MAX_CAP],
            count: 0,
            residues: [0; 4],
        }
    }
}
impl Occurrences {
    fn push(&mut self, offset: u32) {
        let residue = (offset & 255) as usize;
        self.residues[residue / 64] |= 1 << (residue % 64);
        if self.count < MAX_CAP {
            self.offsets[self.count] = offset;
        }
        self.count = (self.count + 1).min(MAX_CAP + 1);
    }
    fn complete(&self, cap: usize) -> Option<&[u32]> {
        (self.count > 0 && self.count <= cap).then(|| &self.offsets[..self.count])
    }
}

/// Fold the complete 256-bit summary to a smaller power-of-two modulus.
fn residue_mask(occurrences: &Occurrences, bits: u32) -> [u64; 4] {
    assert!(matches!(bits, 64 | 128 | 256));
    let words = bits as usize / 64;
    let mut result = [0; 4];
    for (index, word) in occurrences.residues.iter().copied().enumerate() {
        result[index % words] |= word;
    }
    result
}

fn all_residues(bits: u32) -> [u64; 4] {
    let mut result = [0; 4];
    result[..bits as usize / 64].fill(u64::MAX);
    result
}

/// An occurrence at byte p implies a potential literal start at p-query_offset.
/// Right rotation translates the residue set, including wrap across u64 words.
fn align_residues(mask: [u64; 4], bits: u32, query_offset: u32) -> [u64; 4] {
    let words = bits as usize / 64;
    let shift = (query_offset % bits) as usize;
    let mut result = [0; 4];
    for (index, word) in result.iter_mut().take(words).enumerate() {
        let source = (index + shift / 64) % words;
        *word = mask[source] >> (shift % 64);
        if !shift.is_multiple_of(64) {
            *word |= mask[(source + 1) % words] << (64 - shift % 64);
        }
    }
    result
}

fn scan_positions(content: &[u8]) -> AHashMap<Trigram, Occurrences> {
    let mut positions = AHashMap::<Trigram, Occurrences>::new();
    for (offset, window) in content.windows(3).enumerate() {
        positions
            .entry(bytes_to_trigram(window[0], window[1], window[2]))
            .or_default()
            .push(offset as u32);
    }
    positions
}

#[derive(Clone, Copy, Debug)]
struct Anchor {
    gram: Trigram,
    offset: u32,
    frequency: u64,
}

fn query_anchors(literal: &[u8], frequency: impl Fn(Trigram) -> u64) -> Vec<Anchor> {
    literal
        .windows(3)
        .enumerate()
        .map(|(offset, bytes)| {
            let gram = bytes_to_trigram(bytes[0], bytes[1], bytes[2]);
            Anchor {
                gram,
                offset: offset as u32,
                frequency: frequency(gram),
            }
        })
        .collect()
}

fn choose_pair(anchors: &[Anchor]) -> Vec<Anchor> {
    let mut sorted = anchors.to_vec();
    sorted.sort_unstable_by_key(|a| (a.frequency, a.offset));
    let Some(first) = sorted.first().copied() else {
        return Vec::new();
    };
    // Prefer a nonoverlapping second gram at equal candidate frequency ordering.
    // Ranking affects efficiency only; every chosen gram remains mandatory.
    let second = sorted
        .iter()
        .copied()
        .skip(1)
        .find(|a| a.offset.abs_diff(first.offset) >= 3)
        .or_else(|| sorted.get(1).copied());
    second.map_or_else(|| vec![first], |second| vec![first, second])
}

/// Missing, overflowed or invalidated evidence is represented by None.
/// Complete offsets for every supplied gram are necessary: an arbitrary prefix
/// cannot establish absence. Corrupt sidecars must be invalidated by integrity
/// checks before this function; structurally valid corruption is not detectable
/// from relative positions alone.
fn possible_start(anchors: &[(u32, &[u32])], literal_len: u32, file_len: u32) -> bool {
    possible_start_with_residues(anchors, literal_len, file_len, None)
}

fn possible_start_with_residues(
    anchors: &[(u32, &[u32])],
    literal_len: u32,
    file_len: u32,
    residues: Option<([u64; 4], u32)>,
) -> bool {
    let Some((base_offset, base_positions)) = anchors.iter().min_by_key(|a| a.1.len()) else {
        return residues.is_none_or(|(mask, _)| mask.iter().any(|word| *word != 0));
    };
    base_positions
        .iter()
        .filter_map(|p| p.checked_sub(*base_offset))
        .any(|start| {
            start
                .checked_add(literal_len)
                .is_some_and(|end| end <= file_len)
                && residues.is_none_or(|(mask, bits)| {
                    let residue = (start % bits) as usize;
                    mask[residue / 64] & (1 << (residue % 64)) != 0
                })
                && anchors.iter().all(|(query_offset, positions)| {
                    start
                        .checked_add(*query_offset)
                        .is_some_and(|target| positions.binary_search(&target).is_ok())
                })
        })
}

#[derive(Clone, Copy)]
struct Verdict {
    keep: bool,
    known: usize,
}

fn filter(
    positions: Option<&AHashMap<Trigram, Occurrences>>,
    anchors: &[Anchor],
    cap: usize,
    literal_len: u32,
    file_len: u32,
    require_pair: bool,
) -> Verdict {
    let known: Vec<_> = anchors
        .iter()
        .filter_map(|a| {
            positions?
                .get(&a.gram)?
                .complete(cap)
                .map(|p| (a.offset, p))
        })
        .collect();
    Verdict {
        keep: (require_pair && known.len() < 2) || possible_start(&known, literal_len, file_len),
        known: known.len(),
    }
}

/// Hybrid evidence uses exact lists up to cap; overflow gets a complete residue
/// mask unless that mask is saturated. Missing/saturated evidence is all ones.
/// Modulo aliases may retain false positives, but cannot remove a true match.
fn filter_hybrid(
    positions: Option<&AHashMap<Trigram, Occurrences>>,
    anchors: &[Anchor],
    cap: usize,
    bits: u32,
    literal_len: u32,
    file_len: u32,
    require_pair: bool,
) -> Verdict {
    let mut intersection = all_residues(bits);
    let mut exact = Vec::new();
    let mut known = 0;
    for anchor in anchors {
        let Some(occurrences) = positions.and_then(|p| p.get(&anchor.gram)) else {
            continue;
        };
        if occurrences.count == 0 {
            continue; // An absent/default record is not evidence of absence.
        }
        let mask = residue_mask(occurrences, bits);
        if let Some(offsets) = occurrences.complete(cap) {
            exact.push((anchor.offset, offsets));
        } else if mask == all_residues(bits) {
            continue; // No record is stored for saturated summaries.
        }
        known += 1;
        let aligned = align_residues(mask, bits, anchor.offset);
        for (word, evidence) in intersection.iter_mut().zip(aligned) {
            *word &= evidence;
        }
    }
    Verdict {
        keep: (require_pair && known < 2)
            || possible_start_with_residues(
                &exact,
                literal_len,
                file_len,
                Some((intersection, bits)),
            ),
        known,
    }
}

fn varint_bytes(mut value: u32) -> u64 {
    let mut length = 1;
    while value >= 128 {
        value >>= 7;
        length += 1;
    }
    length
}

fn position_bytes(offsets: &[u32]) -> u64 {
    let mut previous = 0;
    let mut bytes = varint_bytes(offsets.len() as u32);
    for &offset in offsets {
        bytes += varint_bytes(offset - previous);
        previous = offset;
    }
    bytes
}

#[derive(Default, Serialize)]
struct Space {
    retained_doc_gram_pairs: u64,
    retained_positions: u64,
    overflow_doc_gram_pairs: u64,
    residue_doc_gram_pairs: u64,
    saturated_doc_gram_pairs: u64,
    unselected_doc_gram_pairs: u64,
    payload_bytes: u64,
    dictionary_entries: u64,
    segment_headers_bytes: u64,
    #[serde(skip)]
    previous_docs: AHashMap<Trigram, DocId>,
}
impl Space {
    fn begin_segment(&mut self) {
        self.previous_docs.clear();
        self.segment_headers_bytes += 4;
    }
    fn account(&mut self, id: DocId, gram: Trigram, offsets: &[u32]) {
        let previous = self.previous_docs.insert(gram, id);
        if previous.is_none() {
            self.dictionary_entries += 1;
        }
        self.payload_bytes += varint_bytes(id - previous.unwrap_or(0)) + position_bytes(offsets);
        self.retained_doc_gram_pairs += 1;
        self.retained_positions += offsets.len() as u64;
    }
    fn account_residue(&mut self, id: DocId, gram: Trigram, bits: u32) {
        let previous = self.previous_docs.insert(gram, id);
        if previous.is_none() {
            self.dictionary_entries += 1;
        }
        // Hybrid record: delta doc ID, one-byte kind tag, fixed-width raw mask.
        self.payload_bytes += varint_bytes(id - previous.unwrap_or(0)) + 1 + u64::from(bits / 8);
        self.retained_doc_gram_pairs += 1;
        self.residue_doc_gram_pairs += 1;
    }
    fn total_bytes(&self) -> u64 {
        // Same 20-byte record shape as grams.dict, but in a separate sidecar:
        // gram u32, offset u64, payload length u32, known-document count u32.
        self.payload_bytes + self.dictionary_entries * 20 + self.segment_headers_bytes
    }
}

#[derive(Default, Serialize)]
struct FilterStats {
    surviving_files: u64,
    surviving_bytes: u64,
    candidates_with_two_known_anchors: u64,
    candidates_without_known_anchors: u64,
    known_anchor_visits: u64,
}
impl FilterStats {
    fn record(&mut self, verdict: Verdict, bytes: u64) {
        if verdict.keep {
            self.surviving_files += 1;
            self.surviving_bytes += bytes;
        }
        self.candidates_with_two_known_anchors += u64::from(verdict.known >= 2);
        self.candidates_without_known_anchors += u64::from(verdict.known == 0);
        self.known_anchor_visits += verdict.known as u64;
    }
}

#[derive(Serialize)]
struct QueryStats {
    literal: String,
    candidate_files: u64,
    candidate_bytes: u64,
    matching_files: u64,
    matching_bytes: u64,
    selected_query_anchors: usize,
    pair_query_offsets: Vec<u32>,
    fixed_rare_pair: FilterStats,
    all_known_anchors: FilterStats,
    #[serde(skip)]
    anchors: Vec<Anchor>,
    #[serde(skip)]
    pair: Vec<Anchor>,
}

#[derive(Serialize)]
struct Policy {
    selection: Selection,
    occurrence_cap: usize,
    residue_bits: Option<u32>,
    space: Space,
    estimated_sidecar_bytes: u64,
    queries: Vec<QueryStats>,
}

struct Query {
    literal: String,
    candidates: RoaringBitmap,
    anchors: Vec<Anchor>,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .context("Usage: position_lab ROOT [LITERAL ...]")?;
    let mut literals: Vec<String> = args.collect();
    if literals.is_empty() {
        literals = DEFAULT_LITERALS.iter().map(|s| (*s).into()).collect();
    }
    for literal in &literals {
        ensure!(
            !literal.is_empty() && !literal.contains(['\n', '\r']),
            "Only nonempty case-sensitive line-local literals are supported"
        );
        ensure!(literal.len() <= u32::MAX as usize, "literal too large");
    }
    let reader = IndexReader::open(Path::new(&root))?;
    let mut frequencies = AHashMap::new();
    for literal in &literals {
        for gram in fxi::utils::query_trigrams(literal) {
            frequencies
                .entry(gram)
                .or_insert_with(|| reader.get_trigram_docs(gram).len());
        }
    }
    let queries: Vec<_> = literals
        .into_iter()
        .map(|literal| {
            let grams: Vec<_> = fxi::utils::query_trigrams(&literal)
                .into_iter()
                .filter(|g| !reader.is_stop_gram(*g))
                .collect();
            let candidates = if grams.is_empty() {
                reader.valid_doc_ids().clone()
            } else {
                reader.get_trigram_docs_with_bloom(&grams) & reader.valid_doc_ids()
            };
            let anchors = query_anchors(literal.as_bytes(), |gram| frequencies[&gram]);
            Query {
                literal,
                candidates,
                anchors,
            }
        })
        .collect();
    let mut policies = Vec::new();
    for selection in [
        Selection::HashEighth,
        Selection::HashQuarter,
        Selection::Boundary,
        Selection::All,
    ] {
        // Exact-only controls plus cap-four hybrid summaries. Each policy is
        // an independent hypothetical sidecar; storage is never summed across policies.
        let variants = CAPS
            .into_iter()
            .map(|cap| (cap, None))
            .chain([64, 128, 256].into_iter().map(|bits| (4, Some(bits))));
        for (occurrence_cap, residue_bits) in variants {
            let stats = queries
                .iter()
                .map(|query| {
                    let anchors: Vec<_> = query
                        .anchors
                        .iter()
                        .copied()
                        .filter(|a| selection.includes(a.gram))
                        .collect();
                    let pair = choose_pair(&anchors);
                    QueryStats {
                        literal: query.literal.clone(),
                        candidate_files: 0,
                        candidate_bytes: 0,
                        matching_files: 0,
                        matching_bytes: 0,
                        selected_query_anchors: anchors.len(),
                        pair_query_offsets: pair.iter().map(|a| a.offset).collect(),
                        fixed_rare_pair: FilterStats::default(),
                        all_known_anchors: FilterStats::default(),
                        anchors,
                        pair,
                    }
                })
                .collect();
            policies.push(Policy {
                selection,
                occurrence_cap,
                residue_bits,
                space: Space::default(),
                estimated_sidecar_bytes: 0,
                queries: stats,
            });
        }
    }
    let mut documents: Vec<_> = reader.documents().iter().filter(|d| d.is_valid()).collect();
    documents.sort_unstable_by_key(|d| (d.segment_id, d.doc_id));
    let mut segment = None;
    let mut files = 0u64;
    let mut source_bytes = 0u64;
    let mut peak_file_distinct_grams = 0usize;
    let mut peak_segment_distinct_lists = 0usize;
    for document in documents {
        if segment != Some(document.segment_id) {
            for policy in &mut policies {
                policy.space.begin_segment();
            }
            segment = Some(document.segment_id);
        }
        let path = reader
            .get_full_path(document)
            .context("invalid document path")?;
        let before =
            std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        let content = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let after = std::fs::metadata(&path)?;
        ensure!(
            before.len() == after.len() && before.modified()? == after.modified()?,
            "source changed during the probe: {}",
            path.display()
        );
        ensure!(
            content.len() as u64 == document.size && content.len() <= u32::MAX as usize,
            "source size differs from index or cannot fit u32 offsets: {}",
            path.display()
        );
        std::str::from_utf8(&content).context("indexed source is no longer UTF-8")?;
        let positions = scan_positions(&content);
        peak_file_distinct_grams = peak_file_distinct_grams.max(positions.len());
        let size = content.len() as u64;
        for (&gram, occurrences) in &positions {
            for policy in &mut policies {
                if !policy.selection.includes(gram) {
                    policy.space.unselected_doc_gram_pairs += 1;
                } else if let Some(offsets) = occurrences.complete(policy.occurrence_cap) {
                    policy.space.account(document.doc_id, gram, offsets);
                    // Exact-only records need no tag; hybrid records distinguish
                    // exact lists (tag 0) from fixed-width residue masks (tag 1).
                    policy.space.payload_bytes += u64::from(policy.residue_bits.is_some());
                } else {
                    policy.space.overflow_doc_gram_pairs += 1;
                    if let Some(bits) = policy.residue_bits {
                        if residue_mask(occurrences, bits) == all_residues(bits) {
                            policy.space.saturated_doc_gram_pairs += 1;
                        } else {
                            policy.space.account_residue(document.doc_id, gram, bits);
                        }
                    }
                }
            }
        }
        for (query_number, query) in queries.iter().enumerate() {
            // Independent substring oracle; literals cannot cross line boundaries.
            let matches = memchr::memmem::find(&content, query.literal.as_bytes()).is_some();
            let candidate = query.candidates.contains(document.doc_id);
            ensure!(
                !matches || candidate,
                "existing index misses {:?} in {}; rebuild an immutable fixture",
                query.literal,
                path.display()
            );
            for policy in &mut policies {
                let stats = &mut policy.queries[query_number];
                if matches {
                    stats.matching_files += 1;
                    stats.matching_bytes += size;
                }
                if !candidate {
                    continue;
                }
                stats.candidate_files += 1;
                stats.candidate_bytes += size;
                let evaluate = |anchors: &[Anchor], require_pair| {
                    if let Some(bits) = policy.residue_bits {
                        filter_hybrid(
                            Some(&positions),
                            anchors,
                            policy.occurrence_cap,
                            bits,
                            query.literal.len() as u32,
                            content.len() as u32,
                            require_pair,
                        )
                    } else {
                        filter(
                            Some(&positions),
                            anchors,
                            policy.occurrence_cap,
                            query.literal.len() as u32,
                            content.len() as u32,
                            require_pair,
                        )
                    }
                };
                let pair = evaluate(&stats.pair, true);
                let all = evaluate(&stats.anchors, false);
                ensure!(
                    !matches || (pair.keep && all.keep),
                    "position filter lost {:?} in {} for {:?}/{}",
                    query.literal,
                    path.display(),
                    policy.selection,
                    policy.occurrence_cap
                );
                stats.fixed_rare_pair.record(pair, size);
                stats.all_known_anchors.record(all, size);
            }
        }
        peak_segment_distinct_lists = peak_segment_distinct_lists
            .max(policies.iter().map(|p| p.space.previous_docs.len()).sum());
        files += 1;
        source_bytes += size;
    }
    for policy in &mut policies {
        policy.estimated_sidecar_bytes = policy.space.total_bytes();
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "root": root, "files": files, "source_bytes": source_bytes,
            "default_query_suite": DEFAULT_LITERALS,
            "peak_file_distinct_grams": peak_file_distinct_grams,
            "peak_segment_dictionary_states_across_all_policies": peak_segment_distinct_lists,
            "policies": policies,
            "method": "Streaming source scan with all corpus grams, not query-specific storage. Exact-only policies omit lists above the occurrence cap; hybrid policies retain complete modulo summaries as described separately. Missing/unselected evidence is Unknown. Fixed rare pair selects two low-document-frequency selected grams, preferring nonoverlap; all-known mode intersects every available complete query anchor. Both are conservative and every actual literal match is asserted to survive. Hash selection is fixed and independent of corpus/query results; boundary selects grams containing ASCII punctuation/whitespace/control bytes. Each policy describes a separate hypothetical index, not simultaneous storage.",
            "residue_method": "Hybrid policies retain exact lists through cap 4, then complete position-residue masks at 64/128/256 bits. All occurrences update the mask even after the exact list overflows. Saturated masks are omitted as Unknown (all ones). Rotate each mask right by the gram's query offset and intersect; an empty intersection proves no literal match. Exact lists, when available, also constrain absolute starts. Modulo collisions only add false positives; this is an established approximate positional technique, not a novelty claim.",
            "hybrid_encoding": "Same per-segment dictionary as exact-only. Hybrid record: VByte delta doc ID; one-byte kind tag; exact payload is VByte count plus delta positions, residue payload is a raw bits/8-byte mask. Missing, unselected, and saturated records have no payload and mean Unknown. Per-policy estimates include each hybrid tag, every retained residue mask, and every retained exact list; saturated omissions do not advance that gram's doc-ID delta baseline.",
            "encoding": "Per original segment: 4-byte dictionary count plus 20 bytes per retained gram; payload per known gram/document: VByte delta doc ID, VByte occurrence count, VByte delta byte positions. Omitted document records mean Unknown, so no overflow bitmap is needed. Exact byte accounting for this hypothetical payload; excludes integrity checks, metadata/versioning, alignment, and optional skip data. No production file is written.",
            "limitations": "Offline feasibility and space estimates only. No end-to-end search latency, production build-time, serialized decoder, or memory-RSS claims. All-known mode may require many posting lookups; fixed-pair mode can retain many unknown documents. A deployment must validate sidecar integrity, handle missing legacy/delta segments conservatively, preserve tombstones, and include sidecar work in updates/compaction. Sources must be immutable and match the existing index; the lab re-creates positions from present source bytes. Indexed positions narrow an old snapshot and cannot establish that current source still matches. Any proposed byte-offset verification hint must be checked against current metadata-validated content and fall back to full verification when it fails."
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchors(literal: &str) -> Vec<Anchor> {
        query_anchors(literal.as_bytes(), |_| 1)
    }
    fn survives(content: &str, literal: &str, cap: usize) -> bool {
        filter(
            Some(&scan_positions(content.as_bytes())),
            &anchors(literal),
            cap,
            literal.len() as u32,
            content.len() as u32,
            false,
        )
        .keep
    }

    #[test]
    fn offsets_include_overlapping_occurrences_and_repeated_query_grams() {
        let positions = scan_positions(b"aaaaaa");
        let aaa = bytes_to_trigram(b'a', b'a', b'a');
        assert_eq!(positions[&aaa].complete(4), Some([0, 1, 2, 3].as_slice()));
        assert!(survives("aaaaaa", "aaaaa", 4));
        assert!(survives("aaaaaa", "aaaaa", 1)); // Overflow is not absence.
        assert!(!survives("abc---bcd", "abcd", 4));
        assert!(survives("xabc---bcd---abcd", "abcd", 4));
    }

    #[test]
    fn unknown_missing_overflow_and_invalidated_corruption_never_reject() {
        let needle = "struct file_operations";
        let evidence = scan_positions(needle.as_bytes());
        let query = anchors(needle);
        assert!(
            filter(
                None,
                &query,
                4,
                needle.len() as u32,
                needle.len() as u32,
                false
            )
            .keep
        );
        assert!(
            filter(
                Some(&AHashMap::new()),
                &query,
                4,
                needle.len() as u32,
                needle.len() as u32,
                false
            )
            .keep
        );
        // Simulate the reader rejecting a corrupt sidecar: expose no evidence.
        // The lab does not implement a checksum or claim to detect arbitrary bit flips.
        let corrupt_sidecar_is_valid = false;
        assert!(
            filter(
                corrupt_sidecar_is_valid.then_some(&evidence),
                &query,
                4,
                needle.len() as u32,
                needle.len() as u32,
                false
            )
            .keep
        );
        assert!(survives(
            "abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc",
            "abcabcabc",
            16
        ));
    }

    #[test]
    fn byte_alignment_handles_unicode_token_fragments_and_punctuation() {
        for (content, literal) in [
            ("destruct file_operations_extra", "struct file_operations"),
            ("prefixKéλsuffix", "Kéλ"),
            ("xfoo_bar::baz!", "foo_bar::baz"),
            ("needle\r\nother", "needle"),
            ("a\0bcde", "\0bcde"),
        ] {
            for cap in CAPS {
                assert!(survives(content, literal, cap), "{literal:?}");
            }
        }
        assert!(!possible_start(&[(4, &[0, 1, 2])], 8, 20));
        assert!(!possible_start(&[(0, &[u32::MAX])], 2, u32::MAX));
        assert!(possible_start(&[], u32::MAX, 0)); // Unknown is always conservative.
    }

    #[test]
    fn all_substrings_survive_all_sampling_policies_and_caps() {
        let mut content = String::from("aAa_abaaba\r\nKéλ::fooBar42 ");
        // Repetitions cross every cap, while every possible literal substring
        // exercises different gram offsets and sampling choices.
        content.push_str(&"abc".repeat(20));
        let evidence = scan_positions(content.as_bytes());
        let boundaries: Vec<_> = content
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(content.len()))
            .collect();
        for &start in &boundaries {
            for &end in &boundaries {
                if start >= end {
                    continue;
                }
                let literal = &content[start..end];
                for selection in [
                    Selection::HashEighth,
                    Selection::HashQuarter,
                    Selection::Boundary,
                    Selection::All,
                ] {
                    let query: Vec<_> = anchors(literal)
                        .into_iter()
                        .filter(|a| selection.includes(a.gram))
                        .collect();
                    let pair = choose_pair(&query);
                    for cap in CAPS {
                        for (selected, require_pair) in [(&query, false), (&pair, true)] {
                            assert!(
                                filter(
                                    Some(&evidence),
                                    selected,
                                    cap,
                                    literal.len() as u32,
                                    content.len() as u32,
                                    require_pair
                                )
                                .keep,
                                "{literal:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn encoding_estimate_matches_actual_varints_and_segment_resets() {
        let mut space = Space::default();
        space.begin_segment();
        let gram = 7;
        space.account(127, gram, &[0, 127, 128, 16384]);
        space.account(256, gram, &[10]);
        let mut encoded = Vec::new();
        for value in [127, 4, 0, 127, 1, 16256, 129, 1, 10] {
            fxi::utils::encode_varint(value, &mut encoded);
        }
        assert_eq!(space.payload_bytes, encoded.len() as u64);
        assert_eq!(space.total_bytes(), 4 + 20 + encoded.len() as u64);
        space.begin_segment();
        space.account(0, gram, &[0]);
        assert_eq!(space.dictionary_entries, 2);
        assert_eq!(space.retained_doc_gram_pairs, 3);
        assert_eq!(space.retained_positions, 6);
        assert_eq!(space.total_bytes(), 8 + 40 + encoded.len() as u64 + 3);
    }

    #[test]
    fn residue_rotations_match_bit_oracle_across_words_and_wrap() {
        let mut occurrences = Occurrences::default();
        for offset in [0, 1, 63, 64, 127, 128, 191, 192, 255, 256, 511] {
            occurrences.push(offset);
        }
        for bits in [64, 128, 256] {
            let mask = residue_mask(&occurrences, bits);
            for shift in 0..=bits * 2 {
                let actual = align_residues(mask, bits, shift);
                let mut expected = [0u64; 4];
                for bit in 0..bits {
                    if mask[bit as usize / 64] & (1 << (bit % 64)) != 0 {
                        let target = (bit + bits - shift % bits) % bits;
                        expected[target as usize / 64] |= 1 << (target % 64);
                    }
                }
                assert_eq!(actual, expected, "bits={bits} shift={shift}");
            }
        }
    }

    #[test]
    fn residue_aliases_are_false_positives_and_saturation_is_unknown() {
        let mut first = Occurrences::default();
        let mut second = Occurrences::default();
        for offset in [0, 128] {
            first.push(offset);
        }
        for offset in [68, 196] {
            second.push(offset);
        }
        let positions = AHashMap::from_iter([(1, first), (2, second)]);
        let query = [
            Anchor {
                gram: 1,
                offset: 0,
                frequency: 1,
            },
            Anchor {
                gram: 2,
                offset: 4,
                frequency: 1,
            },
        ];
        // No offsets are actually four bytes apart, but 64-bit masks alias them.
        assert!(filter_hybrid(Some(&positions), &query, 1, 64, 7, 256, false).keep);
        assert!(!filter_hybrid(Some(&positions), &query, 1, 128, 7, 256, false).keep);
        for bits in [64, 128, 256] {
            let mut saturated = Occurrences::default();
            for offset in 0..bits {
                saturated.push(offset);
            }
            assert_eq!(residue_mask(&saturated, bits), all_residues(bits));
            let saturated_positions = AHashMap::from_iter([(1, saturated), (2, saturated)]);
            let verdict = filter_hybrid(Some(&saturated_positions), &query, 1, bits, 7, 256, false);
            assert!(verdict.keep);
            assert_eq!(verdict.known, 0);
            assert!(filter_hybrid(None, &query, 1, bits, 7, 256, false).keep);
        }
    }

    #[test]
    fn hybrid_summaries_preserve_substrings_beyond_exact_caps() {
        let content = format!(
            "{}Kéλ::fooBar42{}",
            "aAa_abaaba ".repeat(3),
            "abc".repeat(40)
        );
        let evidence = scan_positions(content.as_bytes());
        let boundaries: Vec<_> = content
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(content.len()))
            .collect();
        for (start_index, &start) in boundaries.iter().enumerate() {
            for length in [1, 2, 3, 4, 8, 16, 40] {
                let Some(&end) = boundaries.get(start_index + length) else {
                    continue;
                };
                let literal = &content[start..end];
                for selection in [
                    Selection::HashEighth,
                    Selection::HashQuarter,
                    Selection::Boundary,
                    Selection::All,
                ] {
                    let query: Vec<_> = anchors(literal)
                        .into_iter()
                        .filter(|a| selection.includes(a.gram))
                        .collect();
                    let pair = choose_pair(&query);
                    for bits in [64, 128, 256] {
                        for cap in [1, 4] {
                            for (selected, require_pair) in [(&query, false), (&pair, true)] {
                                assert!(
                                    filter_hybrid(
                                        Some(&evidence),
                                        selected,
                                        cap,
                                        bits,
                                        literal.len() as u32,
                                        content.len() as u32,
                                        require_pair
                                    )
                                    .keep,
                                    "{literal:?} bits={bits} cap={cap}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn hybrid_encoding_counts_tags_masks_and_omitted_delta_gaps() {
        let mut space = Space::default();
        space.begin_segment();
        space.account(127, 7, &[0, 10]);
        space.payload_bytes += 1; // Exact-list kind tag.
        // A saturated record at doc128 would be omitted; its ID must not update
        // previous_docs, so the next retained delta is 256-127, not 256-128.
        space.account_residue(256, 7, 128);
        let mut encoded = Vec::new();
        fxi::utils::encode_varint(127, &mut encoded);
        encoded.push(0);
        for value in [2, 0, 10] {
            fxi::utils::encode_varint(value, &mut encoded);
        }
        fxi::utils::encode_varint(129, &mut encoded);
        encoded.push(1);
        encoded.extend([0x55; 16]);
        assert_eq!(space.payload_bytes, encoded.len() as u64);
        assert_eq!(space.total_bytes(), 4 + 20 + encoded.len() as u64);
        assert_eq!(space.residue_doc_gram_pairs, 1);
    }
}
