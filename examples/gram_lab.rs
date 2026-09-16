//! Offline representation experiment. Never used by production search.
//! Compares trigram sets, independent phase/follow masks, and their joint
//! signature. All candidates must contain the brute-force matching file set.
//! Usage: cargo run --release --example gram_lab -- CONTROLLED_CORPUS
use ahash::AHashMap;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Copy, Default)]
struct Evidence {
    phase: u8,
    next: u8,
    joint: u64,
}

fn gram(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], 0])
}
fn bucket(b: u8) -> u8 {
    b.wrapping_mul(37).rotate_left(3) & 7
}
fn evidence(data: &[u8]) -> AHashMap<u32, Evidence> {
    let mut map: AHashMap<u32, Evidence> = AHashMap::new();
    for (offset, bytes) in data.windows(3).enumerate() {
        let e = map.entry(gram(bytes)).or_default();
        let phase = offset % 8;
        e.phase |= 1 << phase;
        if let Some(&next) = data.get(offset + 3) {
            let next = bucket(next);
            e.next |= 1 << next;
            e.joint |= 1 << (phase * 8 + usize::from(next));
        }
    }
    map
}

// Spend extra bits only when at least half the marginal cross-product is
// absent. This is an experimental space allocation rule, fixed before the
// second-corpus measurement, not a claim of optimality.
fn use_joint(e: Evidence) -> bool {
    let combinations = e.phase.count_ones() * e.next.count_ones();
    combinations >= 4 && 2 * e.joint.count_ones() <= combinations
}

fn allowed_phase(e: Evidence, next: Option<u8>, mode: usize) -> u8 {
    let mode = if mode == 3 {
        if use_joint(e) { 2 } else { 1 }
    } else {
        mode
    };
    match (mode, next) {
        (0, _) => 255,
        (_, None) => e.phase,
        (1, Some(b)) => {
            if e.next & (1 << bucket(b)) != 0 {
                e.phase
            } else {
                0
            }
        }
        (_, Some(b)) => (0..8).fold(0, |mask, phase| {
            mask | if e.joint & (1 << (phase * 8 + usize::from(bucket(b)))) != 0 {
                1 << phase
            } else {
                0
            }
        }),
    }
}

fn query(
    index: &AHashMap<u32, Vec<(usize, Evidence)>>,
    needle: &[u8],
    mode: usize,
) -> BTreeSet<usize> {
    let mut constraints: Vec<_> = needle
        .windows(3)
        .enumerate()
        .map(|(i, b)| (i, index.get(&gram(b))))
        .collect();
    if constraints.iter().any(|(_, p)| p.is_none()) {
        return BTreeSet::new();
    }
    constraints.sort_by_key(|(_, p)| p.unwrap().len());
    let Some((offset, postings)) = constraints.first() else {
        panic!("query must have at least 3 bytes")
    };
    let mut candidates: AHashMap<usize, u8> = postings
        .unwrap()
        .iter()
        .filter_map(|&(id, e)| {
            let phases = allowed_phase(e, needle.get(offset + 3).copied(), mode)
                .rotate_right((*offset % 8) as u32);
            (phases != 0).then_some((id, phases))
        })
        .collect();
    for (offset, postings) in constraints.iter().skip(1) {
        candidates.retain(|id, phases| {
            let entries = postings.unwrap();
            let Ok(i) = entries.binary_search_by_key(id, |&(id, _)| id) else {
                return false;
            };
            *phases &= allowed_phase(entries[i].1, needle.get(offset + 3).copied(), mode)
                .rotate_right((*offset % 8) as u32);
            *phases != 0
        });
        if candidates.is_empty() {
            break;
        }
    }
    candidates.into_keys().collect()
}

fn main() -> anyhow::Result<()> {
    let root = std::env::args().nth(1).expect("controlled corpus root");
    let mut paths: Vec<_> = ignore::WalkBuilder::new(Path::new(&root))
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.path().to_owned())
        .collect();
    paths.sort();
    let mut files = Vec::new();
    for path in paths {
        let data = std::fs::read(path)?;
        if !data.is_empty()
            && data.len() <= 10_000_000
            && !data.contains(&0)
            && std::str::from_utf8(&data).is_ok()
        {
            files.push(data);
        }
    }
    let start = Instant::now();
    let mut index: AHashMap<u32, Vec<(usize, Evidence)>> = AHashMap::new();
    for (id, data) in files.iter().enumerate() {
        for (gram, e) in evidence(data) {
            index.entry(gram).or_default().push((id, e));
        }
    }
    let build_ms = start.elapsed().as_secs_f64() * 1000.;
    let postings: usize = index.values().map(Vec::len).sum();
    let adaptive = index
        .values()
        .flatten()
        .filter(|(_, e)| use_joint(*e))
        .count();
    let saturated = index
        .values()
        .flatten()
        .filter(|(_, e)| e.joint == u64::MAX)
        .count();
    // Deterministic substrings from evenly spaced files, plus one-byte mutations.
    // Selection is independent of candidate counts and signature parameters.
    let mut queries = BTreeSet::new();
    for data in files.iter().step_by((files.len() / 100).max(1)) {
        for length in [4, 8, 16, 32] {
            if data.len() < length {
                continue;
            }
            let start = data.len() / 3;
            let end = (start + length).min(data.len());
            let needle = &data[start..end];
            if needle.len() < 3 || needle.contains(&b'\n') || needle.contains(&b'\r') {
                continue;
            }
            queries.insert(needle.to_vec());
            let mut mutated = needle.to_vec();
            let last = mutated.len() - 1;
            mutated[last] = if mutated[last] == b'X' { b'Y' } else { b'X' };
            queries.insert(mutated);
        }
    }
    for text in [
        "return",
        "static void",
        "raxFind",
        "serverAssert",
        "dictRehash",
        "unlikely_absent_symbol",
    ] {
        queries.insert(text.as_bytes().to_vec());
    }
    let mut rows = Vec::new();
    for needle in queries {
        let expected: BTreeSet<_> = files
            .iter()
            .enumerate()
            .filter(|(_, b)| memchr::memmem::find(b, &needle).is_some())
            .map(|(i, _)| i)
            .collect();
        let mut modes = Vec::new();
        for mode in 0..4 {
            let mut samples = Vec::new();
            let mut found = BTreeSet::new();
            for _ in 0..7 {
                let start = Instant::now();
                found = query(&index, &needle, mode);
                samples.push(start.elapsed().as_micros());
            }
            assert!(
                expected.is_subset(&found),
                "false negative in mode {mode}: {needle:?}"
            );
            samples.sort();
            modes.push(json!({"candidates":found.len(),"candidate_bytes":found.iter().map(|&i|files[i].len()).sum::<usize>(),
                "lookup_median_us":samples[3],"false_positives":found.len()-expected.len()}));
        }
        rows.push(json!({"query_hex":needle.iter().map(|b|format!("{b:02x}")).collect::<String>(),"matches":expected.len(),"modes":modes}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"files":files.len(),"source_bytes":files.iter().map(Vec::len).sum::<usize>(),
        "build_joint_ms":build_ms,"postings":postings,"joint_saturated_postings":saturated,
        "estimated_uncompressed_posting_bytes":{"trigram":postings*4,"independent":postings*6,"joint":postings*12,"adaptive_joint":postings*6 + adaptive*8 + postings.div_ceil(8)},
        "note":"In-memory prototype. Payload estimates exclude dictionaries, allocation and compression. Build includes all signatures. No end-to-end speed claim.",
        "modes":["trigram","independent_8_bit_phase_and_follow","joint_8_by_8_phase_follow","adaptive_joint"],"rows":rows})
        )?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signatures_never_reject_a_real_substring_including_file_end() {
        let mut files = Vec::new();
        for bits in 0u32..1024 {
            files.push(
                (0..10)
                    .map(|i| if bits & (1 << i) != 0 { b'a' } else { b'b' })
                    .collect::<Vec<_>>(),
            );
        }
        let mut index: AHashMap<u32, Vec<(usize, Evidence)>> = AHashMap::new();
        for (id, data) in files.iter().enumerate() {
            for (gram, e) in evidence(data) {
                index.entry(gram).or_default().push((id, e));
            }
        }
        for length in 3..=6 {
            for bits in 0u32..(1 << length) {
                let needle: Vec<_> = (0..length)
                    .map(|i| if bits & (1 << i) != 0 { b'a' } else { b'b' })
                    .collect();
                let exact: BTreeSet<_> = files
                    .iter()
                    .enumerate()
                    .filter(|(_, data)| memchr::memmem::find(data, &needle).is_some())
                    .map(|(id, _)| id)
                    .collect();
                let simple = query(&index, &needle, 0);
                let independent = query(&index, &needle, 1);
                let joint = query(&index, &needle, 2);
                let adaptive = query(&index, &needle, 3);
                assert!(joint.is_subset(&adaptive));
                assert!(adaptive.is_subset(&independent));
                assert!(exact.is_subset(&joint));
                assert!(joint.is_subset(&independent));
                assert!(independent.is_subset(&simple));
            }
        }
    }
}
