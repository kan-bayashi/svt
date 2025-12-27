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

use crate::config::Config;
use crate::fit::{FitMode, ViewMode};
use crate::kgp::KgpState;
use crate::sender::{StatusIndicator, TerminalWriter, WriterRequest, WriterResultKind};
use crate::worker::{ImageRequest, ImageWorker};

pub struct RenderedImage {
    pub path: PathBuf,
    pub target: (u32, u32),
    pub fit_mode: FitMode,
    pub original_size: (u32, u32),
    pub actual_size: (u32, u32),
    pub encoded_chunks: Vec<Vec<u8>>,
}

pub struct App {
    pub images: Vec<PathBuf>,
    pub current_index: usize,
    pub picker: Picker,
    pub should_quit: bool,
    pub fit_mode: FitMode,
    pub view_mode: ViewMode,
    pub tile_cursor: usize,
    prev_tile_cursor: Option<usize>,
    pub kgp_state: KgpState,
    config: Config,
    worker: ImageWorker,
    writer: TerminalWriter,
    pending_request: Option<(PathBuf, (u32, u32), FitMode)>,
    render_cache: Vec<RenderedImage>,
    render_cache_limit: usize,
    kgp_id: u32,
    in_flight_transmit: bool,
    pending_display: Option<Rect>,
    render_epoch: u64,
    clear_after_nav: bool,
    is_tmux: bool,
}

pub fn is_tmux_env() -> bool {
    std::env::var_os("TMUX").is_some()
}

fn ensure_tmux_allow_passthrough_on(is_tmux: bool) {
    use std::process::Command;

    if is_tmux {
        // Use -pq to set pane-local option quietly (doesn't affect other panes/sessions)
        let _ = Command::new("tmux")
            .args(["set-option", "-pq", "allow-passthrough", "on"])
            .output();
    }
}

impl App {
    /// Create a new application instance.
    pub fn new(images: Vec<PathBuf>, config: Config) -> Result<Self> {
        let is_tmux = is_tmux_env();
        ensure_tmux_allow_passthrough_on(is_tmux);

        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)));
        let render_cache_limit = config.render_cache_size;
        let kgp_id = Self::generate_kgp_id();
        let app = App {
            images,
            current_index: 0,
            picker,
            should_quit: false,
            fit_mode: FitMode::Normal,
            view_mode: ViewMode::default(),
            tile_cursor: 0,
            prev_tile_cursor: None,
            kgp_state: KgpState::default(),
            config,
            worker: ImageWorker::new(),
            writer: TerminalWriter::new(),
            pending_request: None,
            render_cache: Vec::with_capacity(render_cache_limit),
            render_cache_limit,
            kgp_id,
            in_flight_transmit: false,
            pending_display: None,
            render_epoch: 0,
            clear_after_nav: false,
            is_tmux,
        };

        // Clear any stale terminal-side image cache at startup.
        app.writer.send(WriterRequest::ClearAll {
            area: None,
            is_tmux,
        });

        Ok(app)
    }

    /// Generate a single KGP ID for this process (yazi-style).
    /// Using a fixed ID ensures terminal-side cache is always overwritten,
    /// preventing "wrong image" issues from stale data.
    fn generate_kgp_id() -> u32 {
        const MIN_COMPONENT: u32 = 16;
        const MUL: u32 = 0x9E3779B1;
        const MAX_ATTEMPTS: u32 = 10000;

        // Start from process ID to get some variation between instances
        let base = std::process::id();
        let mut idx = base;

        for _ in 0..MAX_ATTEMPTS {
            let id = idx.wrapping_mul(MUL).rotate_left(8);
            let r = (id >> 16) & 0xff;
            let g = (id >> 8) & 0xff;
            let b = id & 0xff;
            if r >= MIN_COMPONENT && g >= MIN_COMPONENT && b >= MIN_COMPONENT {
                return id;
            }
            idx = idx.wrapping_add(1);
        }

        // Fallback: use a known-good ID if we couldn't find one
        // This should never happen in practice, but provides safety
        0x10_10_10_10
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

    /// Toggle between `Single` and `Tile` view modes.
    pub fn toggle_view_mode(&mut self) {
        match self.view_mode {
            ViewMode::Single => {
                // Entering tile mode: set cursor to current image position in page
                self.view_mode = ViewMode::Tile;
                // tile_cursor is the absolute position in the image list
                self.tile_cursor = self.current_index;
            }
            ViewMode::Tile => {
                // Exiting tile mode: set current_index to cursor position
                self.current_index = self.tile_cursor;
                self.view_mode = ViewMode::Single;
            }
        }
        self.invalidate_render();
    }

    /// Move tile cursor by delta (wraps around).
    /// Returns true if page changed (requires re-render), false if only cursor moved.
    pub fn move_tile_cursor(&mut self, delta: i32, grid: (usize, usize)) -> bool {
        if self.images.is_empty() {
            return false;
        }
        let (cols, rows) = grid;
        let tiles_per_page = cols * rows;
        if tiles_per_page == 0 {
            return false;
        }

        let old_page = self.tile_cursor / tiles_per_page;
        self.prev_tile_cursor = Some(self.tile_cursor);

        let len = self.images.len() as i32;
        self.tile_cursor = (self.tile_cursor as i32 + delta).rem_euclid(len) as usize;

        let new_page = self.tile_cursor / tiles_per_page;
        let page_changed = old_page != new_page;

        if page_changed {
            self.invalidate_render();
        }
        page_changed
    }

    /// Move tile cursor to next/prev row.
    /// Returns true if page changed.
    pub fn move_tile_cursor_row(&mut self, delta: i32, grid: (usize, usize)) -> bool {
        let (cols, _) = grid;
        self.move_tile_cursor(delta * cols as i32, grid)
    }

    /// Move tile page (Shift+H/J/K/L).
    /// After page change, cursor moves to the first tile of the new page.
    pub fn move_tile_page(&mut self, delta: i32, grid: (usize, usize)) {
        let (cols, rows) = grid;
        let tiles_per_page = cols * rows;
        let len = self.images.len();
        if len == 0 || tiles_per_page == 0 {
            return;
        }

        let current_page = self.tile_cursor / tiles_per_page;
        let max_page = (len - 1) / tiles_per_page;
        let new_page = (current_page as i32 + delta).clamp(0, max_page as i32) as usize;

        if new_page == current_page {
            return;
        }

        self.prev_tile_cursor = Some(self.tile_cursor);
        self.tile_cursor = new_page * tiles_per_page;
        self.invalidate_render();
    }

    /// Draw tile cursor via ANSI overlay (fast, no image re-render).
    pub fn draw_tile_cursor(&self, terminal_size: Rect) {
        let grid = Self::calculate_tile_grid(terminal_size, self.config.cell_aspect_ratio);
        let image_area = Self::image_area(terminal_size);
        let (cols, rows) = grid;
        let tiles_per_page = cols * rows;
        if tiles_per_page == 0 {
            return;
        }
        let cursor_in_page = self.tile_cursor % tiles_per_page;
        let prev_cursor_in_page = self.prev_tile_cursor.map(|prev| prev % tiles_per_page);

        self.writer.send(WriterRequest::TileCursor {
            grid,
            cursor_idx: cursor_in_page,
            image_area,
            prev_cursor_idx: prev_cursor_in_page,
            cell_size: self.picker.font_size(),
        });
    }

    /// Select current tile and switch to Single mode.
    pub fn select_tile(&mut self) {
        self.current_index = self.tile_cursor;
        self.view_mode = ViewMode::Single;
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
        self.go_to_index(self.images.len().saturating_sub(1));
    }

    pub fn go_to_1based(&mut self, n: usize) {
        self.go_to_index(n.saturating_sub(1));
    }

    fn invalidate_render(&mut self) {
        self.pending_request = None;
        // Note: Do NOT clear in_flight_transmit here.
        // cancel_image_output() needs it to invalidate the correct cache entry.
    }

    fn current_path(&self) -> Option<&PathBuf> {
        self.images.get(self.current_index)
    }

    /// Compute image area from terminal size (excluding status bar).
    fn image_area(terminal_size: Rect) -> Rect {
        let full = Rect::new(0, 0, terminal_size.width, terminal_size.height);
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(full)[0]
    }

    /// Calculate optimal tile grid size based on terminal dimensions.
    /// Returns (cols, rows) for the tile grid.
    pub fn calculate_tile_grid(terminal_size: Rect, cell_aspect_ratio: f64) -> (usize, usize) {
        let image_area = Self::image_area(terminal_size);

        // For visually square tiles, we need to account for the cell aspect ratio.
        // cell_aspect_ratio = cell_height_pixels / cell_width_pixels (typically ~2.0)
        const MIN_TILE_WIDTH: u16 = 16;
        const MAX_COLS: usize = 6;
        const MAX_ROWS: usize = 6;

        // Calculate min tile height to get visually square tiles
        let min_tile_height = (MIN_TILE_WIDTH as f64 / cell_aspect_ratio).round() as u16;
        let min_tile_height = min_tile_height.max(4); // Minimum 4 cells tall

        let cols = (image_area.width / MIN_TILE_WIDTH) as usize;
        let rows = (image_area.height / min_tile_height) as usize;

        // Clamp to reasonable bounds
        let cols = cols.clamp(2, MAX_COLS);
        let rows = rows.clamp(2, MAX_ROWS);

        (cols, rows)
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
                original_size: result.original_size,
                actual_size: result.actual_size,
                encoded_chunks: result.encoded_chunks,
            });
        }
    }

    pub fn poll_writer(&mut self) {
        while let Some(result) = self.writer.try_recv() {
            if result.epoch != self.render_epoch {
                continue;
            }
            if matches!(result.kind, WriterResultKind::TransmitDone { .. }) {
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

        let image_area = Self::image_area(terminal_size);

        let (cell_w, cell_h) = self.picker.font_size();
        if cell_w == 0 || cell_h == 0 || image_area.width == 0 || image_area.height == 0 {
            return StatusIndicator::Busy;
        }

        let max_w_px = u32::from(image_area.width) * u32::from(cell_w);
        let max_h_px = u32::from(image_area.height) * u32::from(cell_h);
        let target = (max_w_px, max_h_px);

        // Get the cache key based on view mode
        let cache_path = match self.view_mode {
            ViewMode::Single => {
                let Some(path) = self.current_path() else {
                    return StatusIndicator::Busy;
                };
                path.clone()
            }
            ViewMode::Tile => {
                let grid = Self::calculate_tile_grid(terminal_size, self.config.cell_aspect_ratio);
                let tiles_per_page = grid.0 * grid.1;
                if tiles_per_page == 0 {
                    return StatusIndicator::Busy;
                }
                let page_start = (self.tile_cursor / tiles_per_page) * tiles_per_page;
                PathBuf::from(format!("__tile_page_{}", page_start))
            }
        };

        let Some(rendered) = self
            .render_cache
            .iter()
            .find(|r| r.path == cache_path && r.target == target && r.fit_mode == self.fit_mode)
        else {
            return StatusIndicator::Busy;
        };

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

        match self.view_mode {
            ViewMode::Single => {
                if self.fit_mode == FitMode::Fit {
                    StatusIndicator::Fit
                } else {
                    StatusIndicator::Ready
                }
            }
            ViewMode::Tile => StatusIndicator::Tile,
        }
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
            area: cancel_area,
            epoch: self.render_epoch,
        });
        self.clear_after_nav = true;
        self.in_flight_transmit = false;
        self.pending_display = None;
        self.kgp_state.invalidate();
    }

    /// Request rendering / placement for the current image.
    ///
    /// When `allow_transmission` is false (navigation latch), this method does nothing to keep UX snappy.
    pub fn prepare_render_request(&mut self, terminal_size: Rect, allow_transmission: bool) {
        // Navigation/scrolling: do not do any image work (decode/resize/transmit/place).
        // This keeps status bar updates responsive by avoiding both stdout contention and CPU load.
        if !allow_transmission {
            return;
        }

        match self.view_mode {
            ViewMode::Single => self.prepare_single_render(terminal_size),
            ViewMode::Tile => self.prepare_tile_render(terminal_size),
        }
    }

    fn prepare_single_render(&mut self, terminal_size: Rect) {
        let Some(path) = self.current_path().cloned() else {
            return;
        };

        let old_area = self.kgp_state.last_area();
        let image_area = Self::image_area(terminal_size);

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
            let (actual_size, encoded_chunks) = {
                let rendered = &self.render_cache[idx];
                (rendered.actual_size, rendered.encoded_chunks.clone())
            };

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

            // Skip if already displayed.
            if self.kgp_state.last_area() == Some(area)
                && self.kgp_state.last_kgp_id() == Some(self.kgp_id)
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
            if self.clear_after_nav {
                self.writer.send(WriterRequest::ClearAll {
                    area: None,
                    is_tmux: self.is_tmux,
                });
                self.clear_after_nav = false;
            }

            self.writer.send(WriterRequest::ImageTransmit {
                encoded_chunks,
                area,
                kgp_id: self.kgp_id,
                old_area,
                epoch: self.render_epoch,
                is_tmux: self.is_tmux,
            });
            self.pending_display = Some(area);
            return;
        }

        // Request from worker if not already pending
        let pending_key = (path, target, self.fit_mode);
        if self.pending_request.as_ref() != Some(&pending_key) {
            self.worker.request(ImageRequest {
                path: pending_key.0.clone(),
                target,
                fit_mode: self.fit_mode,
                kgp_id: self.kgp_id,
                is_tmux: self.is_tmux,
                compress_level: self.config.compression_level(),
                tmux_kitty_max_pixels: self.config.tmux_kitty_max_pixels,
                trace_worker: self.config.trace_worker,
                view_mode: ViewMode::Single,
                tile_paths: None,
                tile_grid: None,
                cell_size: None,
            });
            self.pending_request = Some(pending_key);
        }
    }

    fn prepare_tile_render(&mut self, terminal_size: Rect) {
        let old_area = self.kgp_state.last_area();
        let image_area = Self::image_area(terminal_size);

        let (cell_w, cell_h) = self.picker.font_size();
        if cell_w == 0 || cell_h == 0 || image_area.width == 0 || image_area.height == 0 {
            return;
        }

        let grid = Self::calculate_tile_grid(terminal_size, self.config.cell_aspect_ratio);
        let (cols, rows) = grid;

        // Calculate canvas size in pixels
        let max_w_px = u32::from(image_area.width) * u32::from(cell_w);
        let max_h_px = u32::from(image_area.height) * u32::from(cell_h);
        let target = (max_w_px, max_h_px);

        // Get tile paths for current page
        let tiles_per_page = cols * rows;
        let page_start = (self.tile_cursor / tiles_per_page) * tiles_per_page;
        let tile_paths: Vec<PathBuf> = self
            .images
            .iter()
            .skip(page_start)
            .take(tiles_per_page)
            .cloned()
            .collect();

        if tile_paths.is_empty() {
            return;
        }

        // Use a synthetic path for tile cache key (cursor is drawn via ANSI overlay, not part of cache)
        let cache_key = PathBuf::from(format!("__tile_page_{}", page_start));

        // Check cache
        let cached_idx = self
            .render_cache
            .iter()
            .position(|r| r.path == cache_key && r.target == target && r.fit_mode == self.fit_mode);

        if let Some(idx) = cached_idx {
            let (actual_size, encoded_chunks) = {
                let rendered = &self.render_cache[idx];
                (rendered.actual_size, rendered.encoded_chunks.clone())
            };

            let cells_w = actual_size.0.div_ceil(u32::from(cell_w));
            let cells_h = actual_size.1.div_ceil(u32::from(cell_h));
            let cells_w = cells_w.min(u32::from(image_area.width)) as u16;
            let cells_h = cells_h.min(u32::from(image_area.height)) as u16;
            let area = Rect::new(image_area.x, image_area.y, cells_w, cells_h);

            if self.kgp_state.last_area() == Some(area)
                && self.kgp_state.last_kgp_id() == Some(self.kgp_id)
            {
                return;
            }
            if self.pending_display == Some(area) {
                return;
            }

            if self.in_flight_transmit {
                return;
            }
            self.in_flight_transmit = true;
            if self.clear_after_nav {
                self.writer.send(WriterRequest::ClearAll {
                    area: None,
                    is_tmux: self.is_tmux,
                });
                self.clear_after_nav = false;
            }

            self.writer.send(WriterRequest::ImageTransmit {
                encoded_chunks,
                area,
                kgp_id: self.kgp_id,
                old_area,
                epoch: self.render_epoch,
                is_tmux: self.is_tmux,
            });
            self.pending_display = Some(area);
            return;
        }

        // Request tile composite from worker (cursor is drawn via ANSI overlay)
        let pending_key = (cache_key.clone(), target, self.fit_mode);
        if self.pending_request.as_ref() != Some(&pending_key) {
            self.worker.request(ImageRequest {
                path: cache_key,
                target,
                fit_mode: self.fit_mode,
                kgp_id: self.kgp_id,
                is_tmux: self.is_tmux,
                compress_level: self.config.compression_level(),
                tmux_kitty_max_pixels: self.config.tmux_kitty_max_pixels,
                trace_worker: self.config.trace_worker,
                view_mode: ViewMode::Tile,
                tile_paths: Some(tile_paths),
                tile_grid: Some(grid),
                cell_size: Some((cell_w, cell_h)),
            });
            self.pending_request = Some(pending_key);
        }
    }

    fn prefetch_count(&self) -> usize {
        self.config.prefetch_count
    }

    /// Prefetch adjacent images (next and previous) into the render cache.
    /// Call this after the current image is fully displayed.
    pub fn prefetch_adjacent(&mut self, terminal_size: Rect) {
        // Skip if there's already a pending request (don't overwhelm the worker)
        if self.pending_request.is_some() {
            return;
        }

        let prefetch_count = self.prefetch_count();
        if prefetch_count == 0 {
            return;
        }

        let image_area = Self::image_area(terminal_size);
        let (cell_w, cell_h) = self.picker.font_size();
        if cell_w == 0 || cell_h == 0 || image_area.width == 0 || image_area.height == 0 {
            return;
        }

        let max_w_px = u32::from(image_area.width) * u32::from(cell_w);
        let max_h_px = u32::from(image_area.height) * u32::from(cell_h);
        let target = (max_w_px, max_h_px);

        // Try to prefetch next and previous images
        let len = self.images.len();
        if len <= 1 {
            return;
        }

        // Build list of indices to prefetch: next N, then prev N
        let mut indices = Vec::with_capacity(prefetch_count * 2);
        for i in 1..=prefetch_count {
            indices.push((self.current_index + i) % len); // next
        }
        for i in 1..=prefetch_count {
            indices.push((self.current_index + len - i) % len); // prev
        }

        for idx in indices {
            let path = &self.images[idx];

            // Skip if already in cache
            let in_cache = self
                .render_cache
                .iter()
                .any(|r| &r.path == path && r.target == target && r.fit_mode == self.fit_mode);
            if in_cache {
                continue;
            }

            // Send prefetch request
            self.worker.request(ImageRequest {
                path: path.clone(),
                target,
                fit_mode: self.fit_mode,
                kgp_id: self.kgp_id,
                is_tmux: self.is_tmux,
                compress_level: self.config.compression_level(),
                tmux_kitty_max_pixels: self.config.tmux_kitty_max_pixels,
                trace_worker: self.config.trace_worker,
                view_mode: ViewMode::Single,
                tile_paths: None,
                tile_grid: None,
                cell_size: None,
            });
            // Only prefetch one at a time to avoid overwhelming the worker
            break;
        }
    }

    pub fn clear_kgp_overlay(&mut self) {
        let Some(area) = self.kgp_state.last_area() else {
            return;
        };

        self.writer.send(WriterRequest::ClearAll {
            area: Some(area),
            is_tmux: self.is_tmux,
        });
    }

    /// Copy the current image's absolute path to clipboard via OSC 52.
    pub fn copy_path_to_clipboard(&self) -> bool {
        let Some(path) = self.current_path() else {
            return false;
        };
        let Some(path_str) = path.to_str() else {
            return false;
        };
        self.writer.send(WriterRequest::CopyToClipboard {
            data: path_str.as_bytes().to_vec(),
            is_tmux: self.is_tmux,
        });
        true
    }

    /// Copy the current image data to clipboard (local only, uses OS API).
    pub fn copy_image_to_clipboard(&self) -> bool {
        use arboard::{Clipboard, ImageData};

        let Some(path) = self.current_path() else {
            return false;
        };
        let Ok(img) = image::open(path) else {
            return false;
        };
        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        let image_data = ImageData {
            width: width as usize,
            height: height as usize,
            bytes: rgba.into_raw().into(),
        };
        let Ok(mut clipboard) = Clipboard::new() else {
            return false;
        };
        clipboard.set_image(image_data).is_ok()
    }

    pub fn current_image_name(&self) -> String {
        self.images
            .get(self.current_index)
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    }

    /// Get the original resolution of the current image from cache.
    fn current_image_resolution(&self) -> Option<(u32, u32)> {
        let path = self.current_path()?;
        self.render_cache
            .iter()
            .find(|r| &r.path == path)
            .map(|r| r.original_size)
    }

    pub fn status_text(&self, terminal_size: Rect) -> String {
        // Nerdfont icons
        const ICON_IMAGE: &str = "\u{e60d}"; //  (nf-seti-image)
        const SEP: &str = "\u{e0b1}"; //  (Powerline separator)

        match self.view_mode {
            ViewMode::Single => {
                // terminal_size is only used in Tile mode for grid calculation
                let resolution = self
                    .current_image_resolution()
                    .map(|(w, h)| format!(" [{w}x{h}]"))
                    .unwrap_or_default();

                let mut status = format!(
                    "{}/{} {} {} {}{}",
                    self.current_index + 1,
                    self.images.len(),
                    SEP,
                    ICON_IMAGE,
                    self.current_image_name(),
                    resolution,
                );

                if self.config.debug {
                    if self.is_tmux {
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
            ViewMode::Tile => {
                let grid = Self::calculate_tile_grid(terminal_size, self.config.cell_aspect_ratio);
                let (cols, rows) = grid;
                let tiles_per_page = cols * rows;
                let page_start = (self.tile_cursor / tiles_per_page) * tiles_per_page;
                let page_end = (page_start + tiles_per_page).min(self.images.len());
                format!(
                    "[{}-{}/{}] {} {}x{} Grid",
                    page_start + 1,
                    page_end,
                    self.images.len(),
                    SEP,
                    cols,
                    rows
                )
            }
        }
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
            view_mode: ViewMode::default(),
            tile_cursor: 0,
            prev_tile_cursor: None,
            kgp_state: KgpState::default(),
            config: Config::default(),
            worker: ImageWorker::new(),
            writer: TerminalWriter::new(),
            pending_request: None,
            render_cache: Vec::new(),
            render_cache_limit: 5,
            kgp_id: App::generate_kgp_id(),
            in_flight_transmit: false,
            pending_display: None,
            render_epoch: 0,
            clear_after_nav: false,
            is_tmux: false,
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
        let terminal = Rect::new(0, 0, 80, 24);
        let status = app.status_text(terminal);
        // New format: "{fit_icon} 1/3  {image_icon} test0.png"
        assert!(status.contains("1/3"));
        assert!(status.contains("test0.png"));
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
            original_size: (100, 100),
            actual_size: (1, 1),
            encoded_chunks: vec![b"x".to_vec()],
        });
        app.pending_request = Some((PathBuf::from("y.png"), (1, 1), FitMode::Normal));
        app.in_flight_transmit = true;

        app.reload();
        assert!(app.render_cache.is_empty());
        assert!(app.pending_request.is_none());
        assert!(!app.in_flight_transmit);
    }
}
