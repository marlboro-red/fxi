//! Shared bounded on-disk fixture for deterministic tests and libFuzzer.
use fxi::index::{build::build_index_with_options, reader::IndexReader};
use fxi::query::{QueryExecutor, parse_query};
use fxi::utils::app_data::{get_index_dir, remove_index};
use std::{fs, path::PathBuf};

type Error = Box<dyn std::error::Error>;
pub struct TinyIndex {
    root: tempfile::TempDir,
    components: Vec<(PathBuf, Vec<u8>)>,
}
impl Drop for TinyIndex {
    fn drop(&mut self) {
        let _ = remove_index(self.root.path());
    }
}
impl TinyIndex {
    pub fn new() -> Result<Self, Error> {
        fxi::utils::app_data::isolate_test_storage().unwrap();
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join(".git"))?;
        fs::write(root.path().join("a.txt"), "alpha needle\nneedle twice\n")?;
        fs::write(root.path().join("b.rs"), "beta alpha\n")?;
        build_index_with_options(root.path(), true, true, Some(1))?;
        let index = get_index_dir(root.path())?;
        let mut components = Vec::new();
        for relative in [
            "meta.json",
            "docs.bin",
            "paths.bin",
            "segments/seg_0001/grams.dict",
            "segments/seg_0001/grams.postings",
            "segments/seg_0001/tokens.dict",
            "segments/seg_0001/tokens.postings",
            "segments/seg_0001/tokens.positions",
        ] {
            let path = index.join(relative);
            let bytes = fs::read(&path)?;
            assert!(bytes.len() <= 65536, "fixture must remain small");
            components.push((path, bytes));
        }
        let fixture = Self { root, components };
        let reader = IndexReader::open(fixture.root.path())?;
        assert_eq!(
            QueryExecutor::new(&reader).execute_match_counts(&parse_query("needle"), 0)?,
            vec![(PathBuf::from("a.txt"), 2)]
        );
        Ok(fixture)
    }
    /// Input limits are enforced here, including when called outside libFuzzer.
    /// Every iteration restores baseline bytes before mutating one component.
    pub fn exercise(&self, data: &[u8]) -> Result<bool, Error> {
        if data.len() < 3 || data.len() > 256 {
            return Ok(false);
        }
        for (path, bytes) in &self.components {
            fs::write(path, bytes)?;
        }
        let (path, original) = &self.components[data[0] as usize % self.components.len()];
        let mut bytes = original.clone();
        let at = u16::from_le_bytes([data[1], data[2]]) as usize % (bytes.len() + 1);
        match data.get(3).copied().unwrap_or(0) % 4 {
            0 => bytes.truncate(at),
            1 => {
                for (index, byte) in data[4.min(data.len())..].iter().enumerate() {
                    if let Some(target) = bytes.get_mut(at + index) {
                        *target ^= byte;
                    }
                }
            }
            2 => {
                for (index, byte) in data[4.min(data.len())..].iter().enumerate() {
                    if let Some(target) = bytes.get_mut(at + index) {
                        *target = *byte;
                    }
                }
            }
            _ => {
                if at < bytes.len() {
                    bytes.remove(at);
                }
            }
        }
        fs::write(path, bytes)?;
        // Invalid bytes may be rejected at open or during deferred query
        // validation. Mutations can also encode a different valid index.
        let Ok(reader) = IndexReader::open(self.root.path()) else {
            return Ok(false);
        };
        for pattern in [
            "needle",
            "alpha",
            "\"alpha needle\"",
            "re:/needle|beta/",
            "re:/absentmarker/",
        ] {
            let executor = QueryExecutor::new(&reader);
            let query = parse_query(pattern);
            if let Ok(files) = executor.execute_files_only(&query, 8) {
                assert!(files.len() <= 8);
            }
            if let Ok(counts) = executor.execute_match_counts(&query, 8) {
                assert!(counts.len() <= 8);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxi::utils::{decode_varint, encode_varint};

    #[test]
    fn bounded_component_mutations_never_panic() {
        let fixture = TinyIndex::new().unwrap();
        let mut accepted = 0;
        let mut rejected = 0;
        for component in 0..fixture.components.len() {
            let len = fixture.components[component].1.len();
            for offset in [0, 1, 2, 3, 4, 7, 8, 15, len / 2, len.saturating_sub(1), len] {
                for mode in 0..4 {
                    for byte in [0, 1, 0x7f, 0x80, 0xff] {
                        let input = [
                            component as u8,
                            offset as u8,
                            (offset >> 8) as u8,
                            mode,
                            byte,
                            byte,
                            byte,
                            byte,
                        ];
                        let result = std::panic::catch_unwind(|| fixture.exercise(&input))
                            .unwrap_or_else(|_| panic!("index mutation input={input:?}"));
                        if result.unwrap_or_else(|error| panic!("input={input:?}: {error}")) {
                            accepted += 1;
                        } else {
                            rejected += 1;
                        }
                    }
                }
            }
        }
        assert!(accepted > 0 && rejected > 0, "must exercise both outcomes");
    }

    // Independent arithmetic decoder: no production shifts/masks/control flow
    // reused. Noncanonical but numerically valid encodings remain legal.
    fn reference(bytes: &[u8]) -> Option<(u32, usize)> {
        let mut value = 0u64;
        let mut weight = 1u64;
        for (index, byte) in bytes.iter().take(5).enumerate() {
            value += u64::from(byte % 128) * weight;
            if value > u64::from(u32::MAX) {
                return None;
            }
            if *byte < 128 {
                return Some((value as u32, index + 1));
            }
            weight *= 128;
        }
        None
    }
    #[test]
    fn generated_varints_match_independent_arithmetic_oracle() {
        let mut random = 0x194920260918u64;
        for iteration in 0..20000 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let value = if iteration < 256 {
                iteration
            } else {
                random as u32
            };
            let mut encoded = Vec::new();
            encode_varint(value, &mut encoded);
            assert_eq!(decode_varint(&encoded), Some((value, encoded.len())));
            assert_eq!(reference(&encoded), Some((value, encoded.len())));
            for length in 0..=8 {
                let bytes = random.to_le_bytes();
                assert_eq!(
                    decode_varint(&bytes[..length]),
                    reference(&bytes[..length]),
                    "bytes={:?}",
                    &bytes[..length]
                );
            }
        }
        for last in 0..=255 {
            let bytes = [255, 255, 255, 255, last];
            assert_eq!(decode_varint(&bytes), reference(&bytes));
        }
    }
}
