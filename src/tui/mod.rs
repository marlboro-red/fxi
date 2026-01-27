mod app;
mod ui;

use anyhow::Result;
use app::App;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
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

    // Create app state
    let mut app = App::new(path)?;

    if let Some(query) = initial_query {
        app.set_query(&query);
        app.execute_search();
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
        terminal.draw(|f| ui::draw(f, app))?;

        // Poll for events with timeout for responsive UI
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                // Global keybindings
                match (key.modifiers, key.code) {
                    (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Ok(()),
                    (KeyModifiers::CONTROL, KeyCode::Char('q')) => return Ok(()),
                    _ => {}
                }

                match app.mode {
                    app::Mode::Search => match key.code {
                        KeyCode::Esc => {
                            if app.query.is_empty() {
                                return Ok(());
                            }
                            app.clear_query();
                        }
                        KeyCode::Enter => {
                            if !app.results.is_empty() {
                                app.open_selected();
                            }
                        }
                        KeyCode::Down | KeyCode::Tab => app.select_next(),
                        KeyCode::Up | KeyCode::BackTab => app.select_prev(),
                        KeyCode::PageDown => app.select_page_down(),
                        KeyCode::PageUp => app.select_page_up(),
                        KeyCode::Char(c) => {
                            app.query.push(c);
                            app.execute_search();
                        }
                        KeyCode::Backspace => {
                            app.query.pop();
                            app.execute_search();
                        }
                        KeyCode::F(5) => app.reindex(),
                        _ => {}
                    },
                    app::Mode::Preview => match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => app.mode = app::Mode::Search,
                        KeyCode::Down | KeyCode::Char('j') => app.scroll_preview_down(),
                        KeyCode::Up | KeyCode::Char('k') => app.scroll_preview_up(),
                        KeyCode::PageDown => app.scroll_preview_page_down(),
                        KeyCode::PageUp => app.scroll_preview_page_up(),
                        KeyCode::Enter | KeyCode::Char('o') => app.open_selected(),
                        _ => {}
                    },
                }

                // Toggle preview mode
                if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    app.toggle_preview();
                }
            }
        }
    }
}
