//! Interactive terminal user interface.
//!
//! This module provides a full-featured TUI for interactive code search:
//!
//! - Background searches submitted with Enter
//! - Vim-style keybindings (j/k, Ctrl+d/u, gg/G)
//! - Syntax-highlighted file preview
//! - Context lines around matches
//!
//! ## Architecture
//!
//! - [`app`] - Application state and search logic
//! - [`ui`] - Ratatui-based rendering
//!
//! ## Keybindings
//!
//! | Key | Action |
//! |-----|--------|
//! | `Enter` | Execute search |
//! | `j`/`Down` | Next result |
//! | `k`/`Up` | Previous result |
//! | `Ctrl+d` | Page down |
//! | `Ctrl+u` | Page up |
//! | `gg` | Go to first result |
//! | `G` | Go to last result |
//! | `Ctrl+p` | Toggle preview |
//! | `?` | Show help |
//! | `Esc` | Clear query / exit |

mod app;
mod ui;

use anyhow::Result;
use app::App;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// Restore the terminal on every return path, including failed app setup.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            crossterm::cursor::Show
        );
    }
}

pub fn run(path: PathBuf, initial_query: Option<String>) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let _terminal_guard = TerminalGuard;
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
        app.set_initial_query(&query);
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

fn run_app<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    <B as ratatui::backend::Backend>::Error: Send + Sync + 'static,
{
    loop {
        // Check for background index load completion (non-blocking)
        app.poll_index_load();

        // Check for background search completion (non-blocking)
        app.poll_search();

        terminal.draw(|f| ui::draw(f, app))?;

        // Use shorter timeout when searching for faster UI updates (animated indicator)
        // Otherwise use 16ms for 60fps responsiveness
        let timeout = if app.is_searching() || app.is_loading() {
            Duration::from_millis(16) // Fast polling during active operations
        } else {
            Duration::from_millis(50) // Normal: balance responsiveness vs CPU
        };

        // Poll for events with timeout for responsive UI
        if event::poll(timeout)? {
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
                        // Handle pending 'g' key for gg command (only when not editing)
                        if !app.editing && app.pending_key == Some('g') {
                            app.clear_pending_key();
                            if key.code == KeyCode::Char('g') {
                                app.select_first();
                                continue;
                            }
                            // If not 'g', fall through to normal handling
                        }

                        // Check for Ctrl+key combinations first
                        match (key.modifiers, key.code) {
                            // Vim: Ctrl+j/Ctrl+n - select next result (only when not editing)
                            (KeyModifiers::CONTROL, KeyCode::Char('j'))
                            | (KeyModifiers::CONTROL, KeyCode::Char('n'))
                                if !app.editing =>
                            {
                                app.select_next()
                            }
                            // Vim: Ctrl+k - select previous result (only when not editing)
                            (KeyModifiers::CONTROL, KeyCode::Char('k')) if !app.editing => {
                                app.select_prev()
                            }
                            // Vim: Ctrl+d - page down (only when not editing)
                            (KeyModifiers::CONTROL, KeyCode::Char('d')) if !app.editing => {
                                app.select_page_down()
                            }
                            // Vim: Ctrl+u - page up (only when not editing)
                            (KeyModifiers::CONTROL, KeyCode::Char('u')) if !app.editing => {
                                app.select_page_up()
                            }
                            // Vim: Ctrl+w - delete word backward (always available for editing)
                            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                                app.delete_word();
                                app.editing = true;
                            }
                            // Vim: Ctrl+h - backspace (always available for editing)
                            (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                                app.query.pop();
                                app.editing = true;
                            }
                            // Vim: Ctrl+a - go to first result (only when not editing)
                            (KeyModifiers::CONTROL, KeyCode::Char('a')) if !app.editing => {
                                app.select_first()
                            }
                            // Vim: Ctrl+e - go to last result (only when not editing)
                            (KeyModifiers::CONTROL, KeyCode::Char('e')) if !app.editing => {
                                app.select_last()
                            }
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
                                    app.editing = false;
                                }
                                KeyCode::Down | KeyCode::Tab => app.select_next(),
                                KeyCode::Up | KeyCode::BackTab => app.select_prev(),
                                KeyCode::PageDown => app.select_page_down(),
                                KeyCode::PageUp => app.select_page_up(),
                                KeyCode::Char('g') if !app.editing => {
                                    // Start 'gg' sequence for vim-style go to top
                                    app.pending_key = Some('g');
                                }
                                KeyCode::Char('G') if !app.editing => {
                                    // Vim: G - go to last result
                                    app.select_last();
                                }
                                KeyCode::Char('?') if !app.editing => {
                                    // Show help panel
                                    app.show_help();
                                }
                                KeyCode::Char(c) => {
                                    app.query.push(c);
                                    app.editing = true;
                                }
                                KeyCode::Backspace => {
                                    app.query.pop();
                                    app.editing = true;
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};

    #[test]
    fn terminal_guard_restores_on_startup_error() {
        const HELPER: &str = "FXI_TEST_TUI_TERMINAL_GUARD";
        if std::env::var_os(HELPER).is_some() {
            assert!(unsafe { libc::setsid() } >= 0);
            assert_eq!(unsafe { libc::ioctl(0, libc::TIOCSCTTY as _, 0) }, 0);
            let mut before = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(unsafe { libc::tcgetattr(0, &mut before) }, 0);
            let directory = tempfile::tempdir().unwrap();
            assert!(run(directory.path().join("does-not-exist"), None).is_err());
            let mut after = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(unsafe { libc::tcgetattr(0, &mut after) }, 0);
            let raw_flags = libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN;
            assert_eq!(before.c_lflag & raw_flags, after.c_lflag & raw_flags);
            assert_eq!(before.c_iflag, after.c_iflag);
            assert_eq!(before.c_oflag, after.c_oflag);
            return;
        }
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::addr_of_mut!(size),
                )
            },
            0
        );
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tui::tests::terminal_guard_restores_on_startup_error",
                "--nocapture",
            ])
            .env(HELPER, "1")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            // A session leader can wait for PTY output to drain during exit.
            let mut output = [0u8; 4096];
            while master.read(&mut output).is_ok_and(|read| read > 0) {}
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                drop(master);
                let _ = child.wait();
                panic!("TUI terminal restoration helper timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());
    }
}
