use crate::tui::app::{App, Mode};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

pub fn draw(f: &mut Frame, app: &App) {
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
}

fn draw_query_input(f: &mut Frame, app: &App, area: Rect) {
    let input = Paragraph::new(app.query.as_str())
        .style(Style::default().fg(Color::Yellow))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Search (F5: reindex, Ctrl+P: preview, Esc: quit) "),
        );

    f.render_widget(input, area);

    // Show cursor
    if app.mode == Mode::Search {
        f.set_cursor_position((area.x + app.query.len() as u16 + 1, area.y + 1));
    }
}

fn draw_main_area(f: &mut Frame, app: &App, area: Rect) {
    match app.mode {
        Mode::Search => {
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
            let content_style = Style::default().fg(Color::White);

            // Format: path:line  content
            let path_str = result.path.to_string_lossy();
            let line_num = result.line_number;

            // Truncate content if needed
            let max_content_len = area.width.saturating_sub(path_str.len() as u16 + 10) as usize;
            let content = if result.line_content.len() > max_content_len {
                format!("{}...", &result.line_content[..max_content_len.saturating_sub(3)])
            } else {
                result.line_content.clone()
            };

            let line = Line::from(vec![
                Span::styled(format!("{}:", path_str), path_style),
                Span::styled(format!("{}", line_num), line_style),
                Span::raw("  "),
                Span::styled(content.trim().to_string(), content_style),
            ]);

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
        let lines: Vec<Line> = preview
            .lines()
            .enumerate()
            .skip(app.preview_scroll)
            .take(area.height.saturating_sub(2) as usize)
            .map(|(line_num, line)| {
                let actual_line = line_num + 1;

                // Check if this is the matched line
                let is_match = app
                    .get_selected_result()
                    .map(|r| r.line_number as usize == actual_line)
                    .unwrap_or(false);

                let line_num_style = Style::default().fg(Color::DarkGray);
                let content_style = if is_match {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                Line::from(vec![
                    Span::styled(format!("{:4} ", actual_line), line_num_style),
                    Span::styled(line, content_style),
                ])
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
    let status = Paragraph::new(app.status_message.as_str())
        .style(Style::default().fg(Color::Cyan));

    f.render_widget(status, area);
}

/// Highlight matches in text
fn highlight_match(text: &str, start: usize, end: usize) -> Vec<Span<'_>> {
    let mut spans = Vec::new();

    if start > 0 {
        spans.push(Span::raw(&text[..start]));
    }

    spans.push(Span::styled(
        &text[start..end],
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));

    if end < text.len() {
        spans.push(Span::raw(&text[end..]));
    }

    spans
}
