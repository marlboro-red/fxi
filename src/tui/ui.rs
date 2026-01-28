use crate::tui::app::{App, Mode};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
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
    // Clear the area first to prevent artifacts
    f.render_widget(Clear, area);

    // Calculate inner dimensions (accounting for borders)
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;

    let mut items: Vec<ListItem> = app
        .results
        .iter()
        .enumerate()
        .map(|(idx, result)| {
            let is_selected = idx == app.selected;
            let selection_bg = if is_selected {
                Some(Color::DarkGray)
            } else {
                None
            };
            let path_style = Style::default().fg(Color::Blue);
            let line_style = Style::default().fg(Color::Yellow);

            // Format: path:line
            let path_str = truncate_path(&result.path.to_string_lossy());
            let line_num = result.line_number;

            let apply_bg = |style: Style| -> Style {
                if let Some(bg) = selection_bg {
                    style.bg(bg)
                } else {
                    style
                }
            };

            let display_str = format!("{}:{}", path_str, line_num);
            let display_len = display_str.len();

            let mut spans = vec![
                Span::styled(format!("{}:", path_str), apply_bg(path_style)),
                Span::styled(format!("{}", line_num), apply_bg(line_style)),
            ];

            // Pad the line to fill the full inner width so selection background extends to edge
            if display_len < inner_width {
                let padding = " ".repeat(inner_width - display_len);
                spans.push(Span::styled(padding, apply_bg(Style::default())));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    // Pad with empty items to fill the visible area
    // This ensures old content is fully overwritten when switching to fewer results
    while items.len() < inner_height {
        items.push(ListItem::new(Line::from("")));
    }

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Results ({}) ", app.results.len())),
    );

    // Use ListState to properly track selection and enable automatic scrolling
    let mut state = ListState::default();
    if !app.results.is_empty() {
        state.select(Some(app.selected));
    }

    f.render_stateful_widget(list, area, &mut state);
}

fn draw_preview(f: &mut Frame, app: &App, area: Rect) {
    // Clear the preview area first to prevent artifacts from previous content
    f.render_widget(Clear, area);

    // Calculate inner dimensions (accounting for borders)
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;
    // Line number gutter is 5 chars ("1234 "), content gets the rest
    let content_width = inner_width.saturating_sub(5);

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

        // Use cached highlighted content with viewport offset
        // (highlighted_lines, start_offset) where start_offset is 0-indexed line number
        let highlighted_data = app.get_highlighted();

        let mut lines: Vec<Line> = preview
            .lines()
            .enumerate()
            .skip(app.preview_scroll)
            .take(inner_height)
            .map(|(line_num, plain_line)| {
                let actual_line = line_num + 1;
                let is_match = actual_line == match_line;

                // Truncate line content to fit panel width (prevents horizontal overflow)
                let truncated_line = if plain_line.len() > content_width {
                    let truncate_at = plain_line.floor_char_boundary(content_width);
                    &plain_line[..truncate_at]
                } else {
                    plain_line
                };

                let line_num_style = Style::default().fg(Color::DarkGray);
                let mut spans = vec![
                    Span::styled(format!("{:4} ", actual_line), line_num_style),
                ];

                // Use highlighted spans if available, otherwise fall back to plain text
                // Account for viewport offset when looking up highlighted lines
                if let Some((highlighted, start_offset)) = highlighted_data {
                    // Convert absolute line number to relative index in highlighted buffer
                    let relative_idx = line_num.checked_sub(start_offset);
                    if let Some(idx) = relative_idx {
                        if let Some(line_spans) = highlighted.get(idx) {
                            // Calculate total content length for padding
                            let mut content_len = 0usize;
                            if is_match {
                                // For matched line, add bold modifier to all spans
                                for span in line_spans {
                                    // Truncate span content if needed
                                    let span_content = if content_len + span.content.len() > content_width {
                                        let remaining = content_width.saturating_sub(content_len);
                                        let truncate_at = span.content.floor_char_boundary(remaining);
                                        &span.content[..truncate_at]
                                    } else {
                                        &span.content
                                    };
                                    content_len += span_content.len();

                                    let mut style = span.style;
                                    style = style.add_modifier(Modifier::BOLD);
                                    // Also add a subtle background to indicate match
                                    style = style.bg(Color::Rgb(60, 60, 40));
                                    spans.push(Span::styled(span_content.to_string(), style));

                                    if content_len >= content_width {
                                        break;
                                    }
                                }
                                // Pad match line to extend background to edge
                                if content_len < content_width {
                                    let padding = " ".repeat(content_width - content_len);
                                    spans.push(Span::styled(
                                        padding,
                                        Style::default()
                                            .bg(Color::Rgb(60, 60, 40))
                                            .add_modifier(Modifier::BOLD),
                                    ));
                                }
                            } else {
                                for span in line_spans {
                                    // Truncate span content if needed
                                    let span_content = if content_len + span.content.len() > content_width {
                                        let remaining = content_width.saturating_sub(content_len);
                                        let truncate_at = span.content.floor_char_boundary(remaining);
                                        &span.content[..truncate_at]
                                    } else {
                                        &span.content
                                    };
                                    content_len += span_content.len();
                                    spans.push(Span::styled(span_content.to_string(), span.style));

                                    if content_len >= content_width {
                                        break;
                                    }
                                }
                            }
                            return Line::from(spans);
                        }
                    }
                }

                // Fallback for lines outside highlighted range or no highlighting available
                if is_match {
                    let content_style = Style::default()
                        .fg(Color::Yellow)
                        .bg(Color::Rgb(60, 60, 40))
                        .add_modifier(Modifier::BOLD);
                    spans.push(Span::styled(truncated_line.to_string(), content_style));
                    // Pad match line to extend background to edge
                    if truncated_line.len() < content_width {
                        let padding = " ".repeat(content_width - truncated_line.len());
                        spans.push(Span::styled(padding, content_style));
                    }
                } else {
                    spans.push(Span::styled(truncated_line.to_string(), Style::default()));
                }
                Line::from(spans)
            })
            .collect();

        // Pad with empty lines to fill the entire visible area
        // This ensures old content is fully overwritten when switching to shorter files
        while lines.len() < inner_height {
            lines.push(Line::from(""));
        }

        Text::from(lines)
    } else {
        Text::raw("No preview available")
    };

    let preview = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title(title));

    f.render_widget(preview, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    // Get current time for animations
    let time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    // Show animated indicators for loading/searching
    let (status_text, style) = if app.is_loading() {
        // Loading index animation
        let spinner = match (time_ms / 100) % 4 {
            0 => "⠋",
            1 => "⠙",
            2 => "⠹",
            _ => "⠸",
        };
        let dots = match (time_ms / 300) % 4 {
            0 => "",
            1 => ".",
            2 => "..",
            _ => "...",
        };
        (
            format!("{} Loading index{}", spinner, dots),
            Style::default().fg(Color::Yellow).bg(Color::Reset),
        )
    } else if app.is_searching() {
        // Searching animation with elapsed time
        let spinner = match (time_ms / 80) % 8 {
            0 => "⣾",
            1 => "⣽",
            2 => "⣻",
            3 => "⢿",
            4 => "⡿",
            5 => "⣟",
            6 => "⣯",
            _ => "⣷",
        };
        let elapsed = app.search_duration_ms().unwrap_or(0);
        (
            format!("{} Searching... ({:.0}ms)", spinner, elapsed),
            Style::default().fg(Color::Magenta).bg(Color::Reset),
        )
    } else {
        (
            app.status_message.clone(),
            Style::default().fg(Color::Cyan).bg(Color::Reset),
        )
    };

    // Pad the status message to fill the entire width to prevent artifacts
    let padded_message = format!("{:<width$}", status_text, width = area.width as usize);

    let status = Paragraph::new(padded_message).style(style);
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

/// Truncate a path to show at most 3 directory components, prefixing with "..." if truncated
fn truncate_path(path_str: &str) -> String {
    // Split by both forward and back slashes to handle cross-platform paths
    let components: Vec<&str> = path_str.split(['/', '\\']).collect();

    if components.len() <= 4 {
        // 3 directories + filename = 4 components max without truncation
        path_str.to_string()
    } else {
        // Take last 4 components (3 dirs + filename) and prefix with ...
        let truncated: Vec<&str> = components.iter().rev().take(4).rev().cloned().collect();
        format!(".../{}", truncated.join("/"))
    }
}

