//! Output formatting for search results.
//!
//! This module formats search results in a ripgrep-compatible style,
//! providing familiar output for command-line users.
//!
//! ## Output Modes
//!
//! - **Default**: File path, line number, and highlighted matches
//! - **Heading mode** (`--heading`): Group results by file
//! - **Files only** (`-l`): Print only matching file names
//! - **Count** (`-c`): Print match count per file
//!
//! ## Color Scheme
//!
//! - **Magenta**: File paths
//! - **Green**: Line numbers
//! - **Red (bold)**: Match highlights
//! - **Cyan**: Context separators
//!
//! ## Example Output
//!
//! ```text
//! src/main.rs
//! 42:    let result = search_index("query");
//! 43-    println!("{:?}", result);
//! --
//! 100:   search_index("another query");
//! ```

use crate::server::protocol::ContentMatch;
use std::io::{self, Write};
use termcolor::{BufferedStandardStream, Color, ColorChoice, ColorSpec, WriteColor};

/// Color specs built once per print call instead of per output line.
struct Colors {
    path: ColorSpec,
    path_heading: ColorSpec,
    line_num: ColorSpec,
    separator: ColorSpec,
    highlight: ColorSpec,
}

impl Colors {
    fn new() -> Self {
        let mut path = ColorSpec::new();
        path.set_fg(Some(Color::Magenta));
        let mut path_heading = ColorSpec::new();
        path_heading.set_fg(Some(Color::Magenta)).set_bold(true);
        let mut line_num = ColorSpec::new();
        line_num.set_fg(Some(Color::Green));
        let mut separator = ColorSpec::new();
        separator.set_fg(Some(Color::Cyan));
        let mut highlight = ColorSpec::new();
        highlight.set_fg(Some(Color::Red)).set_bold(true);
        Self {
            path,
            path_heading,
            line_num,
            separator,
            highlight,
        }
    }
}

/// Buffered stdout: one syscall per buffer instead of one per line.
fn buffered_stdout(color: bool) -> BufferedStandardStream {
    // The caller has already resolved --color (always/never/auto with tty
    // detection) into a bool, so force here: termcolor's Auto would consult
    // TERM and silently drop colors in environments where it is unset
    // (breaking --color=always under pipes and on CI)
    let choice = if color {
        ColorChoice::Always
    } else {
        ColorChoice::Never
    };
    BufferedStandardStream::stdout(choice)
}

/// Print content matches in ripgrep-style format
pub fn print_content_matches(
    matches: &[ContentMatch],
    color: bool,
    heading: bool,
) -> io::Result<()> {
    let mut stdout = buffered_stdout(color);

    if matches.is_empty() {
        return Ok(());
    }

    let colors = Colors::new();
    // Merge overlapping context intervals and promote context to a match when
    // either role references the same line. Every displayed line appears once.
    enum Row<'a> {
        Context(&'a str),
        Match(&'a ContentMatch),
    }
    let mut files: std::collections::BTreeMap<
        &std::path::Path,
        std::collections::BTreeMap<u32, Row<'_>>,
    > = std::collections::BTreeMap::new();
    let with_context = matches
        .iter()
        .any(|m| !m.context_before.is_empty() || !m.context_after.is_empty());
    for m in matches {
        let rows = files.entry(m.path.as_path()).or_default();
        for (number, text) in m.context_before.iter().chain(&m.context_after) {
            rows.entry(*number).or_insert(Row::Context(text));
        }
        rows.insert(m.line_number, Row::Match(m));
    }
    for (file_index, (path, rows)) in files.into_iter().enumerate() {
        if file_index != 0 && heading {
            writeln!(stdout)?;
        }
        if heading {
            stdout.set_color(&colors.path_heading)?;
            writeln!(stdout, "{}", path.display())?;
            stdout.reset()?;
        }
        let mut previous = None;
        for (number, row) in rows {
            if with_context && previous.is_some_and(|last: u32| number > last.saturating_add(1)) {
                stdout.set_color(&colors.separator)?;
                writeln!(stdout, "--")?;
                stdout.reset()?;
            }
            match row {
                Row::Context(text) => {
                    print_context_line(&mut stdout, &colors, path, number, text, heading)?
                }
                Row::Match(m) => print_match_line(
                    &mut stdout,
                    &colors,
                    path,
                    number,
                    &m.line_content,
                    m.match_start,
                    m.match_end,
                    heading,
                )?,
            }
            previous = Some(number);
        }
    }

    stdout.flush()
}

/// Print a context line (non-matching)
fn print_context_line(
    stdout: &mut BufferedStandardStream,
    colors: &Colors,
    path: &std::path::Path,
    line_num: u32,
    content: &str,
    heading: bool,
) -> io::Result<()> {
    if !heading {
        // Print path prefix when not using heading mode
        stdout.set_color(&colors.path)?;
        write!(stdout, "{}", path.display())?;
        stdout.reset()?;
        write!(stdout, "-")?;
    }

    // Print line number
    stdout.set_color(&colors.line_num)?;
    write!(stdout, "{}", line_num)?;
    stdout.reset()?;
    write!(stdout, "-")?;

    // Print content
    writeln!(stdout, "{}", content)?;

    Ok(())
}

/// Print a match line with highlighted match
#[allow(clippy::too_many_arguments)]
fn print_match_line(
    stdout: &mut BufferedStandardStream,
    colors: &Colors,
    path: &std::path::Path,
    line_num: u32,
    content: &str,
    match_start: usize,
    match_end: usize,
    heading: bool,
) -> io::Result<()> {
    if !heading {
        // Print path prefix when not using heading mode
        stdout.set_color(&colors.path)?;
        write!(stdout, "{}", path.display())?;
        stdout.reset()?;
        write!(stdout, ":")?;
    }

    // Print line number
    stdout.set_color(&colors.line_num)?;
    write!(stdout, "{}", line_num)?;
    stdout.reset()?;
    write!(stdout, ":")?;

    // Print content with match highlighted
    let bytes = content.as_bytes();
    let safe_start = match_start.min(bytes.len());
    let safe_end = match_end.min(bytes.len());

    // Text before match
    if safe_start > 0 {
        write!(stdout, "{}", &content[..safe_start])?;
    }

    // The match itself (highlighted)
    if safe_end > safe_start {
        stdout.set_color(&colors.highlight)?;
        write!(stdout, "{}", &content[safe_start..safe_end])?;
        stdout.reset()?;
    }

    // Text after match
    if safe_end < content.len() {
        write!(stdout, "{}", &content[safe_end..])?;
    }

    writeln!(stdout)?;

    Ok(())
}

pub fn print_file_paths(paths: &[std::path::PathBuf], color: bool) -> io::Result<()> {
    print_path_iter(paths.iter().map(|p| p.as_path()), color)
}

fn print_path_iter<'a>(
    paths: impl IntoIterator<Item = &'a std::path::Path>,
    color: bool,
) -> io::Result<()> {
    let mut stdout = buffered_stdout(color);
    let colors = Colors::new();
    let mut seen_files = std::collections::HashSet::new();
    for path in paths {
        if seen_files.insert(path) {
            stdout.set_color(&colors.path)?;
            writeln!(stdout, "{}", path.display())?;
            stdout.reset()?;
        }
    }
    stdout.flush()
}

/// Print match count per file (for -c flag)
pub fn print_match_counts(matches: &[ContentMatch], color: bool) -> io::Result<()> {
    let mut counts: std::collections::HashMap<&std::path::Path, usize> =
        std::collections::HashMap::new();

    for m in matches {
        *counts.entry(&m.path).or_insert(0) += 1;
    }

    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));

    print_count_iter(sorted, color)
}

/// Print already aggregated path/count pairs in the supplied order.
pub fn print_file_counts(counts: &[(std::path::PathBuf, usize)], color: bool) -> io::Result<()> {
    print_count_iter(
        counts.iter().map(|(path, count)| (path.as_path(), *count)),
        color,
    )
}

fn print_count_iter<'a>(
    counts: impl IntoIterator<Item = (&'a std::path::Path, usize)>,
    color: bool,
) -> io::Result<()> {
    let mut stdout = buffered_stdout(color);
    let colors = Colors::new();
    for (path, count) in counts {
        stdout.set_color(&colors.path)?;
        write!(stdout, "{}", path.display())?;
        stdout.reset()?;
        write!(stdout, ":")?;
        stdout.set_color(&colors.line_num)?;
        writeln!(stdout, "{}", count)?;
        stdout.reset()?;
    }

    stdout.flush()
}
