//! macOS metadata batching experiment; never used by production search.
//! cargo run --release --example metadata_lab -- CONTROLLED_CORPUS
#[cfg(target_os = "macos")]
mod mac {
    use anyhow::{Context, Result, ensure};
    use rayon::prelude::*;
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::fs::{File, Metadata};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const ERROR: u32 = 0x2000_0000;
    const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_CRTIME
        | libc::ATTR_CMN_MODTIME
        | libc::ATTR_CMN_CHGTIME
        | libc::ATTR_CMN_FILEID
        | ERROR;

    #[derive(Debug, PartialEq, Eq)]
    struct Stamp {
        dev: u64,
        ino: u64,
        size: u64,
        modified: SystemTime,
        created: SystemTime,
        changed: (i64, i64),
    }
    impl Stamp {
        fn from_metadata(m: Metadata) -> Result<Self> {
            Ok(Self {
                dev: m.dev(),
                ino: m.ino(),
                size: m.len(),
                modified: m.modified()?,
                created: m.created()?,
                changed: (m.ctime(), m.ctime_nsec()),
            })
        }
    }
    fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
        Ok(u32::from_ne_bytes(
            bytes
                .get(offset..offset + 4)
                .context("truncated u32")?
                .try_into()?,
        ))
    }
    fn u64_at(bytes: &[u8], offset: usize) -> Result<u64> {
        Ok(u64::from_ne_bytes(
            bytes
                .get(offset..offset + 8)
                .context("truncated u64")?
                .try_into()?,
        ))
    }
    fn timestamp(bytes: &[u8], offset: usize) -> Result<SystemTime> {
        let seconds = u64_at(bytes, offset)? as i64;
        let nanos = u64_at(bytes, offset + 8)? as i64;
        ensure!((0..1_000_000_000).contains(&nanos), "invalid nanoseconds");
        let base = if seconds >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
        };
        base.and_then(|base| base.checked_add(Duration::from_nanos(nanos as u64)))
            .context("timestamp out of range")
    }

    // FSOPT_PACK_INVAL_ATTRS reserves the requested fields for regular files.
    // Other entry types can omit file-group fields; inspect validity first. Fields are packed to four-byte boundaries;
    // decode bytes rather than forming potentially unaligned typed references.
    fn record(bytes: &[u8]) -> Result<Option<(OsString, Stamp)>> {
        ensure!(
            bytes.len() >= 28 && u32_at(bytes, 0)? as usize == bytes.len(),
            "invalid record size"
        );
        if u32_at(bytes, 24)? != 0 {
            return Ok(None);
        }
        if u32_at(bytes, 4)? & (COMMON & !ERROR) != COMMON & !ERROR
            || u32_at(bytes, 16)? & libc::ATTR_FILE_DATALENGTH == 0
        {
            return Ok(None);
        }
        if u32_at(bytes, 40)? != 1 {
            return Ok(None);
        }
        ensure!(bytes.len() >= 108, "truncated regular-file attributes");
        let start = 28usize
            .checked_add_signed(u32_at(bytes, 28)? as i32 as isize)
            .context("name offset overflow")?;
        let end = start
            .checked_add(u32_at(bytes, 32)? as usize)
            .context("name length overflow")?;
        let name = bytes.get(start..end).context("name outside record")?;
        ensure!(
            start >= 108 && name.last() == Some(&0) && !name[..name.len() - 1].contains(&0),
            "invalid name"
        );
        ensure!(!name.contains(&b'/'), "invalid path component");
        Ok(Some((
            OsString::from_vec(name[..name.len() - 1].to_vec()),
            Stamp {
                dev: u32_at(bytes, 36)? as i32 as u64,
                ino: u64_at(bytes, 92)?,
                size: u64_at(bytes, 100)?,
                created: timestamp(bytes, 44)?,
                modified: timestamp(bytes, 60)?,
                changed: (u64_at(bytes, 76)? as i64, u64_at(bytes, 84)? as i64),
            },
        )))
    }

    fn directory(path: &Path) -> Result<Vec<(PathBuf, Stamp)>> {
        let file = File::open(path)?;
        let mut attrs = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: COMMON,
            volattr: 0,
            dirattr: 0,
            fileattr: libc::ATTR_FILE_DATALENGTH,
            forkattr: 0,
        };
        let mut buffer = vec![0u64; 8192]; // 64 KiB, eight-byte aligned.
        let mut entries = Vec::new();
        loop {
            // The descriptor owns an open directory. Both pointers reference
            // writable initialized allocations of the supplied sizes.
            let count = unsafe {
                libc::getattrlistbulk(
                    file.as_raw_fd(),
                    (&mut attrs as *mut libc::attrlist).cast(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len() * 8,
                    u64::from(libc::FSOPT_PACK_INVAL_ATTRS),
                )
            };
            if count < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if count == 0 {
                break;
            }
            // u64 has no invalid bit patterns; the kernel call has completed.
            let bytes = unsafe {
                std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8)
            };
            let mut cursor = 0usize;
            for _ in 0..count {
                let length = u32_at(bytes, cursor)? as usize;
                ensure!(
                    length > 0 && length.is_multiple_of(8),
                    "invalid record stride"
                );
                let end = cursor.checked_add(length).context("record overflow")?;
                if let Some((name, stamp)) =
                    record(bytes.get(cursor..end).context("truncated record")?)?
                {
                    entries.push((path.join(name), stamp));
                }
                cursor = end;
            }
        }
        Ok(entries)
    }

    pub fn run() -> Result<()> {
        let root = PathBuf::from(
            std::env::args()
                .nth(1)
                .context("CONTROLLED_CORPUS required")?,
        )
        .canonicalize()?;
        let mut files: Vec<_> = ignore::WalkBuilder::new(&root)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
            .map(|entry| entry.into_path())
            .collect();
        files.sort();
        let start = Instant::now();
        let dirs: Vec<_> = files
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let grouping_ms = start.elapsed().as_secs_f64() * 1000.0;
        let pool = rayon::ThreadPoolBuilder::new().num_threads(8).build()?;
        let stat = || -> Result<Vec<_>> {
            files
                .par_iter()
                .map(|path| {
                    Ok((
                        path.clone(),
                        Stamp::from_metadata(std::fs::metadata(path)?)?,
                    ))
                })
                .collect()
        };
        let bulk = || -> Result<Vec<_>> {
            Ok(dirs
                .par_iter()
                .map(|dir| directory(dir))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect())
        };
        let expected = pool.install(stat)?;
        let mut samples = [Vec::new(), Vec::new()];
        for rep in 0..12 {
            for mode in if rep % 2 == 0 { [0, 1] } else { [1, 0] } {
                let start = Instant::now();
                let mut found = if mode == 0 {
                    pool.install(stat)?
                } else {
                    pool.install(bulk)?
                };
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                found.sort_by(|a, b| a.0.cmp(&b.0));
                // Bulk reads all entries in selected directories. Compare only
                // the supplied eligible file set; discovery itself is excluded.
                found.retain(|(path, _)| files.binary_search(path).is_ok());
                ensure!(
                    found == expected,
                    "metadata mismatch (mode {mode}, {} vs {} records)",
                    found.len(),
                    expected.len()
                );
                if rep > 0 {
                    samples[mode].push(ms);
                }
            }
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"corpus": root,
            "files": files.len(), "directories": dirs.len(), "threads": 8,
            "grouping_ms": grouping_ms, "stat_samples_ms": samples[0], "bulk_samples_ms": samples[1],
            "note": "Primitive experiment only. Directory grouping is reported separately and reused; discovery and equality checks are outside timing. Every dev/inode/size/mtime/ctime/birthtime agrees with std metadata on the static fixture. No production integration or whole-query speed claim."}))?
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn native_bulk_matches_metadata_across_buffers_and_special_names() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("a")).unwrap();
            let mut paths = Vec::new();
            for i in 0..700 {
                let path = dir.path().join(format!("file-{i}-K.rs"));
                std::fs::write(&path, format!("value {i}")).unwrap();
                paths.push(path);
            }
            let raw = dir.path().join(OsString::from_vec(vec![0xff, b'x']));
            match std::fs::write(&raw, "raw path") {
                Ok(()) => paths.push(raw),
                Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => {}
                Err(error) => panic!("unexpected path creation error: {error}"),
            }
            std::os::unix::fs::symlink(&paths[0], dir.path().join("link")).unwrap();
            let found: std::collections::HashMap<_, _> =
                directory(dir.path()).unwrap().into_iter().collect();
            assert_eq!(found.len(), paths.len());
            for path in paths {
                assert_eq!(
                    found[&path],
                    Stamp::from_metadata(std::fs::metadata(&path).unwrap()).unwrap()
                );
            }
        }
        #[test]
        fn malformed_records_do_not_panic() {
            let mut state = 1u64;
            for len in 0..512 {
                let bytes: Vec<_> = (0..len)
                    .map(|_| {
                        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                        (state >> 32) as u8
                    })
                    .collect();
                assert!(record(&bytes).is_err());
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    mac::run()
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This offline experiment requires macOS.");
}
