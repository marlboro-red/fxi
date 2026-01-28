use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::path::Path;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Maximum lines to highlight for performance (prevents UI blocking on huge files)
const MAX_HIGHLIGHT_LINES: usize = 3000;

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

    /// Highlight a file's content and return lines of styled spans
    /// Limits highlighting to MAX_HIGHLIGHT_LINES for performance
    pub fn highlight_content(
        &self,
        content: &str,
        file_path: &Path,
    ) -> Vec<Vec<Span<'static>>> {
        // Get the syntax definition based on file extension
        let extension = file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let syntax = self
            .syntax_set
            .find_syntax_by_extension(extension)
            .or_else(|| {
                // Try to find by first line (e.g., shebang)
                content
                    .lines()
                    .next()
                    .and_then(|line| self.syntax_set.find_syntax_by_first_line(line))
            })
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        // Use base16-eighties.dark theme for terminal-friendly colors
        let theme = &self.theme_set.themes["base16-eighties.dark"];
        let mut highlighter = HighlightLines::new(syntax, theme);

        let mut result = Vec::with_capacity(content.lines().count().min(MAX_HIGHLIGHT_LINES));
        let mut line_count = 0;

        for line in LinesWithEndings::from(content) {
            // Limit highlighting to prevent UI blocking on huge files
            if line_count >= MAX_HIGHLIGHT_LINES {
                // For remaining lines, add empty spans (will fall back to plain text in UI)
                break;
            }
            line_count += 1;

            let ranges = highlighter
                .highlight_line(line, &self.syntax_set)
                .unwrap_or_default();

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

                    // Remove trailing newline for clean display
                    let clean_text = text.trim_end_matches('\n').trim_end_matches('\r');
                    Span::styled(clean_text.to_string(), ratatui_style)
                })
                .collect();

            result.push(spans);
        }

        result
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
}

impl Default for SyntaxHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert syntect color to ratatui color
fn syntect_color_to_ratatui(color: syntect::highlighting::Color) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}
