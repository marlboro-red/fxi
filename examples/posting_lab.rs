//! Offline codec space experiment over a pinned, immutable index generation.
//! Usage: cargo run --release --example posting_lab -- GENERATION_DIRECTORY
//! Never changes an index. Estimates include one codec byte per posting list.
use fxi::utils::delta_decode;
use std::path::Path;

fn bitmap_bytes(values: &[u32]) -> u64 {
    match (values.first(), values.last()) {
        (Some(&first), Some(&last)) => 4 + (u64::from(last) - u64::from(first) + 1).div_ceil(8),
        _ => 0,
    }
}

fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .expect("immutable generation directory");
    let mut lists = 0u64;
    let mut postings = 0u64;
    let mut current = 0u64;
    let mut hybrid = 0u64;
    let mut bitmap_lists = 0u64;
    let mut bitmap_postings = 0u64;
    let mut roaring_hybrid = 0u64;
    let mut segments: Vec<_> =
        std::fs::read_dir(Path::new(&root).join("segments"))?.collect::<Result<_, _>>()?;
    segments.sort_by_key(|entry| entry.path());
    for segment in segments {
        let dict = std::fs::read(segment.path().join("grams.dict"))?;
        let data = std::fs::read(segment.path().join("grams.postings"))?;
        anyhow::ensure!(
            dict.len() >= 4 && (dict.len() - 4) % 20 == 0,
            "invalid dictionary"
        );
        let count = u32::from_le_bytes(dict[..4].try_into()?);
        anyhow::ensure!(count as usize == (dict.len() - 4) / 20, "invalid count");
        for entry in dict[4..].as_chunks::<20>().0 {
            let offset = usize::try_from(u64::from_le_bytes(entry[4..12].try_into()?))?;
            let length = u32::from_le_bytes(entry[12..16].try_into()?) as usize;
            let frequency = u32::from_le_bytes(entry[16..20].try_into()?);
            let end = offset
                .checked_add(length)
                .ok_or_else(|| anyhow::anyhow!("overflow"))?;
            let bytes = data
                .get(offset..end)
                .ok_or_else(|| anyhow::anyhow!("truncated postings"))?;
            let values = delta_decode(bytes);
            anyhow::ensure!(
                values.len() == frequency as usize && values.windows(2).all(|w| w[0] < w[1]),
                "invalid posting sequence"
            );
            let bitmap = bitmap_bytes(&values);
            let roaring: roaring::RoaringBitmap = values.iter().copied().collect();
            current += length as u64;
            hybrid += 1 + bitmap.min(length as u64);
            roaring_hybrid += 1 + (roaring.serialized_size() as u64).min(length as u64);
            lists += 1;
            postings += values.len() as u64;
            if bitmap < length as u64 {
                bitmap_lists += 1;
                bitmap_postings += values.len() as u64;
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "generation": root, "lists": lists, "postings": postings,
            "current_vbyte_bytes": current, "hybrid_bitmap_bytes": hybrid,
            "hybrid_roaring_bytes": roaring_hybrid, "bitmap_lists": bitmap_lists,
            "bitmap_postings": bitmap_postings,
            "note": "Offline space estimate only. Bitmap stores minimum doc ID (4 bytes) plus span bits; list length already resides in dictionary. Both hybrids add a one-byte codec tag to every list. No production format changes, construction cost, or end-to-end latency claims."
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bitmap_span_handles_boundaries_and_sparse_ids() {
        assert_eq!(bitmap_bytes(&[]), 0);
        assert_eq!(bitmap_bytes(&[u32::MAX]), 5);
        assert_eq!(bitmap_bytes(&[0, 7]), 5);
        assert_eq!(bitmap_bytes(&[0, 8]), 6);
        assert_eq!(bitmap_bytes(&[100, 107]), 5);
        assert_eq!(bitmap_bytes(&[0, u32::MAX]), 4 + (1u64 << 29));
    }
}
