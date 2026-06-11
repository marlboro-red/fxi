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
    let choice = if color {
        ColorChoice::Auto
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
    let mut current_file: Option<&std::path::Path> = None;
    let mut last_line_num: Option<u32> = None;

    for m in matches {
        let is_new_file = current_file.map(|p| p != m.path).unwrap_or(true);

        if is_new_file {
            if current_file.is_some() {
                // Add blank line between files
                writeln!(stdout)?;
            }

            if heading {
                // Print filename header
                stdout.set_color(&colors.path_heading)?;
                writeln!(stdout, "{}", m.path.display())?;
                stdout.reset()?;
            }

            current_file = Some(&m.path);
            last_line_num = None;
        }

        // Print context separator if there's a gap
        if let Some(last) = last_line_num {
            let expected_next = last + 1;
            let first_ctx_line = m
                .context_before
                .first()
                .map(|(n, _)| *n)
                .unwrap_or(m.line_number);

            if first_ctx_line > expected_next {
                stdout.set_color(&colors.separator)?;
                writeln!(stdout, "--")?;
                stdout.reset()?;
            }
        }

        // Print context before
        for (line_num, content) in &m.context_before {
            print_context_line(&mut stdout, &colors, &m.path, *line_num, content, heading)?;
        }

        // Print the match line
        print_match_line(
            &mut stdout,
            &colors,
            &m.path,
            m.line_number,
            &m.line_content,
            m.match_start,
            m.match_end,
            heading,
        )?;

        // Print context after
        for (line_num, content) in &m.context_after {
            print_context_line(&mut stdout, &colors, &m.path, *line_num, content, heading)?;
        }

        // Track last line for gap detection
        last_line_num = Some(
            m.context_after
                .last()
                .map(|(n, _)| *n)
                .unwrap_or(m.line_number),
        );
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

/// Print only filenames (for -l flag)
pub fn print_files_only(matches: &[ContentMatch], color: bool) -> io::Result<()> {
    let mut stdout = buffered_stdout(color);
    let colors = Colors::new();

    let mut seen_files = std::collections::HashSet::new();

    for m in matches {
        if seen_files.insert(m.path.as_path()) {
            stdout.set_color(&colors.path)?;
            writeln!(stdout, "{}", m.path.display())?;
            stdout.reset()?;
        }
    }

    stdout.flush()
}

/// Print match count per file (for -c flag)
pub fn print_match_counts(matches: &[ContentMatch], color: bool) -> io::Result<()> {
    let mut stdout = buffered_stdout(color);
    let colors = Colors::new();

    let mut counts: std::collections::HashMap<&std::path::Path, usize> =
        std::collections::HashMap::new();

    for m in matches {
        *counts.entry(&m.path).or_insert(0) += 1;
    }

    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));

    for (path, count) in sorted {
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
