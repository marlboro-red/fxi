use crate::index::reader::IndexReader;
use crate::utils::{find_codebase_root, get_index_dir, list_indexed_codebases};
use anyhow::Result;
use std::path::Path;

/// Display index statistics
pub fn show_stats(root_path: &Path) -> Result<()> {
    // Auto-detect codebase root
    let root = find_codebase_root(root_path)?;
    let reader = IndexReader::open(&root)?;
    let index_path = get_index_dir(&root)?;

    println!("Index Statistics");
    println!("================");
    println!();
    println!("Root path:        {}", reader.root_path().display());
    println!("Index location:   {}", index_path.display());
    println!("Index version:    {}", reader.meta.version);
    println!("Document count:   {}", reader.meta.doc_count);
    println!("Segment count:    {}", reader.meta.segment_count);
    println!("Stop-grams:       {}", reader.meta.stop_grams.len());

    // Count by language
    let docs = reader.documents();
    let mut lang_counts = std::collections::HashMap::new();
    for doc in docs {
        *lang_counts.entry(format!("{:?}", doc.language)).or_insert(0) += 1;
    }

    println!();
    println!("Files by language:");
    let mut sorted: Vec<_> = lang_counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    for (lang, count) in sorted.iter().take(15) {
        println!("  {:15} {}", lang, count);
    }

    if sorted.len() > 15 {
        println!("  ... and {} more", sorted.len() - 15);
    }

    // Index size
    if let Ok(size) = dir_size(&index_path) {
        println!();
        println!("Index size:       {}", format_size(size));
    }

    // Timestamps
    println!();
    println!(
        "Created:          {}",
        format_timestamp(reader.meta.created_at)
    );
    println!(
        "Updated:          {}",
        format_timestamp(reader.meta.updated_at)
    );

    Ok(())
}

/// List all indexed codebases
pub fn list_indexes() -> Result<()> {
    let codebases = list_indexed_codebases()?;

    if codebases.is_empty() {
        println!("No indexed codebases found.");
        return Ok(());
    }

    println!("Indexed Codebases");
    println!("=================");
    println!();

    for codebase in codebases {
        let exists = codebase.root_path.exists();
        let status = if exists { "" } else { " [missing]" };
        println!("  {}{}", codebase.root_path.display(), status);
        println!("    Index: {}", codebase.index_dir.display());
        println!();
    }

    Ok(())
}

/// Calculate directory size recursively
fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut size = 0;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                size += entry.metadata()?.len();
            } else if path.is_dir() {
                size += dir_size(&path)?;
            }
        }
    }
    Ok(size)
}

/// Format byte size to human readable
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}

/// Format unix timestamp as human-readable date string
fn format_timestamp(ts: u64) -> String {
    // Convert to a basic readable format without external crate
    let secs = ts;
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Calculate date from days since epoch (1970-01-01)
    let mut remaining_days = days as i64;
    let mut year: i64 = 1970;

    loop {
        let days_in_year: i64 = if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let month_days: [i64; 12] = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining_days < md {
            month = i + 1;
            break;
        }
        remaining_days -= md;
    }
    if month == 0 { month = 12; }
    let day = remaining_days + 1;

    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC", year, month, day, hours, minutes, seconds)
}
