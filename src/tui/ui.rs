use crate::tui::app::{App, Mode};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

pub fn draw(f: &mut Frame, app: &App) {
    // Clear the entire frame to prevent artifacts when content changes
    f.render_widget(Clear, f.area());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Query input
            Constraint::Min(10),   // Results / Preview
            Constraint::Length(1), // Status bar
        ])
        .split(f.area());

    draw_query_input(f, app, chunks[0]);
    draw_main_area(f, app, chunks[1]);
    draw_status_bar(f, app, chunks[2]);

    // Draw help panel overlay if in Help mode
    if app.mode == Mode::Help {
        draw_help_panel(f, f.area());
    }
}

fn draw_query_input(f: &mut Frame, app: &App, area: Rect) {
    let input = Paragraph::new(app.query.as_str())
        .style(Style::default().fg(Color::Yellow))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Search (F1: help) "),
        );

    f.render_widget(input, area);

    // Show cursor
    if app.mode == Mode::Search {
        f.set_cursor_position((area.x + app.query.len() as u16 + 1, area.y + 1));
    }
}

fn draw_main_area(f: &mut Frame, app: &App, area: Rect) {
    // When in help mode, draw the underlying mode's content
    let effective_mode = if app.mode == Mode::Help {
        app.previous_mode
    } else {
        app.mode
    };

    match effective_mode {
        Mode::Search | Mode::Help => {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);

            draw_results_list(f, app, chunks[0]);
            draw_preview(f, app, chunks[1]);
        }
        Mode::Preview => {
            draw_preview(f, app, area);
        }
    }
}

fn draw_results_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .results
        .iter()
        .enumerate()
        .map(|(i, result)| {
            let style = if i == app.selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let path_style = Style::default().fg(Color::Blue);
            let line_style = Style::default().fg(Color::Yellow);

            // Format: path:line  content
            let path_str = result.path.to_string_lossy();
            let line_num = result.line_number;

            // Trim leading whitespace and track offset adjustment
            let trimmed = result.line_content.trim_start();
            let trim_offset = result.line_content.len() - trimmed.len();
            let trimmed = trimmed.trim_end();

            // Truncate content if needed (using floor_char_boundary for UTF-8 safety)
            let max_content_len = area.width.saturating_sub(path_str.len() as u16 + 10) as usize;
            let (content, truncated) = if trimmed.len() > max_content_len {
                let truncate_at = trimmed.floor_char_boundary(max_content_len.saturating_sub(3));
                (format!("{}...", &trimmed[..truncate_at]), true)
            } else {
                (trimmed.to_string(), false)
            };

            // Build line with highlighted match
            let mut spans = vec![
                Span::styled(format!("{}:", path_str), path_style),
                Span::styled(format!("{}", line_num), line_style),
                Span::raw("  "),
            ];

            // Adjust match positions for trimming
            let adj_start = result.match_start.saturating_sub(trim_offset);
            let adj_end = result.match_end.saturating_sub(trim_offset);

            // Only highlight if match is within the displayed content
            if adj_start < content.len() && adj_end > adj_start {
                let end = adj_end.min(content.len());
                // Account for "..." if truncated and match extends past it
                let effective_end = if truncated && end > max_content_len.saturating_sub(3) {
                    max_content_len.saturating_sub(3)
                } else {
                    end
                };
                spans.extend(highlight_match(&content, adj_start, effective_end));
            } else {
                spans.push(Span::styled(
                    content,
                    Style::default().fg(Color::White),
                ));
            }

            let line = Line::from(spans);

            ListItem::new(line).style(style)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Results ({}) ", app.results.len())),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    f.render_widget(list, area);
}

fn draw_preview(f: &mut Frame, app: &App, area: Rect) {
    let title = if let Some(result) = app.get_selected_result() {
        format!(" {} ", result.path.display())
    } else {
        " Preview ".to_string()
    };

    let content = if let Some(ref preview) = app.preview_content {
        // Get the matched line number for highlighting
        let match_line = app
            .get_selected_result()
            .map(|r| r.line_number as usize)
            .unwrap_or(0);

        // Use syntax highlighting if we have a file path
        let highlighted_lines = app.preview_path.as_ref().map(|path| {
            app.highlighter.highlight_content(preview, path)
        });

        let lines: Vec<Line> = preview
            .lines()
            .enumerate()
            .skip(app.preview_scroll)
            .take(area.height.saturating_sub(2) as usize)
            .map(|(line_num, plain_line)| {
                let actual_line = line_num + 1;
                let is_match = actual_line == match_line;

                let line_num_style = Style::default().fg(Color::DarkGray);
                let mut spans = vec![
                    Span::styled(format!("{:4} ", actual_line), line_num_style),
                ];

                // Use highlighted spans if available, otherwise fall back to plain text
                if let Some(ref highlighted) = highlighted_lines {
                    if let Some(line_spans) = highlighted.get(line_num) {
                        if is_match {
                            // For matched line, add bold modifier to all spans
                            for span in line_spans {
                                let mut style = span.style;
                                style = style.add_modifier(Modifier::BOLD);
                                // Also add a subtle background to indicate match
                                style = style.bg(Color::Rgb(60, 60, 40));
                                spans.push(Span::styled(span.content.clone(), style));
                            }
                        } else {
                            spans.extend(line_spans.clone());
                        }
                    } else {
                        // Fallback for lines beyond highlighted content
                        let content_style = if is_match {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        spans.push(Span::styled(plain_line.to_string(), content_style));
                    }
                } else {
                    // No highlighting available, use plain text
                    let content_style = if is_match {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    spans.push(Span::styled(plain_line.to_string(), content_style));
                }

                Line::from(spans)
            })
            .collect();

        Text::from(lines)
    } else {
        Text::raw("No preview available")
    };

    let preview = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });

    f.render_widget(preview, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    // Pad the status message to fill the entire width to prevent artifacts
    let padded_message = format!("{:<width$}", app.status_message, width = area.width as usize);
    let status = Paragraph::new(padded_message)
        .style(Style::default().fg(Color::Cyan).bg(Color::Reset));

    f.render_widget(status, area);
}

fn draw_help_panel(f: &mut Frame, area: Rect) {
    // Calculate centered area for help panel
    let help_width = 60u16.min(area.width.saturating_sub(4));
    let help_height = 28u16.min(area.height.saturating_sub(2));
    let help_x = area.x + (area.width.saturating_sub(help_width)) / 2;
    let help_y = area.y + (area.height.saturating_sub(help_height)) / 2;
    let help_area = Rect::new(help_x, help_y, help_width, help_height);

    // Create help content
    let help_text = vec![
        Line::from(vec![
            Span::styled("  SEARCH MODE", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  F1 / ?       ", Style::default().fg(Color::Cyan)),
            Span::raw("Show this help"),
        ]),
        Line::from(vec![
            Span::styled("  Esc          ", Style::default().fg(Color::Cyan)),
            Span::raw("Clear query / Exit"),
        ]),
        Line::from(vec![
            Span::styled("  Enter        ", Style::default().fg(Color::Cyan)),
            Span::raw("Open file in editor"),
        ]),
        Line::from(vec![
            Span::styled("  Tab/Down     ", Style::default().fg(Color::Cyan)),
            Span::raw("Next result"),
        ]),
        Line::from(vec![
            Span::styled("  Shift+Tab/Up ", Style::default().fg(Color::Cyan)),
            Span::raw("Previous result"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+d       ", Style::default().fg(Color::Cyan)),
            Span::raw("Page down"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+u       ", Style::default().fg(Color::Cyan)),
            Span::raw("Page up"),
        ]),
        Line::from(vec![
            Span::styled("  gg / Ctrl+a  ", Style::default().fg(Color::Cyan)),
            Span::raw("First result"),
        ]),
        Line::from(vec![
            Span::styled("  G / Ctrl+e   ", Style::default().fg(Color::Cyan)),
            Span::raw("Last result"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+p       ", Style::default().fg(Color::Cyan)),
            Span::raw("Toggle preview mode"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+w       ", Style::default().fg(Color::Cyan)),
            Span::raw("Delete word"),
        ]),
        Line::from(vec![
            Span::styled("  F5           ", Style::default().fg(Color::Cyan)),
            Span::raw("Rebuild index"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  PREVIEW MODE", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  j/k          ", Style::default().fg(Color::Cyan)),
            Span::raw("Scroll down/up"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+d/u     ", Style::default().fg(Color::Cyan)),
            Span::raw("Half-page down/up"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+f/b     ", Style::default().fg(Color::Cyan)),
            Span::raw("Full page down/up"),
        ]),
        Line::from(vec![
            Span::styled("  gg / G       ", Style::default().fg(Color::Cyan)),
            Span::raw("Top / Bottom"),
        ]),
        Line::from(vec![
            Span::styled("  n / N        ", Style::default().fg(Color::Cyan)),
            Span::raw("Next / Previous result"),
        ]),
        Line::from(vec![
            Span::styled("  o / Enter    ", Style::default().fg(Color::Cyan)),
            Span::raw("Open file"),
        ]),
        Line::from(vec![
            Span::styled("  q / Esc      ", Style::default().fg(Color::Cyan)),
            Span::raw("Back to search"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Press any key to close", Style::default().fg(Color::DarkGray)),
        ]),
    ];

    let help_paragraph = Paragraph::new(help_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" Keybindings ")
                .title_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(Color::Black));

    // Clear the area behind the help panel
    f.render_widget(ratatui::widgets::Clear, help_area);
    f.render_widget(help_paragraph, help_area);
}

/// Highlight matches in text, returning owned spans
fn highlight_match(text: &str, start: usize, end: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();

    // Clamp positions to valid char boundaries for UTF-8 safety
    let start = text.floor_char_boundary(start.min(text.len()));
    let end = text.floor_char_boundary(end.min(text.len())).max(start);

    if start > 0 {
        spans.push(Span::styled(
            text[..start].to_string(),
            Style::default().fg(Color::White),
        ));
    }

    if end > start {
        spans.push(Span::styled(
            text[start..end].to_string(),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if end < text.len() {
        spans.push(Span::styled(
            text[end..].to_string(),
            Style::default().fg(Color::White),
        ));
    }

    spans
}
