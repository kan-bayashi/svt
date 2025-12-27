// Copyright 2025 Tomoki Hayashi
// MIT License (https://opensource.org/licenses/MIT)

//! Application entry point and event loop.
//!
//! This module:
//! - parses CLI args (multiple file/dir paths)
//! - runs the main input loop (vim-like navigation + counts)
//! - decides when to request renders
//! - sends status updates to `TerminalWriter`
//!
//! Terminal output is centralized in `TerminalWriter` (see `src/sender.rs`).

mod app;
mod config;
mod fit;
mod kgp;
mod prefetch;
mod sender;
mod worker;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use clap::Parser;
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal,
};
use ratatui::layout::Rect;

use crate::app::App;
use crate::app::is_tmux_env;
use crate::config::Config;
use crate::fit::ViewMode;

#[derive(Parser, Debug)]
#[command(name = "svt", about = "Simple Viewer in Terminal")]
struct Cli {
    /// Image file(s) and/or directory path(s)
    #[arg(required = true)]
    paths: Vec<PathBuf>,
}

const SUPPORTED_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| SUPPORTED_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

fn collect_images_from_path(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        if is_image_file(path) {
            return Ok(vec![path.to_path_buf()]);
        } else {
            anyhow::bail!("Not a supported image file: {:?}", path);
        }
    }

    if path.is_dir() {
        let mut images: Vec<PathBuf> = std::fs::read_dir(path)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|p| p.is_file() && is_image_file(p))
            .collect();
        images.sort();
        if images.is_empty() {
            anyhow::bail!("No image files found in directory: {:?}", path);
        }
        return Ok(images);
    }

    anyhow::bail!("Path does not exist: {:?}", path);
}

fn collect_images(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        out.extend(collect_images_from_path(p)?);
    }
    // De-dupe while preserving order (e.g. overlapping directories/globs).
    let mut seen = std::collections::HashSet::<PathBuf>::with_capacity(out.len());
    out.retain(|p| seen.insert(p.clone()));
    if out.is_empty() {
        anyhow::bail!("No image files found");
    }
    Ok(out)
}

fn use_alt_screen(config: &Config) -> bool {
    config.force_alt_screen || (!config.no_alt_screen && !is_tmux_env())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let images = collect_images(&cli.paths)?;
    let config = Config::load();

    let use_alt = use_alt_screen(&config);
    init_terminal(use_alt)?;
    let result = run(images, config);
    restore_terminal(use_alt);

    result
}

/// Duration to show temporary status messages (e.g., "Copied to clipboard").
const TEMP_STATUS_DURATION: Duration = Duration::from_millis(1500);

fn run(images: Vec<PathBuf>, config: Config) -> Result<()> {
    use std::time::Instant;

    let nav_latch = Duration::from_millis(config.nav_latch_ms);
    let cell_aspect_ratio = config.cell_aspect_ratio;
    let mut app = App::new(images, config)?;
    let mut nav_until = Instant::now() - Duration::from_secs(1);
    let mut count: u32 = 0;
    let mut last_status = String::new();
    let mut last_size: (u16, u16) = (0, 0);
    let mut last_indicator = crate::sender::StatusIndicator::Busy;
    let mut temp_status_until: Option<Instant> = None;
    let mut was_transmitting = false;

    loop {
        // Get terminal size once per iteration
        let (term_w, term_h) = terminal::size()?;
        let terminal_rect = Rect::new(0, 0, term_w, term_h);

        // Poll worker for completed renders
        app.poll_worker();

        // Poll writer for completed renders
        let transmitting_before = app.is_transmitting();
        app.poll_writer();
        let transmitting_after = app.is_transmitting();

        // Draw tile cursor after image transmission completes
        if was_transmitting && !transmitting_after && app.view_mode == ViewMode::Tile {
            app.draw_tile_cursor(terminal_rect);
        }
        was_transmitting = transmitting_before || transmitting_after;

        // Process all pending events first (drain the queue)
        while event::poll(Duration::ZERO)? {
            let ev = event::read()?;

            // Handle resize events
            if let Event::Resize(new_w, new_h) = ev {
                // Clear entire screen (including old status bar position)
                clear_screen();
                // Force full redraw on resize
                app.handle_resize();
                last_size = (new_w, new_h);
                last_status.clear(); // Force status redraw
                continue;
            }

            if let Event::Key(key) = ev
                && key.kind == KeyEventKind::Press
            {
                let mut did_nav = false;

                if let KeyCode::Char(c) = key.code
                    && c.is_ascii_digit()
                {
                    // Vim-like count prefix: `1..9` start, `0` continues (not a command on its own).
                    if c != '0' || count != 0 {
                        count = count
                            .saturating_mul(10)
                            .saturating_add((c as u8 - b'0') as u32);
                        // Keep reading digits without triggering redraw per digit.
                        continue;
                    }
                }

                let n = count.max(1) as i32;
                let grid = App::calculate_tile_grid(terminal_rect, cell_aspect_ratio);

                match key.code {
                    KeyCode::Char('q') => app.should_quit = true,
                    KeyCode::Char('j') | KeyCode::Char(' ') => match app.view_mode {
                        ViewMode::Single => {
                            app.move_by(n);
                            did_nav = true;
                        }
                        ViewMode::Tile => {
                            let page_changed = app.move_tile_cursor_row(n, grid);
                            if page_changed {
                                did_nav = true;
                            } else {
                                app.draw_tile_cursor(terminal_rect);
                            }
                        }
                    },
                    KeyCode::Char('k') | KeyCode::Backspace => match app.view_mode {
                        ViewMode::Single => {
                            app.move_by(-n);
                            did_nav = true;
                        }
                        ViewMode::Tile => {
                            let page_changed = app.move_tile_cursor_row(-n, grid);
                            if page_changed {
                                did_nav = true;
                            } else {
                                app.draw_tile_cursor(terminal_rect);
                            }
                        }
                    },
                    KeyCode::Char('h') => match app.view_mode {
                        ViewMode::Single => {
                            app.move_by(-n);
                            did_nav = true;
                        }
                        ViewMode::Tile => {
                            let page_changed = app.move_tile_cursor(-n, grid);
                            if page_changed {
                                did_nav = true;
                            } else {
                                app.draw_tile_cursor(terminal_rect);
                            }
                        }
                    },
                    KeyCode::Char('l') => match app.view_mode {
                        ViewMode::Single => {
                            app.move_by(n);
                            did_nav = true;
                        }
                        ViewMode::Tile => {
                            let page_changed = app.move_tile_cursor(n, grid);
                            if page_changed {
                                did_nav = true;
                            } else {
                                app.draw_tile_cursor(terminal_rect);
                            }
                        }
                    },
                    // Shift+HJKL: page navigation in Tile mode, same as lowercase in Single mode
                    KeyCode::Char('H') => match app.view_mode {
                        ViewMode::Single => {
                            app.move_by(-n);
                            did_nav = true;
                        }
                        ViewMode::Tile => {
                            app.move_tile_page(-n, grid);
                            did_nav = true;
                        }
                    },
                    KeyCode::Char('J') => match app.view_mode {
                        ViewMode::Single => {
                            app.move_by(n);
                            did_nav = true;
                        }
                        ViewMode::Tile => {
                            app.move_tile_page(n, grid);
                            did_nav = true;
                        }
                    },
                    KeyCode::Char('K') => match app.view_mode {
                        ViewMode::Single => {
                            app.move_by(-n);
                            did_nav = true;
                        }
                        ViewMode::Tile => {
                            app.move_tile_page(-n, grid);
                            did_nav = true;
                        }
                    },
                    KeyCode::Char('L') => match app.view_mode {
                        ViewMode::Single => {
                            app.move_by(n);
                            did_nav = true;
                        }
                        ViewMode::Tile => {
                            app.move_tile_page(n, grid);
                            did_nav = true;
                        }
                    },
                    KeyCode::Enter => {
                        if app.view_mode == ViewMode::Tile {
                            app.select_tile();
                            did_nav = true;
                        }
                    }
                    KeyCode::Char('g') => {
                        // Vim-like: `g` (or `N g`) goes to first / Nth (1-based) image.
                        let target = if count > 0 {
                            (count as usize).saturating_sub(1)
                        } else {
                            0
                        };
                        app.go_to_index_with_tile(target);
                        did_nav = true;
                    }
                    KeyCode::Char('G') => {
                        // Vim-like: `G` (or `N G`) goes to last / Nth (1-based) image.
                        let target = if count > 0 {
                            (count as usize).saturating_sub(1)
                        } else {
                            app.images.len().saturating_sub(1)
                        };
                        app.go_to_index_with_tile(target);
                        did_nav = true;
                    }
                    KeyCode::Char('f') => {
                        app.toggle_fit_mode();
                        did_nav = true;
                    }
                    KeyCode::Char('r') => {
                        app.reload();
                        did_nav = true;
                    }
                    KeyCode::Char('t') => {
                        app.toggle_view_mode();
                        did_nav = true;
                    }
                    KeyCode::Char('y') => {
                        if app.copy_path_to_clipboard() {
                            app.send_status(
                                "Copied path to clipboard".to_string(),
                                (term_w, term_h),
                                crate::sender::StatusIndicator::Ready,
                            );
                        } else {
                            app.send_status(
                                "Failed to copy path".to_string(),
                                (term_w, term_h),
                                crate::sender::StatusIndicator::Busy,
                            );
                        }
                        temp_status_until = Some(Instant::now() + TEMP_STATUS_DURATION);
                    }
                    KeyCode::Char('Y') => {
                        if app.copy_image_to_clipboard() {
                            app.send_status(
                                "Copied image to clipboard".to_string(),
                                (term_w, term_h),
                                crate::sender::StatusIndicator::Ready,
                            );
                        } else {
                            app.send_status(
                                "Failed to copy image".to_string(),
                                (term_w, term_h),
                                crate::sender::StatusIndicator::Busy,
                            );
                        }
                        temp_status_until = Some(Instant::now() + TEMP_STATUS_DURATION);
                    }
                    _ => {}
                }

                if did_nav {
                    // Only cancel if not currently transmitting to avoid blank screens.
                    // Transmit must complete to ensure image data is in terminal.
                    if !app.is_transmitting() {
                        app.cancel_image_output();
                    }
                    nav_until = Instant::now() + nav_latch;
                    count = 0;
                    // Don't drain all pending repeats in one loop; update status incrementally.
                    break;
                }
                // Count was for a navigation key; reset if another key is pressed.
                if count != 0 {
                    count = 0;
                }
            }
        }

        if app.should_quit {
            app.clear_kgp_overlay();
            break;
        }

        let allow_transmission = Instant::now() >= nav_until;
        let is_navigating = !allow_transmission;

        // Clear temporary status after timeout.
        if temp_status_until.is_some_and(|t| Instant::now() >= t) {
            temp_status_until = None;
            last_status.clear(); // Force redraw with normal status.
        }

        // Update status bar only when it changes (or on resize).
        let status_now = app.status_text(terminal_rect);
        let indicator = app.status_indicator(terminal_rect, allow_transmission);
        let should_draw = status_now != last_status
            || (term_w, term_h) != last_size
            || indicator != last_indicator;
        if should_draw && temp_status_until.is_none() {
            app.send_status(status_now.clone(), (term_w, term_h), indicator);
            last_status = status_now;
            last_size = (term_w, term_h);
            last_indicator = indicator;
        }

        // Prepare image render request (non-blocking, sends to sender thread).
        // Transmits only after user stops navigating (debounce via nav_latch).
        app.prepare_render_request(terminal_rect, allow_transmission);

        // Prefetch adjacent images/pages after current image is fully displayed.
        if allow_transmission
            && matches!(
                indicator,
                crate::sender::StatusIndicator::Ready
                    | crate::sender::StatusIndicator::Fit
                    | crate::sender::StatusIndicator::Tile
            )
        {
            app.prefetch_adjacent(terminal_rect);
        }

        // Wait for next event or worker result.
        // While navigating, keep the loop tighter so the status bar feels immediate.
        let tick = if is_navigating {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(16)
        };
        let _ = event::poll(tick);
    }

    Ok(())
}

fn init_terminal(use_alt_screen: bool) -> std::io::Result<()> {
    use std::io::stdout;

    use ratatui::crossterm::{
        cursor::{Hide, MoveTo},
        execute,
        terminal::{Clear, ClearType, EnterAlternateScreen, enable_raw_mode},
    };

    enable_raw_mode()?;
    if use_alt_screen {
        execute!(stdout(), EnterAlternateScreen)?;
    }
    execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0), Hide)?;
    Ok(())
}

fn restore_terminal(use_alt_screen: bool) {
    use std::io::stdout;

    use ratatui::crossterm::{
        cursor::Show,
        execute,
        terminal::{LeaveAlternateScreen, disable_raw_mode},
    };

    let _ = disable_raw_mode();
    if use_alt_screen {
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
    let _ = execute!(stdout(), Show);
}

fn clear_screen() {
    use std::io::stdout;

    use ratatui::crossterm::{
        cursor::MoveTo,
        execute,
        terminal::{Clear, ClearType},
    };

    let _ = execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};

    #[test]
    fn test_cli_parses_file_path() {
        let cli = Cli::try_parse_from(["svt", "image.png"]).unwrap();
        assert_eq!(cli.paths, vec![PathBuf::from("image.png")]);
    }

    #[test]
    fn test_cli_parses_directory_path() {
        let cli = Cli::try_parse_from(["svt", "/home/user/photos"]).unwrap();
        assert_eq!(cli.paths, vec![PathBuf::from("/home/user/photos")]);
    }

    #[test]
    fn test_cli_requires_paths_argument() {
        let result = Cli::try_parse_from(["svt"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_is_image_file_png() {
        assert!(is_image_file(&PathBuf::from("test.png")));
        assert!(is_image_file(&PathBuf::from("test.PNG")));
    }

    #[test]
    fn test_is_image_file_jpg() {
        assert!(is_image_file(&PathBuf::from("test.jpg")));
        assert!(is_image_file(&PathBuf::from("test.jpeg")));
        assert!(is_image_file(&PathBuf::from("test.JPEG")));
    }

    #[test]
    fn test_is_image_file_other_formats() {
        assert!(is_image_file(&PathBuf::from("test.gif")));
        assert!(is_image_file(&PathBuf::from("test.webp")));
    }

    #[test]
    fn test_is_image_file_non_image() {
        assert!(!is_image_file(&PathBuf::from("test.txt")));
        assert!(!is_image_file(&PathBuf::from("test.pdf")));
        assert!(!is_image_file(&PathBuf::from("noextension")));
    }

    #[test]
    fn test_collect_images_single_file() {
        let dir = PathBuf::from("/tmp/svt_test_single");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("test.png");
        File::create(&file).unwrap();

        let images = collect_images_from_path(&file).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0], file);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_collect_images_directory() {
        let dir = PathBuf::from("/tmp/svt_test_dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        File::create(dir.join("a.png")).unwrap();
        File::create(dir.join("b.jpg")).unwrap();
        File::create(dir.join("c.txt")).unwrap();

        let images = collect_images(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(images.len(), 2);
        assert!(images.iter().any(|p| p.ends_with("a.png")));
        assert!(images.iter().any(|p| p.ends_with("b.jpg")));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_collect_images_non_image_file_error() {
        let dir = PathBuf::from("/tmp/svt_test_non_image");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("test.txt");
        File::create(&file).unwrap();

        let result = collect_images(&[file]);
        assert!(result.is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_collect_images_empty_dir_error() {
        let dir = PathBuf::from("/tmp/svt_test_empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let result = collect_images(std::slice::from_ref(&dir));
        assert!(result.is_err());

        fs::remove_dir_all(&dir).unwrap();
    }
}
