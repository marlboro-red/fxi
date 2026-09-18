//! Offline source verification experiment, not a production pack format.
//! Run with FXI_INDEXES pointing at a matching index. JSON goes to stdout.
use anyhow::{Context, Result, ensure};
use fxi::index::reader::IndexReader;
use memchr::memmem::Finder;
use rayon::prelude::*;
use std::{fs, io::Write, path::PathBuf, time::Instant};
use xxhash_rust::xxh3::xxh3_64;

#[derive(Clone, Copy, Debug)]
enum Codec {
    Raw,
    Lz4,
    Zstd(i32),
}
#[derive(Clone, Copy)]
struct Mode {
    name: &'static str,
    codec: Codec,
    block: usize,
    raw_prefix: bool,
    max_file: usize,
}
struct Block {
    start: usize,
    stored: usize,
    len: usize,
    hash: u64,
    raw: bool,
}
struct Entry {
    path: PathBuf,
    text: Vec<u8>,
    stamp: Stamp,
    id: u32,
}
#[derive(PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    identity: (u64, u64, i64, i64),
}
fn stamp(path: &std::path::Path) -> Result<Stamp> {
    let m = fs::metadata(path)?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino(), m.ctime(), m.ctime_nsec())
    };
    #[cfg(not(unix))]
    let identity = (0, 0, 0, 0);
    Ok(Stamp {
        len: m.len(),
        modified: m.modified().ok(),
        identity,
    })
}
struct Pack {
    mode: Mode,
    blocks: Vec<Option<Vec<Block>>>,
    map: Option<memmap2::Mmap>,
    _file: fs::File,
    size: u64,
    build_seconds: f64,
}
fn encode(bytes: &[u8], codec: Codec) -> Result<Vec<u8>> {
    Ok(match codec {
        Codec::Raw => bytes.to_vec(),
        Codec::Lz4 => lz4_flex::block::compress(bytes),
        Codec::Zstd(level) => zstd::bulk::compress(bytes, level)?,
    })
}
fn decode(block: &Block, data: &[u8], codec: Codec, out: &mut Vec<u8>) -> Result<()> {
    let bytes = data
        .get(block.start..block.start.checked_add(block.stored).context("overflow")?)
        .context("truncated block")?;
    out.resize(block.len, 0);
    if block.raw {
        ensure!(bytes.len() == block.len, "raw length");
        out.copy_from_slice(bytes);
    } else {
        let n = match codec {
            Codec::Lz4 => lz4_flex::block::decompress_into(bytes, out)?,
            Codec::Zstd(_) => zstd::bulk::decompress_to_buffer(bytes, out.as_mut_slice())?,
            Codec::Raw => anyhow::bail!("invalid raw block"),
        };
        ensure!(n == block.len, "decoded length");
    }
    ensure!(xxh3_64(out) == block.hash, "checksum");
    Ok(())
}
fn build(entries: &[Entry], mode: Mode) -> Result<Pack> {
    let now = Instant::now();
    let mut file = tempfile::tempfile()?;
    let mut size = 0usize;
    let mut blocks = Vec::new();
    for entry in entries {
        if entry.text.len() > mode.max_file {
            blocks.push(None);
            continue;
        }
        let mut list = Vec::new();
        // Prefix remains exactly 4 KiB regardless of the compressed tail block size.
        let prefix = if mode.raw_prefix {
            entry.text.len().min(4096)
        } else {
            0
        };
        let chunks = entry.text[..prefix]
            .chunks(4096)
            .chain(entry.text[prefix..].chunks(mode.block));
        for (i, bytes) in chunks.enumerate() {
            let codec = if mode.raw_prefix && i == 0 {
                Codec::Raw
            } else {
                mode.codec
            };
            let compressed = encode(bytes, codec)?;
            let raw = compressed.len() >= bytes.len();
            let payload = if raw { bytes } else { &compressed };
            file.write_all(payload)?;
            list.push(Block {
                start: size,
                stored: payload.len(),
                len: bytes.len(),
                hash: xxh3_64(bytes),
                raw,
            });
            size += payload.len();
        }
        blocks.push(Some(list));
    }
    file.flush()?;
    // Private immutable tempfile; editable sources are never mapped.
    let map = if size == 0 {
        None
    } else {
        Some(unsafe { memmap2::Mmap::map(&file)? })
    };
    Ok(Pack {
        mode,
        blocks,
        map,
        _file: file,
        size: size as u64,
        build_seconds: now.elapsed().as_secs_f64(),
    })
}
fn packed_match(blocks: &[Block], data: &[u8], codec: Codec, finder: &Finder<'_>) -> Result<bool> {
    let mut decoded = Vec::new();
    let mut window = Vec::new();
    let overlap = finder.needle().len().saturating_sub(1);
    for b in blocks {
        let bytes = if b.raw {
            let bytes = data
                .get(b.start..b.start.checked_add(b.stored).context("overflow")?)
                .context("truncated")?;
            ensure!(
                bytes.len() == b.len && xxh3_64(bytes) == b.hash,
                "raw integrity"
            );
            bytes
        } else {
            decode(b, data, codec, &mut decoded)?;
            &decoded
        };
        if !window.is_empty() {
            let old = window.len();
            window.extend_from_slice(&bytes[..bytes.len().min(overlap)]);
            if finder.find(&window).is_some() {
                return Ok(true);
            }
            window.truncate(old);
        }
        if finder.find(bytes).is_some() {
            return Ok(true);
        }
        if bytes.len() >= overlap {
            window.clear();
            window.extend_from_slice(&bytes[bytes.len() - overlap..]);
        } else {
            window.extend_from_slice(bytes);
            let keep = overlap.min(window.len());
            window.drain(..window.len() - keep);
        }
    }
    Ok(finder.needle().is_empty())
}
fn search(
    entry: &Entry,
    blocks: Option<&Vec<Block>>,
    pack: &Pack,
    finder: &Finder<'_>,
) -> Result<bool> {
    if stamp(&entry.path)? != entry.stamp || blocks.is_none() {
        return Ok(finder
            .find(fs::read_to_string(&entry.path)?.as_bytes())
            .is_some());
    }
    packed_match(
        blocks.unwrap(),
        pack.map.as_deref().unwrap_or(&[]),
        pack.mode.codec,
        finder,
    )
}
fn main() -> Result<()> {
    ensure!(cfg!(unix), "lab requires Unix freshness metadata");
    let root =
        PathBuf::from(std::env::args().nth(1).context("ROOT [REPETITIONS]")?).canonicalize()?;
    let repetitions = std::env::args()
        .nth(2)
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(11);
    ensure!(repetitions > 0, "repetitions");
    let reader = IndexReader::open_uncached(&root)?;
    let mut entries = Vec::new();
    for doc in reader.documents().iter().filter(|d| d.is_valid()) {
        let path = reader.get_full_path(doc).context("path")?;
        let before = stamp(&path)?;
        let text = fs::read_to_string(&path)?.into_bytes();
        ensure!(before == stamp(&path)?, "source changed");
        entries.push(Entry {
            path,
            text,
            stamp: before,
            id: doc.doc_id,
        });
    }
    let modes = [
        Mode {
            name: "raw-4k",
            codec: Codec::Raw,
            block: 4096,
            raw_prefix: false,
            max_file: usize::MAX,
        },
        Mode {
            name: "lz4-4k",
            codec: Codec::Lz4,
            block: 4096,
            raw_prefix: false,
            max_file: usize::MAX,
        },
        Mode {
            name: "lz4-16k",
            codec: Codec::Lz4,
            block: 16384,
            raw_prefix: false,
            max_file: usize::MAX,
        },
        Mode {
            name: "lz4-64k",
            codec: Codec::Lz4,
            block: 65536,
            raw_prefix: false,
            max_file: usize::MAX,
        },
        Mode {
            name: "zstd1-16k",
            codec: Codec::Zstd(1),
            block: 16384,
            raw_prefix: false,
            max_file: usize::MAX,
        },
        Mode {
            name: "zstd3-16k",
            codec: Codec::Zstd(3),
            block: 16384,
            raw_prefix: false,
            max_file: usize::MAX,
        },
        Mode {
            name: "prefix-lz4-16k",
            codec: Codec::Lz4,
            block: 16384,
            raw_prefix: true,
            max_file: usize::MAX,
        },
        Mode {
            name: "prefix-zstd1-16k",
            codec: Codec::Zstd(1),
            block: 16384,
            raw_prefix: true,
            max_file: usize::MAX,
        },
        Mode {
            name: "raw-under64k",
            codec: Codec::Raw,
            block: 4096,
            raw_prefix: false,
            max_file: 65536,
        },
    ];
    let mut packs = Vec::new();
    for mode in modes {
        let p = build(&entries, mode)?;
        eprintln!("{}: {} bytes, {:.3}s", mode.name, p.size, p.build_seconds);
        packs.push(p);
    }
    // Exact round trips for every block of every variant, outside query timing.
    for p in &packs {
        for (entry, blocks) in entries.iter().zip(&p.blocks) {
            if let Some(blocks) = blocks {
                let mut offset = 0;
                let mut out = Vec::new();
                for b in blocks {
                    decode(b, p.map.as_deref().unwrap_or(&[]), p.mode.codec, &mut out)?;
                    ensure!(entry.text[offset..offset + out.len()] == out, "roundtrip");
                    offset += out.len();
                }
                ensure!(offset == entry.text.len(), "full length");
            }
        }
    }
    let patterns = [
        ("auditNonexistentSymbol94283", false),
        ("folio_wait_bit_common", false),
        ("struct file_operations", false),
        ("return", false),
        ("unlikely(", false),
        ("Copyright", false),
        ("auditNonexistentSymbol94283", true),
    ];
    let mut rows = Vec::new();
    for (pattern, force_scan) in patterns {
        let finder = Finder::new(pattern.as_bytes());
        let grams: Vec<_> = fxi::utils::query_trigrams(pattern)
            .into_iter()
            .filter(|g| !reader.is_stop_gram(*g))
            .collect();
        let ids = if force_scan || grams.is_empty() {
            reader.valid_doc_ids().clone()
        } else {
            reader.get_trigram_docs_with_bloom(&grams) & reader.valid_doc_ids()
        };
        let candidates: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| ids.contains(e.id))
            .map(|(i, _)| i)
            .collect();
        // Independent whole-source oracle also catches candidate omissions.
        let expected: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| finder.find(&e.text).is_some())
            .map(|(i, _)| i)
            .collect();
        let mut samples = vec![Vec::new(); packs.len()];
        for rep in 0..=repetitions {
            for step in 0..packs.len() {
                let mode = if rep % 2 == 0 {
                    (step + rep) % packs.len()
                } else {
                    (packs.len() - 1 - step + rep) % packs.len()
                };
                let p = &packs[mode];
                let start = Instant::now();
                let results: Result<Vec<_>> = candidates
                    .par_iter()
                    .with_min_len((candidates.len() / 4).max(1))
                    .map(|&i| {
                        search(&entries[i], p.blocks[i].as_ref(), p, &finder).map(|hit| (i, hit))
                    })
                    .collect();
                let actual: Vec<_> = results?
                    .into_iter()
                    .filter(|(_, hit)| *hit)
                    .map(|(i, _)| i)
                    .collect();
                let ms = start.elapsed().as_secs_f64() * 1000.;
                ensure!(actual == expected, "mismatch {} {}", p.mode.name, pattern);
                if rep != 0 {
                    samples[mode].push(ms);
                }
            }
        }
        rows.push(serde_json::json!({"pattern":pattern,"force_scan":force_scan,"candidates":candidates.len(),"matches":expected.len(),"modes":packs.iter().enumerate().map(|(i,p)|{let mut sorted=samples[i].clone();sorted.sort_by(f64::total_cmp);serde_json::json!({"name":p.mode.name,"median_ms":sorted[sorted.len()/2],"samples_ms":samples[i]})}).collect::<Vec<_>>()}));
        eprintln!("verified {pattern}");
    }
    for e in &entries {
        ensure!(
            stamp(&e.path)? == e.stamp,
            "source changed during experiment"
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"root":root,"files":entries.len(),"source_bytes":entries.iter().map(|e|e.text.len()).sum::<usize>(),"variants":packs.iter().map(|p|serde_json::json!({"name":p.mode.name,"payload_bytes":p.size,"blocks":p.blocks.iter().flatten().map(Vec::len).sum::<usize>(),"packed_files":p.blocks.iter().filter(|b|b.is_some()).count(),"encode_seconds":p.build_seconds})).collect::<Vec<_>>(),"rows":rows,"limits":"Offline warm-filesystem verification stage, four parallel tasks; index opening, planning, CLI and serialization excluded. Raw baseline scans borrowed blocks; production CLI must still be compared separately before shipping. Payload sizes exclude serialized metadata. Full exact roundtrip and whole-source oracle checked. All source bytes resident for oracle; this is not a cold-storage/RSS benchmark. Compression build serial; times are not full index builds."})
        )?
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codecs_boundaries_and_corruption() -> Result<()> {
        for codec in [Codec::Raw, Codec::Lz4, Codec::Zstd(1), Codec::Zstd(3)] {
            for prefix in [false, true] {
                let dir = tempfile::tempdir()?;
                let path = dir.path().join("source");
                let text = format!(
                    "{}éclair{}longneedle{}",
                    "x".repeat(4094),
                    "y".repeat(27),
                    "z".repeat(9000)
                );
                fs::write(&path, &text)?;
                let entries = [Entry {
                    stamp: stamp(&path)?,
                    path,
                    text: text.as_bytes().to_vec(),
                    id: 1,
                }];
                let p = build(
                    &entries,
                    Mode {
                        name: "test",
                        codec,
                        block: 31,
                        raw_prefix: prefix,
                        max_file: usize::MAX,
                    },
                )?;
                for needle in ["éclair", "longneedle", "absent", "", &text[5000..5100]] {
                    assert_eq!(
                        search(
                            &entries[0],
                            p.blocks[0].as_ref(),
                            &p,
                            &Finder::new(needle.as_bytes())
                        )?,
                        text.contains(needle)
                    );
                }
                let mut bytes = p.map.as_deref().unwrap().to_vec();
                bytes[0] ^= 0xff;
                assert!(
                    packed_match(
                        p.blocks[0].as_ref().unwrap(),
                        &bytes,
                        codec,
                        &Finder::new(b"absent")
                    )
                    .is_err()
                );
                assert!(
                    packed_match(
                        p.blocks[0].as_ref().unwrap(),
                        &bytes[..1],
                        codec,
                        &Finder::new(b"absent")
                    )
                    .is_err()
                );
                fs::write(&entries[0].path, "new")?;
                assert!(search(
                    &entries[0],
                    p.blocks[0].as_ref(),
                    &p,
                    &Finder::new(b"new")
                )?);
                assert!(!search(
                    &entries[0],
                    p.blocks[0].as_ref(),
                    &p,
                    &Finder::new(b"longneedle")
                )?);
            }
        }
        Ok(())
    }
}
