mod app;
mod highlighter;
mod ui;

use anyhow::Result;
use app::App;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

pub fn run(path: PathBuf, initial_query: Option<String>) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Clear the terminal to prevent any artifacts from previous content
    terminal.clear()?;

    // Create app state (instant - index loads in background)
    let mut app = App::new(path)?;

    // Set initial query if provided (search will execute when index is ready)
    if let Some(query) = initial_query {
        app.set_query(&query);
        // Don't execute search here - index may not be loaded yet
        // The query will be auto-executed when index load completes
    }

    // Main loop
    let result = run_app(&mut terminal, &mut app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

fn run_app<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    loop {
        // Check for background index load completion (non-blocking)
        app.poll_index_load();

        terminal.draw(|f| ui::draw(f, app))?;

        // Poll for events with timeout for responsive UI
        if event::poll(Duration::from_millis(100))? {
            // Only handle key press events, not release or repeat
            // This fixes duplicate keypresses on Windows where both press and release are reported
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Global keybindings
                match (key.modifiers, key.code) {
                    (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Ok(()),
                    (KeyModifiers::CONTROL, KeyCode::Char('q')) => return Ok(()),
                    _ => {}
                }

                match app.mode {
                    app::Mode::Help => {
                        // In help mode, any key closes help
                        match (key.modifiers, key.code) {
                            (_, KeyCode::Esc)
                            | (_, KeyCode::Char('q'))
                            | (KeyModifiers::SHIFT, KeyCode::Char('?')) => {
                                app.hide_help();
                            }
                            _ => {
                                // Any other key also closes help
                                app.hide_help();
                            }
                        }
                    }
                    app::Mode::Search => {
                        // Handle pending 'g' key for gg command
                        if app.pending_key == Some('g') {
                            app.clear_pending_key();
                            if key.code == KeyCode::Char('g') {
                                app.select_first();
                                continue;
                            }
                            // If not 'g', fall through to normal handling
                        }

                        // Check for Ctrl+key combinations first
                        match (key.modifiers, key.code) {
                            // Vim: Ctrl+j/Ctrl+n - select next result
                            (KeyModifiers::CONTROL, KeyCode::Char('j'))
                            | (KeyModifiers::CONTROL, KeyCode::Char('n')) => app.select_next(),
                            // Vim: Ctrl+k - select previous result (Ctrl+p reserved for toggle preview)
                            (KeyModifiers::CONTROL, KeyCode::Char('k')) => app.select_prev(),
                            // Vim: Ctrl+d - page down
                            (KeyModifiers::CONTROL, KeyCode::Char('d')) => app.select_page_down(),
                            // Vim: Ctrl+u - page up
                            (KeyModifiers::CONTROL, KeyCode::Char('u')) => app.select_page_up(),
                            // Vim: Ctrl+w - delete word backward
                            (KeyModifiers::CONTROL, KeyCode::Char('w')) => app.delete_word(),
                            // Vim: Ctrl+h - backspace (terminal standard)
                            (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                                app.query.pop();
                            }
                            // Vim: Ctrl+a - go to first result
                            (KeyModifiers::CONTROL, KeyCode::Char('a')) => app.select_first(),
                            // Vim: Ctrl+e - go to last result
                            (KeyModifiers::CONTROL, KeyCode::Char('e')) => app.select_last(),
                            // Toggle preview mode
                            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                                app.toggle_preview();
                            }
                            // Non-Ctrl keybindings
                            (KeyModifiers::NONE | KeyModifiers::SHIFT, code) => match code {
                                KeyCode::Esc => {
                                    if app.query.is_empty() {
                                        return Ok(());
                                    }
                                    app.clear_query();
                                }
                                KeyCode::Enter => {
                                    app.execute_search();
                                }
                                KeyCode::Down | KeyCode::Tab => app.select_next(),
                                KeyCode::Up | KeyCode::BackTab => app.select_prev(),
                                KeyCode::PageDown => app.select_page_down(),
                                KeyCode::PageUp => app.select_page_up(),
                                KeyCode::Char('g') => {
                                    // Start 'gg' sequence for vim-style go to top
                                    app.pending_key = Some('g');
                                }
                                KeyCode::Char('G') => {
                                    // Vim: G - go to last result
                                    app.select_last();
                                }
                                KeyCode::Char('?') => {
                                    // Show help panel
                                    app.show_help();
                                }
                                KeyCode::Char(c) => {
                                    app.query.push(c);
                                }
                                KeyCode::Backspace => {
                                    app.query.pop();
                                }
                                KeyCode::F(1) => app.show_help(),
                                KeyCode::F(5) => app.reindex(),
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                    app::Mode::Preview => {
                        // Handle pending 'g' key for gg command
                        if app.pending_key == Some('g') {
                            app.clear_pending_key();
                            if key.code == KeyCode::Char('g') {
                                app.scroll_preview_to_top();
                                continue;
                            }
                            // If not 'g', fall through to normal handling
                        }

                        // Check for Ctrl+key combinations first
                        match (key.modifiers, key.code) {
                            // Vim: Ctrl+d - half-page down
                            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                                app.scroll_preview_half_page_down()
                            }
                            // Vim: Ctrl+u - half-page up
                            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                                app.scroll_preview_half_page_up()
                            }
                            // Vim: Ctrl+f - full page down
                            (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
                                app.scroll_preview_page_down()
                            }
                            // Vim: Ctrl+b - full page up
                            (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
                                app.scroll_preview_page_up()
                            }
                            // Toggle preview mode
                            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                                app.toggle_preview();
                            }
                            // Non-Ctrl keybindings
                            (KeyModifiers::NONE | KeyModifiers::SHIFT, code) => match code {
                                KeyCode::Esc | KeyCode::Char('q') => app.mode = app::Mode::Search,
                                KeyCode::Down | KeyCode::Char('j') => app.scroll_preview_down(),
                                KeyCode::Up | KeyCode::Char('k') => app.scroll_preview_up(),
                                KeyCode::PageDown => app.scroll_preview_page_down(),
                                KeyCode::PageUp => app.scroll_preview_page_up(),
                                KeyCode::Enter | KeyCode::Char('o') => app.open_selected(),
                                // Vim: g - start 'gg' sequence for go to top
                                KeyCode::Char('g') => {
                                    app.pending_key = Some('g');
                                }
                                // Vim: G - go to bottom
                                KeyCode::Char('G') => {
                                    app.scroll_preview_to_bottom();
                                }
                                // Vim: n - next result (from preview)
                                KeyCode::Char('n') => {
                                    app.select_next();
                                }
                                // Vim: N/p - previous result (from preview)
                                KeyCode::Char('N') | KeyCode::Char('p') => {
                                    app.select_prev();
                                }
                                // Show help panel
                                KeyCode::Char('?') | KeyCode::F(1) => {
                                    app.show_help();
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}
