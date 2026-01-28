use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::path::Path;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Maximum lines to highlight for performance (prevents UI blocking on huge files)
const MAX_HIGHLIGHT_LINES: usize = 3000;

/// Number of lines to highlight around the visible viewport
const VIEWPORT_BUFFER_LINES: usize = 50;

/// Maximum line number from which to start highlighting
/// Beyond this, syntax highlighting is skipped entirely for performance
/// (syntect requires sequential processing from line 0)
const MAX_HIGHLIGHT_START_LINE: usize = 500;

/// Syntax highlighter for code preview
pub struct SyntaxHighlighter {
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
}

impl SyntaxHighlighter {
    /// Create a new syntax highlighter with default syntax definitions and themes
    pub fn new() -> Self {
        Self {
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
        }
    }

    /// Highlight only a specific range of lines (for lazy/incremental highlighting)
    /// start_line and end_line are 0-indexed
    #[allow(dead_code)]
    pub fn highlight_range(
        &self,
        content: &str,
        file_path: &Path,
        start_line: usize,
        end_line: usize,
    ) -> Vec<Vec<Span<'static>>> {
        let extension = file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let syntax = self
            .syntax_set
            .find_syntax_by_extension(extension)
            .or_else(|| {
                content
                    .lines()
                    .next()
                    .and_then(|line| self.syntax_set.find_syntax_by_first_line(line))
            })
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let theme = &self.theme_set.themes["base16-eighties.dark"];
        let mut highlighter = HighlightLines::new(syntax, theme);

        let mut result = Vec::with_capacity(end_line.saturating_sub(start_line) + 1);

        // Process lines, but we need to process from the beginning to maintain state
        // Only collect spans for lines within the range
        for (idx, line) in LinesWithEndings::from(content).enumerate() {
            let ranges = highlighter
                .highlight_line(line, &self.syntax_set)
                .unwrap_or_default();

            if idx >= start_line && idx <= end_line {
                let spans: Vec<Span<'static>> = ranges
                    .into_iter()
                    .map(|(style, text)| {
                        let fg = syntect_color_to_ratatui(style.foreground);
                        let mut ratatui_style = Style::default().fg(fg);

                        if style.font_style.contains(FontStyle::BOLD) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::BOLD);
                        }
                        if style.font_style.contains(FontStyle::ITALIC) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::ITALIC);
                        }
                        if style.font_style.contains(FontStyle::UNDERLINE) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::UNDERLINED);
                        }

                        let clean_text = text.trim_end_matches('\n').trim_end_matches('\r');
                        Span::styled(clean_text.to_string(), ratatui_style)
                    })
                    .collect();

                result.push(spans);
            }

            // Stop processing once we've passed the end_line
            if idx > end_line {
                break;
            }
        }

        result
    }

    /// Viewport-optimized highlighting: only highlight the visible region plus a buffer
    /// This is much faster for large files where we only display a small viewport
    /// Returns (highlighted_lines, start_line_offset) so caller can map indices correctly
    pub fn highlight_viewport(
        &self,
        content: &str,
        file_path: &Path,
        viewport_start: usize,
        viewport_height: usize,
    ) -> (Vec<Vec<Span<'static>>>, usize) {
        // Skip syntax highlighting for deep line numbers - syntect requires
        // sequential processing from line 0, making it O(line_number)
        if viewport_start > MAX_HIGHLIGHT_START_LINE {
            return (Vec::new(), viewport_start);
        }

        let total_lines = content.lines().count();

        // Calculate the range to highlight with buffer on both sides
        let highlight_start = viewport_start.saturating_sub(VIEWPORT_BUFFER_LINES);
        let highlight_end = (viewport_start + viewport_height + VIEWPORT_BUFFER_LINES)
            .min(total_lines)
            .min(highlight_start + MAX_HIGHLIGHT_LINES);

        let extension = file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let syntax = self
            .syntax_set
            .find_syntax_by_extension(extension)
            .or_else(|| {
                content
                    .lines()
                    .next()
                    .and_then(|line| self.syntax_set.find_syntax_by_first_line(line))
            })
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let theme = &self.theme_set.themes["base16-eighties.dark"];
        let mut highlighter = HighlightLines::new(syntax, theme);

        let mut result = Vec::with_capacity(highlight_end.saturating_sub(highlight_start));

        // For syntect, we need to process from the beginning to maintain state
        // But we can skip collecting spans until we reach the start
        for (idx, line) in LinesWithEndings::from(content).enumerate() {
            if idx >= highlight_end {
                break;
            }

            let ranges = highlighter
                .highlight_line(line, &self.syntax_set)
                .unwrap_or_default();

            if idx >= highlight_start {
                let spans: Vec<Span<'static>> = ranges
                    .into_iter()
                    .map(|(style, text)| {
                        let fg = syntect_color_to_ratatui(style.foreground);
                        let mut ratatui_style = Style::default().fg(fg);

                        if style.font_style.contains(FontStyle::BOLD) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::BOLD);
                        }
                        if style.font_style.contains(FontStyle::ITALIC) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::ITALIC);
                        }
                        if style.font_style.contains(FontStyle::UNDERLINE) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::UNDERLINED);
                        }

                        let clean_text = text.trim_end_matches('\n').trim_end_matches('\r');
                        Span::styled(clean_text.to_string(), ratatui_style)
                    })
                    .collect();

                result.push(spans);
            }
        }

        (result, highlight_start)
    }
}

/// Convert syntect color to ratatui color
fn syntect_color_to_ratatui(color: syntect::highlighting::Color) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

impl Default for SyntaxHighlighter {
    fn default() -> Self {
        Self::new()
    }
}
