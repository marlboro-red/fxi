//! Compact ownership of a file's distinct tokens while it waits for inversion.
//! Offsets refer to UTF-8 boundaries established from the input strings.
use std::ops::Index;

#[derive(Default)]
pub struct PackedTokens {
    data: String,
    offsets: Vec<usize>,
}

impl PackedTokens {
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }
    #[allow(dead_code)] // Public collection API, paired with len.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.offsets.windows(2).map(|p| &self.data[p[0]..p[1]])
    }
}

impl From<Vec<String>> for PackedTokens {
    fn from(tokens: Vec<String>) -> Self {
        let mut data = String::with_capacity(tokens.iter().map(String::len).sum());
        let mut offsets = Vec::with_capacity(tokens.len() + 1);
        offsets.push(0);
        for token in tokens {
            data.push_str(&token);
            offsets.push(data.len());
        }
        Self { data, offsets }
    }
}

impl Index<usize> for PackedTokens {
    type Output = str;
    fn index(&self, id: usize) -> &str {
        &self.data[self.offsets[id]..self.offsets[id + 1]]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packing_preserves_order_duplicates_and_utf8_boundaries() {
        for words in [
            vec![],
            vec![""],
            vec!["zeta", "alpha", "alpha", "", "KΣ", "a\0b"],
        ] {
            let packed =
                PackedTokens::from(words.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
            assert_eq!(packed.len(), words.len());
            assert_eq!(packed.iter().collect::<Vec<_>>(), words);
            for (id, word) in words.iter().enumerate() {
                assert_eq!(&packed[id], *word);
            }
        }
        assert!(PackedTokens::default().is_empty());
    }
}
