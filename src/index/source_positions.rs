//! Bounded, lazy byte-position evidence owned by one immutable source snapshot.
use memchr::memmem::Finder;
use std::sync::{Arc, Mutex};

const MAX_GRAMS: usize = 2;
const MAX_POSITIONS: usize = 32;
const MIN_SOURCE_BYTES: usize = 4096;

#[derive(Clone)]
enum Evidence {
    Complete(Arc<[u32]>),
    Overflow,
}

/// Source bytes and their derived evidence always have identical lifetimes.
/// No weak reference can pin an evicted inline source allocation.
pub struct SourceSnapshot {
    text: Box<str>,
    positions: Mutex<Vec<(u32, Evidence)>>,
}

impl From<String> for SourceSnapshot {
    fn from(text: String) -> Self {
        Self {
            text: text.into_boxed_str(),
            positions: Mutex::new(Vec::new()),
        }
    }
}

impl std::ops::Deref for SourceSnapshot {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

/// Only use for a proven nonempty, exact, line-local byte literal.
pub(crate) struct LiteralProbe<'a> {
    literal: &'a [u8],
    finder: Finder<'a>,
    gram: u32,
    offset: usize,
}

impl<'a> LiteralProbe<'a> {
    pub(crate) fn new(literal: &'a [u8], offset: usize) -> Option<Self> {
        if literal.len() < 8 {
            return None;
        }
        let bytes = literal.get(offset..offset.checked_add(3)?)?;
        let gram = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
        Some(Self {
            literal,
            finder: Finder::new(literal),
            gram,
            offset,
        })
    }

    pub(crate) fn contains(&self, source: &SourceSnapshot) -> bool {
        let bytes = source.as_bytes();
        if bytes.len() < MIN_SOURCE_BYTES || bytes.len() > u32::MAX as usize {
            return self.finder.find(bytes).is_some();
        }
        let evidence = source.positions.lock().ok().and_then(|mut entries| {
            let index = entries.iter().position(|(gram, _)| *gram == self.gram)?;
            entries.swap(0, index);
            Some(entries[0].1.clone())
        });
        let evidence = evidence.unwrap_or_else(|| {
            let evidence = collect(bytes, &self.literal[self.offset..self.offset + 3]);
            if let Ok(mut entries) = source.positions.lock() {
                // A concurrent query may have populated the same immutable evidence.
                if !entries.iter().any(|(gram, _)| *gram == self.gram) {
                    if entries.capacity() == 0 {
                        entries.reserve_exact(MAX_GRAMS);
                    }
                    if entries.len() == MAX_GRAMS {
                        entries.pop();
                    }
                    entries.insert(0, (self.gram, evidence.clone()));
                }
            }
            evidence
        });
        match evidence {
            Evidence::Overflow => self.finder.find(bytes).is_some(),
            Evidence::Complete(positions) => positions.iter().any(|&position| {
                let Some(start) = (position as usize).checked_sub(self.offset) else {
                    return false;
                };
                let Some(end) = start.checked_add(self.literal.len()) else {
                    return false;
                };
                bytes.get(start..end) == Some(self.literal)
            }),
        }
    }
}

fn collect(bytes: &[u8], gram: &[u8]) -> Evidence {
    let finder = Finder::new(gram);
    let mut positions = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = finder.find(&bytes[cursor..]) {
        if positions.len() == MAX_POSITIONS {
            return Evidence::Overflow;
        }
        let position = cursor + relative;
        positions.push(position as u32);
        // Overlapping grams are necessary, e.g. every offset of "aaaa".
        cursor = position + 1;
    }
    Evidence::Complete(positions.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_positions_and_overflow_are_distinct() {
        let Evidence::Complete(positions) = collect(b"aaaaaa", b"aaa") else {
            panic!()
        };
        assert_eq!(&*positions, &[0, 1, 2, 3]);
        assert!(matches!(collect(&[b'a'; 40], b"aaa"), Evidence::Overflow));
        let Evidence::Complete(positions) = collect(b"abc", b"xyz") else {
            panic!()
        };
        assert!(positions.is_empty());
    }

    #[test]
    fn evidence_is_bounded_and_snapshot_specific() {
        let text = format!("{}struct file_operations caféabcdefgh\n", "x".repeat(5000));
        let old = SourceSnapshot::from(text);
        let changed = SourceSnapshot::from("x".repeat(6000));
        for _ in 0..3 {
            for literal in [
                "struct file_operations",
                "file_operations",
                "caféabcdefgh",
                "abcdefghZ",
                "xxxxxxxx",
            ] {
                let probe = LiteralProbe::new(literal.as_bytes(), 2).unwrap();
                assert_eq!(probe.contains(&old), old.contains(literal));
                assert_eq!(probe.contains(&changed), changed.contains(literal));
                assert!(old.positions.lock().unwrap().len() <= MAX_GRAMS);
            }
        }
    }

    #[test]
    fn complete_positions_match_literal_oracle_at_boundaries_and_under_contention() {
        let source = SourceSnapshot::from(format!("abcdefgh{}abcdefghX", "z".repeat(5000)));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for literal in [
                        "abcdefgh",
                        "abcdefghX",
                        "Xabcdefgh",
                        "zabcdefgh",
                        "abcdefghXX",
                    ] {
                        let probe = LiteralProbe::new(literal.as_bytes(), 2).unwrap();
                        assert_eq!(probe.contains(&source), source.contains(literal));
                    }
                });
            }
        });
        assert!(source.positions.lock().unwrap().len() <= MAX_GRAMS);
    }
}
