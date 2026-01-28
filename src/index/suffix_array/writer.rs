//! Suffix array writer
//!
//! Writes suffix array data structures to disk in a format optimized for
//! memory-mapped reading.

use super::builder::BuiltSuffixArray;
use super::types::*;
use anyhow::Result;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// Writes suffix array files to a segment directory
pub struct SuffixArrayWriter;

impl SuffixArrayWriter {
    /// Write all suffix array files to a segment directory
    ///
    /// Creates:
    /// - concat.bin: Concatenated document text
    /// - concat.idx: Document boundary index
    /// - sa.bin: The suffix array itself
    pub fn write(segment_path: &Path, built_sa: &BuiltSuffixArray) -> Result<()> {
        // Skip if empty
        if built_sa.text.is_empty() {
            return Ok(());
        }

        // Write concatenated text
        Self::write_concat(segment_path, &built_sa.text)?;

        // Write document index
        Self::write_index(segment_path, &built_sa.boundaries, built_sa.text.len() as u64)?;

        // Write suffix array
        Self::write_suffix_array(segment_path, &built_sa.suffix_array)?;

        Ok(())
    }

    /// Write concatenated text to concat.bin
    fn write_concat(segment_path: &Path, text: &[u8]) -> Result<()> {
        let path = segment_path.join("concat.bin");
        let mut file = BufWriter::with_capacity(65536, File::create(&path)?);
        file.write_all(text)?;
        file.flush()?;
        Ok(())
    }

    /// Write document index to concat.idx
    fn write_index(
        segment_path: &Path,
        boundaries: &[DocBoundary],
        total_size: u64,
    ) -> Result<()> {
        let path = segment_path.join("concat.idx");
        let mut file = BufWriter::with_capacity(65536, File::create(&path)?);

        // Write header
        let header = ConcatIndexHeader::new(boundaries.len() as u32, total_size);
        file.write_all(&header.magic.to_le_bytes())?;
        file.write_all(&header.version.to_le_bytes())?;
        file.write_all(&header.doc_count.to_le_bytes())?;
        file.write_all(&header.total_size.to_le_bytes())?;
        file.write_all(&header.flags.to_le_bytes())?;

        // Write entries
        for boundary in boundaries {
            file.write_all(&boundary.doc_id.to_le_bytes())?;
            file.write_all(&boundary.start.to_le_bytes())?;
            file.write_all(&boundary.end.to_le_bytes())?;
        }

        file.flush()?;
        Ok(())
    }

    /// Write suffix array to sa.bin
    fn write_suffix_array(segment_path: &Path, sa: &[SuffixEntry]) -> Result<()> {
        let path = segment_path.join("sa.bin");
        let mut file = BufWriter::with_capacity(65536, File::create(&path)?);

        // Write header
        let header = SuffixArrayHeader::new(sa.len() as u64);
        file.write_all(&header.magic.to_le_bytes())?;
        file.write_all(&header.version.to_le_bytes())?;
        file.write_all(&header.suffix_count.to_le_bytes())?;
        file.write_all(&header.flags.to_le_bytes())?;

        // Write suffix array entries
        // Using a buffer to reduce system call overhead
        let mut buffer = Vec::with_capacity(8 * 1024); // 1024 entries at a time
        for &entry in sa {
            buffer.extend_from_slice(&entry.to_le_bytes());
            if buffer.len() >= 8 * 1024 {
                file.write_all(&buffer)?;
                buffer.clear();
            }
        }
        if !buffer.is_empty() {
            file.write_all(&buffer)?;
        }

        file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::suffix_array::builder::SuffixArrayBuilder;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_write_suffix_array() {
        let temp_dir = tempdir().unwrap();
        let segment_path = temp_dir.path().join("seg_0001");
        fs::create_dir_all(&segment_path).unwrap();

        // Build a simple suffix array
        let mut builder = SuffixArrayBuilder::with_defaults();
        builder.add_document(1, b"hello world");
        builder.add_document(2, b"foo bar");
        let built = builder.build();

        // Write it
        SuffixArrayWriter::write(&segment_path, &built).unwrap();

        // Verify files exist
        assert!(segment_path.join("concat.bin").exists());
        assert!(segment_path.join("concat.idx").exists());
        assert!(segment_path.join("sa.bin").exists());

        // Verify concat.bin content
        let concat = fs::read(segment_path.join("concat.bin")).unwrap();
        assert_eq!(&concat[..5], b"hello"); // Case-insensitive lowercase

        // Verify sa.bin header
        let sa_data = fs::read(segment_path.join("sa.bin")).unwrap();
        let magic = u32::from_le_bytes(sa_data[0..4].try_into().unwrap());
        assert_eq!(magic, SA_MAGIC);
    }
}
