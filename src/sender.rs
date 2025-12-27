// Copyright 2025 Tomoki Hayashi
// MIT License (https://opensource.org/licenses/MIT)

//! Terminal output writer.
//!
//! This module is the only place allowed to write to stdout. It serializes output and prevents
//! escape-sequence interleaving across threads.
//!
//! Key properties:
//! - Status updates are prioritized and flushed immediately.
//! - Image output is chunked at safe boundaries (KGP chunks and per-row placement/erase).
//! - Image output can be cancelled on navigation.

use std::collections::VecDeque;
use std::io::{IsTerminal, Write, stdout};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use ratatui::layout::Rect;

use crate::kgp::{delete_all, delete_by_id, erase_rows, place_rows};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusIndicator {
    Busy,
    Ready,
    Fit,
    Tile,
}

pub enum WriterRequest {
    /// Update the status row (single-line HUD at the bottom).
    Status {
        text: String,
        size: (u16, u16),
        indicator: StatusIndicator,
    },
    /// Transmit image bytes (KGP) and place the image in the terminal area.
    ImageTransmit {
        encoded_chunks: Vec<Vec<u8>>,
        area: Rect,
        kgp_id: u32,
        old_area: Option<Rect>,
        epoch: u64,
        is_tmux: bool,
    },
    /// Clear any KGP overlays (used on shutdown).
    ClearAll {
        area: Option<Rect>,
        is_tmux: bool,
    },
    /// Cancel an in-flight image task (best-effort).
    CancelImage {
        area: Option<Rect>,
        epoch: u64,
    },
    /// Copy data to clipboard via OSC 52.
    CopyToClipboard {
        data: Vec<u8>,
        is_tmux: bool,
    },
    /// Draw tile cursor border (ANSI overlay).
    TileCursor {
        grid: (usize, usize),
        cursor_idx: usize,
        image_area: Rect,
        prev_cursor_idx: Option<usize>,
        cell_size: (u16, u16),
    },
    Shutdown,
}

pub struct WriterResult {
    pub kind: WriterResultKind,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterResultKind {
    TransmitDone { kgp_id: u32 },
}

struct Task {
    chunks: VecDeque<Vec<u8>>,
    complete: Option<WriterResultKind>,
    epoch: u64,
    clears_dirty: bool,
}

struct WriterState {
    should_quit: bool,
    last_status: Option<(String, (u16, u16), StatusIndicator)>,
    status_dirty: bool,
    current_task: Option<Task>,
    current_epoch: u64,
    dirty_area: Option<Rect>,
}

pub struct TerminalWriter {
    request_tx: Sender<WriterRequest>,
    result_rx: Receiver<WriterResult>,
    handle: Option<JoinHandle<()>>,
}

impl TerminalWriter {
    /// Spawn the writer thread.
    pub fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<WriterRequest>();
        let (result_tx, result_rx) = mpsc::channel::<WriterResult>();

        let handle = thread::spawn(move || {
            Self::writer_loop(request_rx, result_tx);
        });

        Self {
            request_tx,
            result_rx,
            handle: Some(handle),
        }
    }

    /// Send a request to the writer thread.
    pub fn send(&self, req: WriterRequest) {
        let _ = self.request_tx.send(req);
    }

    /// Poll for completion notifications (e.g. transmit finished for a `kgp_id`).
    pub fn try_recv(&self) -> Option<WriterResult> {
        self.result_rx.try_recv().ok()
    }

    fn writer_loop(request_rx: Receiver<WriterRequest>, result_tx: Sender<WriterResult>) {
        let mut out = stdout();
        let is_tty = out.is_terminal();

        let mut state = WriterState {
            should_quit: false,
            last_status: None,
            status_dirty: false,
            current_task: None,
            current_epoch: 0,
            dirty_area: None,
        };
        let mut bytes_since_flush: usize = 0;
        const FLUSH_THRESHOLD: usize = 64 * 1024;

        loop {
            if state.should_quit {
                break;
            }

            if state.current_task.is_none() && !state.status_dirty {
                match request_rx.recv() {
                    Ok(msg) => Self::apply_msg(msg, &mut state, is_tty, &mut out),
                    Err(_) => break,
                }
            }

            while let Ok(msg) = request_rx.try_recv() {
                Self::apply_msg(msg, &mut state, is_tty, &mut out);
                if state.should_quit {
                    break;
                }
            }

            if state.status_dirty {
                if let Some((text, size, indicator)) = state.last_status.clone() {
                    if is_tty {
                        let _ = Self::render_status(&mut out, &text, size, indicator);
                        let _ = out.flush();
                    }
                    bytes_since_flush = 0;
                }
                state.status_dirty = false;
            }

            if let Some(task) = &mut state.current_task {
                if task.epoch != state.current_epoch {
                    state.current_task = None;
                    continue;
                }
                if !is_tty {
                    if let Some(kind) = task.complete {
                        let _ = result_tx.send(WriterResult {
                            kind,
                            epoch: task.epoch,
                        });
                    }
                    if task.clears_dirty {
                        state.dirty_area = None;
                    }
                    state.current_task = None;
                    continue;
                }
                if let Some(chunk) = task.chunks.pop_front() {
                    if !chunk.is_empty() {
                        let _ = out.write_all(&chunk);
                        bytes_since_flush = bytes_since_flush.saturating_add(chunk.len());
                        if bytes_since_flush >= FLUSH_THRESHOLD {
                            let _ = out.flush();
                            bytes_since_flush = 0;
                        }
                    }
                } else {
                    let _ = out.flush();
                    bytes_since_flush = 0;
                    if let Some(kind) = task.complete {
                        let _ = result_tx.send(WriterResult {
                            kind,
                            epoch: task.epoch,
                        });
                    }
                    if task.clears_dirty {
                        state.dirty_area = None;
                    }
                    state.current_task = None;
                }
            }
        }
    }

    fn apply_msg(msg: WriterRequest, state: &mut WriterState, is_tty: bool, out: &mut impl Write) {
        match msg {
            WriterRequest::Shutdown => {
                state.should_quit = true;
            }
            WriterRequest::Status {
                text,
                size,
                indicator,
            } => {
                state.last_status = Some((text, size, indicator));
                state.status_dirty = true;
            }
            WriterRequest::ClearAll { area, is_tmux } => {
                // Preempt current image work.
                state.current_task = None;
                state.dirty_area = None;
                if is_tty {
                    let _ = Self::clear_all(out, area, is_tmux);
                    let _ = out.flush();
                }
            }
            WriterRequest::CancelImage { area, epoch } => {
                if epoch >= state.current_epoch {
                    state.current_epoch = epoch;
                    state.current_task = None;
                }
                if let Some(cancel_area) = area {
                    let next = match state.dirty_area.take() {
                        Some(prev) => union_rect(prev, cancel_area),
                        None => cancel_area,
                    };
                    state.dirty_area = Some(next);
                }
                // Cancellation is best-effort: just stop writing further chunks.
                // task_transmit always erases old_area to clean up stale placements.
                if is_tty {
                    let _ = out.write_all(b"\x1b[0m");
                    let _ = out.flush();
                }
            }
            WriterRequest::ImageTransmit {
                encoded_chunks,
                area,
                kgp_id,
                old_area,
                epoch,
                is_tmux,
            } => {
                if epoch < state.current_epoch {
                    return;
                }
                state.current_epoch = epoch;
                let cleanup_area = state.dirty_area;
                state.current_task = Some(Self::task_transmit(
                    encoded_chunks,
                    area,
                    kgp_id,
                    old_area,
                    cleanup_area,
                    epoch,
                    is_tmux,
                ));
            }
            WriterRequest::CopyToClipboard { data, is_tmux } => {
                if is_tty {
                    let osc52 = build_osc52_clipboard(&data, is_tmux);
                    let _ = out.write_all(&osc52);
                    let _ = out.flush();
                }
            }
            WriterRequest::TileCursor {
                grid,
                cursor_idx,
                image_area,
                prev_cursor_idx,
                cell_size,
            } => {
                if is_tty {
                    // Clear previous cursor if different
                    if let Some(prev_idx) = prev_cursor_idx
                        && prev_idx != cursor_idx
                    {
                        let _ = out.write_all(&Self::build_tile_cursor_escape(
                            grid, prev_idx, image_area, cell_size, false, // clear
                        ));
                    }
                    // Draw new cursor
                    let _ = out.write_all(&Self::build_tile_cursor_escape(
                        grid, cursor_idx, image_area, cell_size, true, // draw
                    ));
                    let _ = out.flush();
                }
            }
        }
    }

    fn task_transmit(
        encoded_chunks: Vec<Vec<u8>>,
        area: Rect,
        kgp_id: u32,
        old_area: Option<Rect>,
        dirty_area: Option<Rect>,
        epoch: u64,
        is_tmux: bool,
    ) -> Task {
        let mut chunks = VecDeque::new();

        // Step 1: Erase old area FIRST (yazi pattern: hide -> show)
        // This ensures that even if cancelled mid-execution, the old image is already erased.
        if let Some(old) = old_area {
            for row in erase_rows(old) {
                chunks.push_back(row);
            }
        }
        for cleanup in Self::cleanup_rects(area, dirty_area) {
            for row in erase_rows(cleanup) {
                chunks.push_back(row);
            }
        }

        // Step 2: Delete existing image data for this ID
        // This prevents stale data from being displayed if transmit is cancelled
        chunks.push_back(delete_by_id(kgp_id, is_tmux));

        // Step 3: Transmit new image data
        for enc in encoded_chunks {
            chunks.push_back(enc);
        }

        // Step 4: Place new image
        for row in place_rows(area, kgp_id) {
            chunks.push_back(row);
        }

        Task {
            chunks,
            complete: Some(WriterResultKind::TransmitDone { kgp_id }),
            epoch,
            clears_dirty: dirty_area.is_some(),
        }
    }

    fn cleanup_rects(area: Rect, dirty_area: Option<Rect>) -> Vec<Rect> {
        match dirty_area {
            Some(dirty) => rect_diff(dirty, area),
            None => Vec::new(),
        }
    }

    fn clear_all(out: &mut impl Write, area: Option<Rect>, is_tmux: bool) -> std::io::Result<()> {
        if let Some(area) = area {
            for row in erase_rows(area) {
                out.write_all(&row)?;
            }
        }
        out.write_all(&delete_all(is_tmux))?;
        out.write_all(b"\x1b[0m")?;
        Ok(())
    }

    fn render_status(
        out: &mut impl Write,
        status_text: &str,
        size: (u16, u16),
        indicator: StatusIndicator,
    ) -> std::io::Result<()> {
        let (w, h) = size;
        if w == 0 || h == 0 {
            return Ok(());
        }

        // Nerdfont icons and Powerline separator
        const ICON_READY: &str = "\u{f012c}"; //  (nf-md-check)
        const ICON_BUSY: &str = "\u{f110}"; //  (nf-fa-spinner)
        const ICON_FIT: &str = "\u{f004c}"; //  (nf-md-arrow_expand_all)
        const ICON_TILE: &str = "\u{f11d9}"; //  (nf-md-view_grid_outline)
        const SEP: &str = "\u{e0b0}"; //  (Powerline separator)

        // ANSI 16-color (uses terminal theme colors)
        // Foreground: 30=Black, 37=White, 90-97=Bright
        // Background: 40=Black, 47=White, 100-107=Bright
        const FG_DARK: u8 = 30; // Black
        const FG_LIGHT: u8 = 97; // Bright White
        const BG_MAIN: u8 = 40; // Black
        const BG_READY: u8 = 42; // Green
        const BG_BUSY: u8 = 43; // Yellow
        const BG_FIT: u8 = 45; // Magenta
        const BG_TILE: u8 = 46; // Cyan

        let row_1based = h;
        // Reserve 4 columns for icon segment " X  " (icon + spaces + separator)
        let available = w.saturating_sub(4);
        let clipped = clip_utf8(status_text, available as usize);

        let (icon, fg_indicator, bg_indicator) = match indicator {
            StatusIndicator::Ready => (ICON_READY, BG_READY - 10, BG_READY), // fg=32 (Green)
            StatusIndicator::Busy => (ICON_BUSY, BG_BUSY - 10, BG_BUSY),     // fg=33 (Yellow)
            StatusIndicator::Fit => (ICON_FIT, BG_FIT - 10, BG_FIT),         // fg=35 (Magenta)
            StatusIndicator::Tile => (ICON_TILE, BG_TILE - 10, BG_TILE),     // fg=36 (Cyan)
        };

        // Clear line with main background
        write!(out, "\x1b[{row_1based};1H\x1b[{BG_MAIN}m\x1b[{w}X")?;

        // Left segment: indicator icon with colored background
        write!(
            out,
            "\x1b[{row_1based};1H\x1b[{FG_DARK};{bg_indicator}m {icon} "
        )?;

        // Powerline separator: indicator color -> main background
        write!(out, "\x1b[{fg_indicator};{BG_MAIN}m{SEP}")?;

        // Main content with light text on dark background
        write!(out, "\x1b[{FG_LIGHT};{BG_MAIN}m {clipped}\x1b[0m")?;

        Ok(())
    }

    /// Build ANSI escape sequence to draw or clear tile cursor border.
    fn build_tile_cursor_escape(
        grid: (usize, usize),
        cursor_idx: usize,
        image_area: Rect,
        cell_size: (u16, u16),
        draw: bool,
    ) -> Vec<u8> {
        use std::fmt::Write;

        let (cols, rows) = grid;
        if cols == 0 || rows == 0 || cursor_idx >= cols * rows {
            return Vec::new();
        }

        let (cell_w, cell_h) = cell_size;
        if cell_w == 0 || cell_h == 0 {
            return Vec::new();
        }

        // Use cell-aligned tile boundaries (matching worker.rs)
        // This ensures cursor position matches the actual tile positions in the image
        let canvas_w_cells = u32::from(image_area.width);
        let canvas_h_cells = u32::from(image_area.height);

        let col = cursor_idx % cols;
        let row = cursor_idx / cols;

        // Calculate tile boundaries in cells (same formula as worker.rs)
        let tile_x_cells = (col as u32 * canvas_w_cells) / cols as u32;
        let tile_y_cells = (row as u32 * canvas_h_cells) / rows as u32;
        let next_tile_x_cells = ((col + 1) as u32 * canvas_w_cells) / cols as u32;
        let next_tile_y_cells = ((row + 1) as u32 * canvas_h_cells) / rows as u32;

        let tile_x = image_area.x + tile_x_cells as u16;
        let tile_y = image_area.y + tile_y_cells as u16;
        let tile_x_end = image_area.x + next_tile_x_cells as u16;
        let tile_y_end = image_area.y + next_tile_y_cells as u16;

        // Unicode box drawing characters (rounded corners)
        const TOP_LEFT: char = '╭';
        const TOP_RIGHT: char = '╮';
        const BOTTOM_LEFT: char = '╰';
        const BOTTOM_RIGHT: char = '╯';
        const HORIZONTAL: char = '─';
        const VERTICAL: char = '│';

        // Pre-allocate buffer (estimate: ~20 bytes per cell)
        let estimated_size = ((tile_x_end - tile_x) + (tile_y_end - tile_y)) as usize * 20;
        let mut s = String::with_capacity(estimated_size);

        if draw {
            s.push_str("\x1b[36m"); // Cyan color
        } else {
            s.push_str("\x1b[0m"); // Reset color
        }

        let char_h = if draw { HORIZONTAL } else { ' ' };
        let char_v = if draw { VERTICAL } else { ' ' };
        let char_tl = if draw { TOP_LEFT } else { ' ' };
        let char_tr = if draw { TOP_RIGHT } else { ' ' };
        let char_bl = if draw { BOTTOM_LEFT } else { ' ' };
        let char_br = if draw { BOTTOM_RIGHT } else { ' ' };

        // Top edge: move to position, draw corner + horizontal line + corner
        let top_row = tile_y + 1; // 1-based
        let left_col = tile_x + 1; // 1-based
        let right_col = tile_x_end; // 1-based

        // Draw top edge
        let _ = write!(s, "\x1b[{};{}H{}", top_row, left_col, char_tl);
        for c in (left_col + 1)..right_col {
            let _ = write!(s, "\x1b[{};{}H{}", top_row, c, char_h);
        }
        let _ = write!(s, "\x1b[{};{}H{}", top_row, right_col, char_tr);

        // Bottom edge
        let bottom_row = tile_y_end;
        let _ = write!(s, "\x1b[{};{}H{}", bottom_row, left_col, char_bl);
        for c in (left_col + 1)..right_col {
            let _ = write!(s, "\x1b[{};{}H{}", bottom_row, c, char_h);
        }
        let _ = write!(s, "\x1b[{};{}H{}", bottom_row, right_col, char_br);

        // Left and right edges (vertical lines)
        for r in (top_row + 1)..bottom_row {
            let _ = write!(s, "\x1b[{};{}H{}", r, left_col, char_v);
            let _ = write!(s, "\x1b[{};{}H{}", r, right_col, char_v);
        }

        s.push_str("\x1b[0m"); // Reset attributes

        s.into_bytes()
    }
}

impl Drop for TerminalWriter {
    fn drop(&mut self) {
        let _ = self.request_tx.send(WriterRequest::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn clip_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = 0;
    for (i, _) in s.char_indices() {
        if i > max_bytes {
            break;
        }
        end = i;
    }
    &s[..end]
}

fn rect_diff(old: Rect, new: Rect) -> Vec<Rect> {
    let mut out = Vec::new();
    let Some(inter) = rect_intersection(old, new) else {
        out.push(old);
        return out;
    };

    let old_x0 = u32::from(old.x);
    let old_y0 = u32::from(old.y);
    let old_x1 = old_x0 + u32::from(old.width);
    let old_y1 = old_y0 + u32::from(old.height);
    let inter_x0 = u32::from(inter.x);
    let inter_y0 = u32::from(inter.y);
    let inter_x1 = inter_x0 + u32::from(inter.width);
    let inter_y1 = inter_y0 + u32::from(inter.height);

    if old_y0 < inter_y0 {
        out.push(Rect::new(
            old.x,
            old.y,
            old.width,
            (inter_y0 - old_y0) as u16,
        ));
    }
    if inter_y1 < old_y1 {
        out.push(Rect::new(
            old.x,
            inter_y1 as u16,
            old.width,
            (old_y1 - inter_y1) as u16,
        ));
    }
    if old_x0 < inter_x0 {
        out.push(Rect::new(
            old.x,
            inter.y,
            (inter_x0 - old_x0) as u16,
            inter.height,
        ));
    }
    if inter_x1 < old_x1 {
        out.push(Rect::new(
            inter_x1 as u16,
            inter.y,
            (old_x1 - inter_x1) as u16,
            inter.height,
        ));
    }

    out
}

fn rect_intersection(a: Rect, b: Rect) -> Option<Rect> {
    let ax0 = u32::from(a.x);
    let ay0 = u32::from(a.y);
    let ax1 = ax0 + u32::from(a.width);
    let ay1 = ay0 + u32::from(a.height);
    let bx0 = u32::from(b.x);
    let by0 = u32::from(b.y);
    let bx1 = bx0 + u32::from(b.width);
    let by1 = by0 + u32::from(b.height);

    let x0 = ax0.max(bx0);
    let y0 = ay0.max(by0);
    let x1 = ax1.min(bx1);
    let y1 = ay1.min(by1);

    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    Some(Rect::new(
        x0 as u16,
        y0 as u16,
        (x1 - x0) as u16,
        (y1 - y0) as u16,
    ))
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let ax0 = u32::from(a.x);
    let ay0 = u32::from(a.y);
    let ax1 = ax0 + u32::from(a.width);
    let ay1 = ay0 + u32::from(a.height);
    let bx0 = u32::from(b.x);
    let by0 = u32::from(b.y);
    let bx1 = bx0 + u32::from(b.width);
    let by1 = by0 + u32::from(b.height);

    let x0 = ax0.min(bx0);
    let y0 = ay0.min(by0);
    let x1 = ax1.max(bx1);
    let y1 = ay1.max(by1);

    Rect::new(x0 as u16, y0 as u16, (x1 - x0) as u16, (y1 - y0) as u16)
}

/// Build OSC 52 escape sequence for clipboard copy.
fn build_osc52_clipboard(data: &[u8], is_tmux: bool) -> Vec<u8> {
    let b64 = base64_simd::STANDARD.encode_to_string(data);

    if is_tmux {
        format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\").into_bytes()
    } else {
        format!("\x1b]52;c;{b64}\x07").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rect_intersection_no_overlap() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(20, 20, 10, 10);
        assert!(rect_intersection(a, b).is_none());
    }

    #[test]
    fn test_rect_intersection_partial() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        let inter = rect_intersection(a, b).unwrap();
        assert_eq!(inter, Rect::new(5, 5, 5, 5));
    }

    #[test]
    fn test_rect_intersection_contained() {
        let a = Rect::new(0, 0, 20, 20);
        let b = Rect::new(5, 5, 5, 5);
        let inter = rect_intersection(a, b).unwrap();
        assert_eq!(inter, Rect::new(5, 5, 5, 5));
    }

    #[test]
    fn test_rect_intersection_same() {
        let a = Rect::new(5, 5, 10, 10);
        let inter = rect_intersection(a, a).unwrap();
        assert_eq!(inter, a);
    }

    #[test]
    fn test_rect_intersection_edge_touch() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(10, 0, 10, 10);
        // Edge touch means no overlap (width/height would be 0)
        assert!(rect_intersection(a, b).is_none());
    }

    #[test]
    fn test_rect_diff_no_overlap() {
        let old = Rect::new(0, 0, 10, 10);
        let new = Rect::new(20, 20, 10, 10);
        let diff = rect_diff(old, new);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0], old);
    }

    #[test]
    fn test_rect_diff_same_rect() {
        let r = Rect::new(5, 5, 10, 10);
        let diff = rect_diff(r, r);
        assert!(diff.is_empty());
    }

    #[test]
    fn test_rect_diff_partial_overlap() {
        // old covers (0,0)-(10,10), new covers (5,5)-(15,15)
        let old = Rect::new(0, 0, 10, 10);
        let new = Rect::new(5, 5, 10, 10);
        let diff = rect_diff(old, new);
        // Should produce strips: top, left
        assert!(!diff.is_empty());
        // Total area of diff should equal old area - intersection area
        let diff_area: u32 = diff
            .iter()
            .map(|r| u32::from(r.width) * u32::from(r.height))
            .sum();
        let old_area = 10 * 10;
        let inter_area = 5 * 5;
        assert_eq!(diff_area, old_area - inter_area);
    }

    #[test]
    fn test_rect_diff_new_inside_old() {
        let old = Rect::new(0, 0, 20, 20);
        let new = Rect::new(5, 5, 10, 10);
        let diff = rect_diff(old, new);
        // Should produce 4 strips around the new rect
        assert_eq!(diff.len(), 4);
    }

    #[test]
    fn test_union_rect_disjoint() {
        let a = Rect::new(0, 0, 5, 5);
        let b = Rect::new(10, 10, 5, 5);
        let u = union_rect(a, b);
        assert_eq!(u, Rect::new(0, 0, 15, 15));
    }

    #[test]
    fn test_union_rect_overlapping() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        let u = union_rect(a, b);
        assert_eq!(u, Rect::new(0, 0, 15, 15));
    }

    #[test]
    fn test_union_rect_same() {
        let r = Rect::new(5, 5, 10, 10);
        let u = union_rect(r, r);
        assert_eq!(u, r);
    }

    #[test]
    fn test_union_rect_contained() {
        let outer = Rect::new(0, 0, 20, 20);
        let inner = Rect::new(5, 5, 5, 5);
        let u = union_rect(outer, inner);
        assert_eq!(u, outer);
    }

    #[test]
    fn test_clip_utf8_no_truncation() {
        let s = "hello";
        assert_eq!(clip_utf8(s, 10), "hello");
    }

    #[test]
    fn test_clip_utf8_exact_fit() {
        let s = "hello";
        assert_eq!(clip_utf8(s, 5), "hello");
    }

    #[test]
    fn test_clip_utf8_truncation() {
        let s = "hello world";
        assert_eq!(clip_utf8(s, 5), "hello");
    }

    #[test]
    fn test_clip_utf8_multibyte() {
        let s = "日本語テスト";
        // Each Japanese character is 3 bytes
        // 6 bytes = 2 chars
        let clipped = clip_utf8(s, 6);
        assert_eq!(clipped, "日本");
    }

    #[test]
    fn test_clip_utf8_multibyte_boundary() {
        let s = "日本語";
        // 7 bytes: can fit 2 chars (6 bytes), not 3rd partial
        let clipped = clip_utf8(s, 7);
        assert_eq!(clipped, "日本");
    }

    #[test]
    fn test_build_osc52_clipboard() {
        let data = b"test";
        let result = build_osc52_clipboard(data, false);
        let s = String::from_utf8_lossy(&result);
        assert!(s.starts_with("\x1b]52;c;"));
        assert!(s.ends_with("\x07"));
        assert!(s.contains("dGVzdA==")); // base64 of "test"
    }

    #[test]
    fn test_build_osc52_clipboard_tmux() {
        let data = b"test";
        let result = build_osc52_clipboard(data, true);
        let s = String::from_utf8_lossy(&result);
        assert!(s.starts_with("\x1bPtmux;"));
        assert!(s.ends_with("\x1b\\"));
    }
}
