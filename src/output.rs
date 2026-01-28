//! Output formatting for ripgrep-like content search results

use crate::server::protocol::ContentMatch;
use std::io::{self, Write};
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

/// Print content matches in ripgrep-style format
pub fn print_content_matches(
    matches: &[ContentMatch],
    color: bool,
    heading: bool,
) -> io::Result<()> {
    let choice = if color {
        ColorChoice::Auto
    } else {
        ColorChoice::Never
    };
    let mut stdout = StandardStream::stdout(choice);

    if matches.is_empty() {
        return Ok(());
    }

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
                stdout.set_color(ColorSpec::new().set_fg(Some(Color::Magenta)).set_bold(true))?;
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
                stdout.set_color(ColorSpec::new().set_fg(Some(Color::Cyan)))?;
                writeln!(stdout, "--")?;
                stdout.reset()?;
            }
        }

        // Print context before
        for (line_num, content) in &m.context_before {
            print_context_line(&mut stdout, &m.path, *line_num, content, heading)?;
        }

        // Print the match line
        print_match_line(
            &mut stdout,
            &m.path,
            m.line_number,
            &m.line_content,
            m.match_start,
            m.match_end,
            heading,
        )?;

        // Print context after
        for (line_num, content) in &m.context_after {
            print_context_line(&mut stdout, &m.path, *line_num, content, heading)?;
        }

        // Track last line for gap detection
        last_line_num = Some(
            m.context_after
                .last()
                .map(|(n, _)| *n)
                .unwrap_or(m.line_number),
        );
    }

    Ok(())
}

/// Print a context line (non-matching)
fn print_context_line(
    stdout: &mut StandardStream,
    path: &std::path::Path,
    line_num: u32,
    content: &str,
    heading: bool,
) -> io::Result<()> {
    if !heading {
        // Print path prefix when not using heading mode
        stdout.set_color(ColorSpec::new().set_fg(Some(Color::Magenta)))?;
        write!(stdout, "{}", path.display())?;
        stdout.reset()?;
        write!(stdout, "-")?;
    }

    // Print line number
    stdout.set_color(ColorSpec::new().set_fg(Some(Color::Green)))?;
    write!(stdout, "{}", line_num)?;
    stdout.reset()?;
    write!(stdout, "-")?;

    // Print content
    writeln!(stdout, "{}", content)?;

    Ok(())
}

/// Print a match line with highlighted match
fn print_match_line(
    stdout: &mut StandardStream,
    path: &std::path::Path,
    line_num: u32,
    content: &str,
    match_start: usize,
    match_end: usize,
    heading: bool,
) -> io::Result<()> {
    if !heading {
        // Print path prefix when not using heading mode
        stdout.set_color(ColorSpec::new().set_fg(Some(Color::Magenta)))?;
        write!(stdout, "{}", path.display())?;
        stdout.reset()?;
        write!(stdout, ":")?;
    }

    // Print line number
    stdout.set_color(ColorSpec::new().set_fg(Some(Color::Green)))?;
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
        stdout.set_color(ColorSpec::new().set_fg(Some(Color::Red)).set_bold(true))?;
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
pub fn print_files_only(matches: &[ContentMatch]) -> io::Result<()> {
    let choice = ColorChoice::Auto;
    let mut stdout = StandardStream::stdout(choice);

    let mut seen_files = std::collections::HashSet::new();

    for m in matches {
        if seen_files.insert(m.path.clone()) {
            stdout.set_color(ColorSpec::new().set_fg(Some(Color::Magenta)))?;
            writeln!(stdout, "{}", m.path.display())?;
            stdout.reset()?;
        }
    }

    Ok(())
}

/// Print match count per file (for -c flag)
pub fn print_match_counts(matches: &[ContentMatch]) -> io::Result<()> {
    let choice = ColorChoice::Auto;
    let mut stdout = StandardStream::stdout(choice);

    let mut counts: std::collections::HashMap<&std::path::Path, usize> =
        std::collections::HashMap::new();

    for m in matches {
        *counts.entry(&m.path).or_insert(0) += 1;
    }

    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));

    for (path, count) in sorted {
        stdout.set_color(ColorSpec::new().set_fg(Some(Color::Magenta)))?;
        write!(stdout, "{}", path.display())?;
        stdout.reset()?;
        write!(stdout, ":")?;
        stdout.set_color(ColorSpec::new().set_fg(Some(Color::Green)))?;
        writeln!(stdout, "{}", count)?;
        stdout.reset()?;
    }

    Ok(())
}
