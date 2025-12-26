// Copyright 2025 Tomoki Hayashi
// MIT License (https://opensource.org/licenses/MIT)

//! Application state and orchestration.
//!
//! `App` owns:
//! - the current selection (`current_index`)
//! - render requests and an LRU-like render cache
//! - the worker thread (decode/resize/encode)
//! - the terminal writer thread (the only stdout writer)
//!
//! Most methods are intentionally non-blocking; heavy work is pushed to the worker/writer.

use std::path::PathBuf;

use anyhow::Result;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui_image::picker::Picker;

use crate::fit::FitMode;
use crate::kgp::KgpState;
use crate::sender::{StatusIndicator, TerminalWriter, WriterRequest, WriterResultKind};
use crate::worker::{ImageRequest, ImageWorker};

pub struct RenderedImage {
    pub path: PathBuf,
    pub target: (u32, u32),
    pub fit_mode: FitMode,
    pub actual_size: (u32, u32),
    pub encoded_chunks: Vec<Vec<u8>>,
    pub transmitted: bool,
    pub last_transmit_seq: u64,
}

fn render_cache_limit() -> usize {
    const DEFAULT: usize = 15;
    const MAX: usize = 200;

    std::env::var("SIVIT_RENDER_CACHE_SIZE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT)
        .clamp(1, MAX)
}

pub struct App {
    pub images: Vec<PathBuf>,
    pub current_index: usize,
    pub picker: Picker,
    pub should_quit: bool,
    pub fit_mode: FitMode,
    pub kgp_state: KgpState,
    worker: ImageWorker,
    writer: TerminalWriter,
    pending_request: Option<(PathBuf, (u32, u32), FitMode)>,
    render_cache: Vec<RenderedImage>,
    render_cache_limit: usize,
    kgp_id: u32,
    in_flight_transmit: bool,
    pending_display: Option<Rect>,
    render_epoch: u64,
    transmit_seq: u64,
    force_retransmit: bool,
    clear_after_nav: bool,
}

pub fn is_tmux_env() -> bool {
    std::env::var_os("TMUX").is_some()
}

fn ensure_tmux_allow_passthrough_on() {
    use std::process::Command;

    if is_tmux_env() {
        let _ = Command::new("tmux")
            .args(["set-option", "-gq", "allow-passthrough", "on"])
            .output();
    }
}

impl App {
    /// Create a new application instance.
    pub fn new(images: Vec<PathBuf>) -> Result<Self> {
        ensure_tmux_allow_passthrough_on();

        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)));
        let render_cache_limit = render_cache_limit();
        let kgp_id = Self::generate_kgp_id();
        let app = App {
            images,
            current_index: 0,
            picker,
            should_quit: false,
            fit_mode: FitMode::Normal,
            kgp_state: KgpState::default(),
            worker: ImageWorker::new(),
            writer: TerminalWriter::new(),
            pending_request: None,
            render_cache: Vec::with_capacity(render_cache_limit),
            render_cache_limit,
            kgp_id,
            in_flight_transmit: false,
            pending_display: None,
            render_epoch: 0,
            transmit_seq: 0,
            force_retransmit: false,
            clear_after_nav: false,
        };

        // Clear any stale terminal-side image cache at startup.
        app.writer.send(WriterRequest::ClearAll {
            area: None,
            is_tmux: is_tmux_env(),
        });

        Ok(app)
    }

    /// Generate a single KGP ID for this process (yazi-style).
    /// Using a fixed ID ensures terminal-side cache is always overwritten,
    /// preventing "wrong image" issues from stale data.
    fn generate_kgp_id() -> u32 {
        const MIN_COMPONENT: u32 = 16;
        const MUL: u32 = 0x9E3779B1;

        // Start from process ID to get some variation between instances
        let base = std::process::id();
        let mut idx = base;

        loop {
            let id = idx.wrapping_mul(MUL).rotate_left(8);
            let r = (id >> 16) & 0xff;
            let g = (id >> 8) & 0xff;
            let b = id & 0xff;
            if r >= MIN_COMPONENT && g >= MIN_COMPONENT && b >= MIN_COMPONENT {
                return id;
            }
            idx = idx.wrapping_add(1);
        }
    }

    pub fn move_by(&mut self, delta: i32) {
        if delta == 0 || self.images.is_empty() {
            return;
        }
        let len = self.images.len() as i32;
        self.current_index = (self.current_index as i32 + delta).rem_euclid(len) as usize;
        self.invalidate_render();
    }

    /// Toggle between `Normal` (shrink-only) and `Fit` (allow upscale).
    pub fn toggle_fit_mode(&mut self) {
        self.fit_mode = self.fit_mode.next();
        self.invalidate_render();
    }

    /// Clear caches/state and force re-decode/re-send on the next tick.
    pub fn reload(&mut self) {
        self.cancel_image_output();
        self.render_cache.clear();
        self.pending_request = None;
        self.kgp_state = KgpState::default();
    }

    fn go_to_index(&mut self, index: usize) {
        if self.images.is_empty() {
            return;
        }
        let index = index.min(self.images.len().saturating_sub(1));
        if self.current_index == index {
            return;
        }
        self.current_index = index;
        self.invalidate_render();
    }

    pub fn go_first(&mut self) {
        self.go_to_index(0);
    }

    pub fn go_last(&mut self) {
        if self.images.is_empty() {
            return;
        }
        self.go_to_index(self.images.len().saturating_sub(1));
    }

    pub fn go_to_1based(&mut self, n: usize) {
        if self.images.is_empty() {
            return;
        }
        let idx = n.saturating_sub(1);
        self.go_to_index(idx);
    }

    fn invalidate_render(&mut self) {
        self.pending_request = None;
        // Note: Do NOT clear in_flight_transmit here.
        // cancel_image_output() needs it to invalidate the correct cache entry.
    }

    fn current_path(&self) -> Option<&PathBuf> {
        self.images.get(self.current_index)
    }

    fn should_retransmit(&self, rendered: &RenderedImage) -> bool {
        self.should_retransmit_with_seq(rendered.transmitted, rendered.last_transmit_seq)
    }

    fn should_retransmit_with_seq(&self, transmitted: bool, last_transmit_seq: u64) -> bool {
        if !transmitted {
            return true;
        }
        let limit = self.render_cache_limit.max(1) as u64;
        self.transmit_seq.saturating_sub(last_transmit_seq) >= limit
    }

    pub fn poll_worker(&mut self) {
        while let Some(result) = self.worker.try_recv() {
            if self.pending_request.as_ref().is_some_and(|(p, t, f)| {
                p == &result.path && *t == result.target && *f == result.fit_mode
            }) {
                self.pending_request = None;
            }
            // Add to cache (LRU: remove existing entry for same path+target, add to end)
            self.render_cache.retain(|r| {
                !(r.path == result.path
                    && r.target == result.target
                    && r.fit_mode == result.fit_mode)
            });
            if self.render_cache.len() >= self.render_cache_limit {
                self.render_cache.remove(0);
            }
            self.render_cache.push(RenderedImage {
                path: result.path,
                target: result.target,
                fit_mode: result.fit_mode,
                actual_size: result.actual_size,
                encoded_chunks: result.encoded_chunks,
                transmitted: false,
                last_transmit_seq: 0,
            });
        }
    }

    pub fn poll_writer(&mut self) {
        while let Some(result) = self.writer.try_recv() {
            if result.epoch != self.render_epoch {
                continue;
            }
            let transmitted = matches!(result.kind, WriterResultKind::TransmitDone { .. });

            if transmitted {
                self.transmit_seq = self.transmit_seq.saturating_add(1);
                let seq = self.transmit_seq;
                // Mark current image as transmitted in cache
                if let Some(path) = self.current_path().cloned() {
                    for rendered in &mut self.render_cache {
                        if rendered.path == path {
                            rendered.transmitted = true;
                            rendered.last_transmit_seq = seq;
                            break;
                        }
                    }
                }
                self.in_flight_transmit = false;
            }

            if let Some(area) = self.pending_display.take() {
                self.kgp_state.set_last(area, self.kgp_id);
            }
        }
    }

    /// Determine whether the current image is fully displayed (`Ready`) or still in progress (`Busy`).
    pub fn status_indicator(
        &self,
        terminal_size: Rect,
        allow_transmission: bool,
    ) -> StatusIndicator {
        if !allow_transmission {
            return StatusIndicator::Busy;
        }
        if self.pending_display.is_some() {
            return StatusIndicator::Busy;
        }
        if self.in_flight_transmit {
            return StatusIndicator::Busy;
        }
        if self.force_retransmit {
            return StatusIndicator::Busy;
        }

        let Some(path) = self.current_path() else {
            return StatusIndicator::Busy;
        };

        let full = Rect::new(0, 0, terminal_size.width, terminal_size.height);
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(full);
        let image_area = chunks[0];

        let (cell_w, cell_h) = self.picker.font_size();
        if cell_w == 0 || cell_h == 0 || image_area.width == 0 || image_area.height == 0 {
            return StatusIndicator::Busy;
        }

        let max_w_px = u32::from(image_area.width) * u32::from(cell_w);
        let max_h_px = u32::from(image_area.height) * u32::from(cell_h);
        let target = (max_w_px, max_h_px);

        let Some(rendered) = self
            .render_cache
            .iter()
            .find(|r| &r.path == path && r.target == target && r.fit_mode == self.fit_mode)
        else {
            return StatusIndicator::Busy;
        };

        if self.should_retransmit(rendered) {
            return StatusIndicator::Busy;
        }

        // Compute expected placement area and require it to match last successful display.
        let cells_w = rendered.actual_size.0.div_ceil(u32::from(cell_w));
        let cells_h = rendered.actual_size.1.div_ceil(u32::from(cell_h));
        let cells_w = cells_w.min(u32::from(image_area.width)) as u16;
        let cells_h = cells_h.min(u32::from(image_area.height)) as u16;
        let offset_x = (image_area.width.saturating_sub(cells_w)) / 2;
        let offset_y = (image_area.height.saturating_sub(cells_h)) / 2;
        let area = Rect::new(
            image_area.x + offset_x,
            image_area.y + offset_y,
            cells_w,
            cells_h,
        );

        if self.kgp_state.last_area() != Some(area)
            || self.kgp_state.last_kgp_id() != Some(self.kgp_id)
        {
            return StatusIndicator::Busy;
        }

        StatusIndicator::Ready
    }

    /// Send the status row to the writer thread.
    pub fn send_status(&self, text: String, size: (u16, u16), indicator: StatusIndicator) {
        self.writer.send(WriterRequest::Status {
            text,
            size,
            indicator,
        });
    }

    /// Check if a transmit is currently in progress.
    pub fn is_transmitting(&self) -> bool {
        self.in_flight_transmit
    }

    /// Cancel any in-flight image output (best-effort).
    pub fn cancel_image_output(&mut self) {
        self.render_epoch = self.render_epoch.saturating_add(1);
        // Get area before clearing pending_display.
        // This area might have partial placement data that needs to be erased.
        let cancel_area = self.pending_display;

        self.writer.send(WriterRequest::CancelImage {
            kgp_id: Some(self.kgp_id),
            is_tmux: is_tmux_env(),
            area: cancel_area,
            epoch: self.render_epoch,
        });
        self.force_retransmit = true;
        self.clear_after_nav = true;

        // Invalidate current image's transmitted status if we were transmitting
        if self.in_flight_transmit {
            if let Some(path) = self.current_path().cloned() {
                for rendered in &mut self.render_cache {
                    if rendered.path == path {
                        rendered.transmitted = false;
                        break;
                    }
                }
            }
            self.in_flight_transmit = false;
        }

        self.pending_display = None;
        self.kgp_state.invalidate();
    }

    /// Request rendering / placement for the current image.
    ///
    /// When `allow_transmission` is false (navigation latch), this method does nothing to keep UX snappy.
    pub fn prepare_render_request(&mut self, terminal_size: Rect, allow_transmission: bool) {
        let Some(path) = self.current_path().cloned() else {
            return;
        };

        // Navigation/scrolling: do not do any image work (decode/resize/transmit/place).
        // This keeps status bar updates responsive by avoiding both stdout contention and CPU load.
        if !allow_transmission {
            return;
        }

        let is_tmux = is_tmux_env();
        let old_area = self.kgp_state.last_area();

        let full = Rect::new(0, 0, terminal_size.width, terminal_size.height);
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(full);
        let image_area = chunks[0];

        let (cell_w, cell_h) = self.picker.font_size();
        if cell_w == 0 || cell_h == 0 || image_area.width == 0 || image_area.height == 0 {
            return;
        }

        let max_w_px = u32::from(image_area.width) * u32::from(cell_w);
        let max_h_px = u32::from(image_area.height) * u32::from(cell_h);
        let target = (max_w_px, max_h_px);

        // Check if we have a cached rendered result
        let cached_idx = self
            .render_cache
            .iter()
            .position(|r| r.path == path && r.target == target && r.fit_mode == self.fit_mode);
        if let Some(idx) = cached_idx {
            let (actual_size, encoded_chunks, transmitted, last_transmit_seq) = {
                let rendered = &self.render_cache[idx];
                (
                    rendered.actual_size,
                    rendered.encoded_chunks.clone(),
                    rendered.transmitted,
                    rendered.last_transmit_seq,
                )
            };
            let force_retransmit = self.force_retransmit;
            let retransmit =
                self.should_retransmit_with_seq(transmitted, last_transmit_seq) || force_retransmit;

            // Calculate area for placement based on actual image size
            let cells_w = actual_size.0.div_ceil(u32::from(cell_w));
            let cells_h = actual_size.1.div_ceil(u32::from(cell_h));
            let cells_w = cells_w.min(u32::from(image_area.width)) as u16;
            let cells_h = cells_h.min(u32::from(image_area.height)) as u16;
            let offset_x = (image_area.width.saturating_sub(cells_w)) / 2;
            let offset_y = (image_area.height.saturating_sub(cells_h)) / 2;
            let area = Rect::new(
                image_area.x + offset_x,
                image_area.y + offset_y,
                cells_w,
                cells_h,
            );

            // Skip if already displayed and a refresh is not needed.
            if self.kgp_state.last_area() == Some(area)
                && self.kgp_state.last_kgp_id() == Some(self.kgp_id)
                && !retransmit
            {
                return;
            }
            if self.pending_display == Some(area) {
                return;
            }

            // Avoid re-starting a transmit every loop while the current one is still in-flight.
            if self.in_flight_transmit {
                return;
            }
            self.in_flight_transmit = true;
            self.force_retransmit = false;
            if self.clear_after_nav {
                self.writer.send(WriterRequest::ClearAll {
                    area: None,
                    is_tmux,
                });
                self.clear_after_nav = false;
            }

            self.writer.send(WriterRequest::ImageTransmit {
                encoded_chunks,
                area,
                kgp_id: self.kgp_id,
                old_area,
                epoch: self.render_epoch,
                is_tmux,
            });
            self.pending_display = Some(area);
            return;
        }

        // Request from worker if not already pending
        if self.pending_request.as_ref() != Some(&(path.clone(), target, self.fit_mode)) {
            self.worker.request(ImageRequest {
                path: path.clone(),
                target,
                fit_mode: self.fit_mode,
                kgp_id: self.kgp_id,
                is_tmux,
            });
            self.pending_request = Some((path, target, self.fit_mode));
        }
    }

    pub fn clear_kgp_overlay(&mut self) {
        let Some(area) = self.kgp_state.last_area() else {
            return;
        };

        let is_tmux = is_tmux_env();
        self.writer.send(WriterRequest::ClearAll {
            area: Some(area),
            is_tmux,
        });
    }

    pub fn current_image_name(&self) -> String {
        self.images
            .get(self.current_index)
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    }

    pub fn status_text(&self) -> String {
        let mut status = format!(
            "[{}/{}] {}",
            self.current_index + 1,
            self.images.len(),
            self.current_image_name(),
        );

        if self.fit_mode == FitMode::Fit {
            status.push_str(" fit");
        }

        if std::env::var_os("SIVIT_DEBUG").is_some() {
            if is_tmux_env() {
                status.push_str(" tmux");
            }
            status.push_str(&format!(
                " caps:{:?} cell:{:?}",
                self.picker.capabilities(),
                self.picker.font_size(),
            ));
        }

        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_app(image_count: usize) -> App {
        let images: Vec<PathBuf> = (0..image_count)
            .map(|i| PathBuf::from(format!("test{}.png", i)))
            .collect();
        App {
            images,
            current_index: 0,
            picker: Picker::from_fontsize((8, 16)),
            should_quit: false,
            fit_mode: FitMode::Normal,
            kgp_state: KgpState::default(),
            worker: ImageWorker::new(),
            writer: TerminalWriter::new(),
            pending_request: None,
            render_cache: Vec::new(),
            render_cache_limit: 5,
            kgp_id: App::generate_kgp_id(),
            in_flight_transmit: false,
            pending_display: None,
            render_epoch: 0,
            transmit_seq: 0,
            force_retransmit: false,
            clear_after_nav: false,
        }
    }

    #[test]
    fn test_move_by_positive() {
        let mut app = create_test_app(3);
        assert_eq!(app.current_index, 0);
        app.move_by(1);
        assert_eq!(app.current_index, 1);
    }

    #[test]
    fn test_move_by_wraps_forward() {
        let mut app = create_test_app(3);
        app.current_index = 2;
        app.move_by(1);
        assert_eq!(app.current_index, 0);
    }

    #[test]
    fn test_move_by_negative() {
        let mut app = create_test_app(3);
        app.current_index = 1;
        app.move_by(-1);
        assert_eq!(app.current_index, 0);
    }

    #[test]
    fn test_move_by_wraps_backward() {
        let mut app = create_test_app(3);
        app.current_index = 0;
        app.move_by(-1);
        assert_eq!(app.current_index, 2);
    }

    #[test]
    fn test_status_text() {
        let app = create_test_app(3);
        assert!(app.status_text().starts_with("[1/3] test0.png"));
    }

    #[test]
    fn test_go_first_and_last() {
        let mut app = create_test_app(3);
        app.current_index = 1;
        app.go_first();
        assert_eq!(app.current_index, 0);
        app.go_last();
        assert_eq!(app.current_index, 2);
    }

    #[test]
    fn test_go_to_1based_clamps() {
        let mut app = create_test_app(3);
        app.go_to_1based(2);
        assert_eq!(app.current_index, 1);
        app.go_to_1based(999);
        assert_eq!(app.current_index, 2);
    }

    #[test]
    fn test_toggle_fit_mode_cycles() {
        let mut app = create_test_app(1);
        assert_eq!(app.fit_mode, FitMode::Normal);
        app.toggle_fit_mode();
        assert_eq!(app.fit_mode, FitMode::Fit);
        app.toggle_fit_mode();
        assert_eq!(app.fit_mode, FitMode::Normal);
    }

    #[test]
    fn test_reload_clears_cache() {
        let mut app = create_test_app(2);
        app.render_cache.push(RenderedImage {
            path: PathBuf::from("x.png"),
            target: (1, 1),
            fit_mode: FitMode::Normal,
            actual_size: (1, 1),
            encoded_chunks: vec![b"x".to_vec()],
            transmitted: true,
            last_transmit_seq: 1,
        });
        app.pending_request = Some((PathBuf::from("y.png"), (1, 1), FitMode::Normal));
        app.in_flight_transmit = true;

        app.reload();
        assert!(app.render_cache.is_empty());
        assert!(app.pending_request.is_none());
        assert!(!app.in_flight_transmit);
    }
}
