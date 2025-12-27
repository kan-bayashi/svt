// Copyright 2025 Tomoki Hayashi
// MIT License (https://opensource.org/licenses/MIT)

//! Image worker thread.
//!
//! This thread performs the expensive work:
//! - decode image from disk
//! - resize based on terminal size and `FitMode`
//! - encode to Kitty Graphics Protocol chunks for transmission
//!
//! Requests are best-effort; newer requests may preempt older ones.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use image::DynamicImage;

use crate::fit::{FitMode, ViewMode};
use crate::kgp::encode_chunks;

pub struct ImageRequest {
    pub path: PathBuf,
    pub target: (u32, u32),
    pub fit_mode: FitMode,
    pub kgp_id: u32,
    pub is_tmux: bool,
    pub compress_level: Option<u32>,
    pub tmux_kitty_max_pixels: u64,
    pub trace_worker: bool,
    // Tile mode fields
    pub view_mode: ViewMode,
    pub tile_paths: Option<Vec<PathBuf>>,
    pub tile_grid: Option<(usize, usize)>,
    pub cell_size: Option<(u16, u16)>, // (width, height) in pixels for padding calculation
}

pub struct ImageResult {
    pub path: PathBuf,
    pub target: (u32, u32),
    pub fit_mode: FitMode,
    pub original_size: (u32, u32),
    pub actual_size: (u32, u32),
    pub encoded_chunks: Vec<Vec<u8>>,
}

pub struct ImageWorker {
    request_tx: Sender<ImageRequest>,
    result_rx: Receiver<ImageResult>,
    _handle: JoinHandle<()>,
}

impl ImageWorker {
    pub fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<ImageRequest>();
        let (result_tx, result_rx) = mpsc::channel::<ImageResult>();

        let handle = thread::spawn(move || {
            Self::worker_loop(request_rx, result_tx);
        });

        Self {
            request_tx,
            result_rx,
            _handle: handle,
        }
    }

    fn drain_to_latest(
        request_rx: &Receiver<ImageRequest>,
        mut current: ImageRequest,
    ) -> ImageRequest {
        while let Ok(newer) = request_rx.try_recv() {
            current = newer;
        }
        current
    }

    fn worker_loop(request_rx: Receiver<ImageRequest>, result_tx: Sender<ImageResult>) {
        let mut cache: Option<(PathBuf, DynamicImage)> = None;
        let mut pending: Option<ImageRequest> = None;

        loop {
            // Get next request: from pending or wait for new one
            let req = if let Some(p) = pending.take() {
                p
            } else {
                match request_rx.recv() {
                    Ok(r) => r,
                    Err(_) => break,
                }
            };

            // Drain any pending requests, keep only the latest
            let req = Self::drain_to_latest(&request_rx, req);

            match req.view_mode {
                ViewMode::Single => {
                    Self::process_single_request(
                        &req,
                        &mut cache,
                        &mut pending,
                        &request_rx,
                        &result_tx,
                    );
                }
                ViewMode::Tile => {
                    Self::process_tile_request(&req, &mut pending, &request_rx, &result_tx);
                }
            }
        }
    }

    fn process_single_request(
        req: &ImageRequest,
        cache: &mut Option<(PathBuf, DynamicImage)>,
        pending: &mut Option<ImageRequest>,
        request_rx: &Receiver<ImageRequest>,
        result_tx: &Sender<ImageResult>,
    ) {
        // Decode (with cache)
        let decode_start = std::time::Instant::now();
        let decoded = if let Some((cached_path, img)) = cache.as_ref() {
            if cached_path == &req.path {
                img.clone()
            } else {
                match Self::decode_image(&req.path) {
                    Some(img) => {
                        *cache = Some((req.path.clone(), img.clone()));
                        img
                    }
                    None => return,
                }
            }
        } else {
            match Self::decode_image(&req.path) {
                Some(img) => {
                    *cache = Some((req.path.clone(), img.clone()));
                    img
                }
                None => return,
            }
        };
        let decode_elapsed = decode_start.elapsed();

        // Check for newer request after decode (most expensive step)
        if let Ok(newer) = request_rx.try_recv() {
            *pending = Some(Self::drain_to_latest(request_rx, newer));
            return; // Abandon current work
        }

        let (orig_w, orig_h) = (decoded.width(), decoded.height());
        let (max_w, max_h) = req.target;
        let (mut target_w, mut target_h) =
            Self::compute_target((orig_w, orig_h), (max_w, max_h), req.fit_mode);

        // Apply max pixels limit (for tmux+kitty compatibility).
        // In `Fit` mode we allow larger images (may be slower / unsupported in some setups).
        if req.fit_mode != FitMode::Fit {
            let max_pixels = req.tmux_kitty_max_pixels;
            let target_pixels = (target_w as u64).saturating_mul(target_h as u64);
            if target_pixels > max_pixels {
                let down = (max_pixels as f64 / target_pixels as f64).sqrt();
                target_w = (target_w as f64 * down).floor().max(1.0) as u32;
                target_h = (target_h as f64 * down).floor().max(1.0) as u32;
            }
        }

        // Resize
        let resize_start = std::time::Instant::now();
        let resized = if target_w != orig_w || target_h != orig_h {
            decoded.thumbnail(target_w, target_h)
        } else {
            decoded
        };
        let actual_size = (resized.width(), resized.height());
        let resize_elapsed = resize_start.elapsed();

        // Check for newer request after resize
        if let Ok(newer) = request_rx.try_recv() {
            *pending = Some(Self::drain_to_latest(request_rx, newer));
            return;
        }

        // Encode
        let encode_start = std::time::Instant::now();
        let encoded_chunks = encode_chunks(&resized, req.kgp_id, req.is_tmux, req.compress_level);
        let encode_elapsed = encode_start.elapsed();

        if req.trace_worker {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/svt_worker.log")
            {
                let _ = writeln!(
                    f,
                    "kgp_id={} path={:?} decode={:?} resize={:?} encode={:?} orig=({},{}) target=({},{}) actual=({},{})",
                    req.kgp_id,
                    req.path,
                    decode_elapsed,
                    resize_elapsed,
                    encode_elapsed,
                    orig_w,
                    orig_h,
                    max_w,
                    max_h,
                    actual_size.0,
                    actual_size.1
                );
            }
        }

        // Send result
        let _ = result_tx.send(ImageResult {
            path: req.path.clone(),
            target: req.target,
            fit_mode: req.fit_mode,
            original_size: (orig_w, orig_h),
            actual_size,
            encoded_chunks,
        });
    }

    fn process_tile_request(
        req: &ImageRequest,
        pending: &mut Option<ImageRequest>,
        request_rx: &Receiver<ImageRequest>,
        result_tx: &Sender<ImageResult>,
    ) {
        let Some(ref tile_paths) = req.tile_paths else {
            return;
        };
        let Some(grid) = req.tile_grid else {
            return;
        };

        // Composite tile images (cursor is drawn separately via ANSI)
        let Some((composite, actual_size)) =
            Self::composite_tile_images(tile_paths, grid, req.target, req.cell_size)
        else {
            return;
        };

        // Check for newer request
        if let Ok(newer) = request_rx.try_recv() {
            *pending = Some(Self::drain_to_latest(request_rx, newer));
            return;
        }

        // Encode
        let encoded_chunks = encode_chunks(&composite, req.kgp_id, req.is_tmux, req.compress_level);

        // Send result
        let _ = result_tx.send(ImageResult {
            path: req.path.clone(),
            target: req.target,
            fit_mode: req.fit_mode,
            original_size: actual_size,
            actual_size,
            encoded_chunks,
        });
    }

    fn compute_target(orig: (u32, u32), max: (u32, u32), fit_mode: FitMode) -> (u32, u32) {
        let (orig_w, orig_h) = orig;
        let (max_w, max_h) = max;

        match fit_mode {
            FitMode::Normal => {
                // Contain + shrink-only (don't enlarge small images).
                if orig_w > max_w || orig_h > max_h {
                    let scale_w = max_w as f64 / orig_w as f64;
                    let scale_h = max_h as f64 / orig_h as f64;
                    let scale = scale_w.min(scale_h);
                    (
                        (orig_w as f64 * scale).floor().max(1.0) as u32,
                        (orig_h as f64 * scale).floor().max(1.0) as u32,
                    )
                } else {
                    (orig_w, orig_h)
                }
            }
            FitMode::Fit => {
                // Contain + allow upscale to fill the viewport as much as possible without overflow.
                let scale_w = max_w as f64 / orig_w as f64;
                let scale_h = max_h as f64 / orig_h as f64;
                let scale = scale_w.min(scale_h);
                (
                    (orig_w as f64 * scale).floor().max(1.0) as u32,
                    (orig_h as f64 * scale).floor().max(1.0) as u32,
                )
            }
        }
    }

    fn decode_image(path: &PathBuf) -> Option<DynamicImage> {
        image::ImageReader::open(path).ok()?.decode().ok()
    }

    /// Composite multiple images into a single tile grid image (without cursor).
    fn composite_tile_images(
        paths: &[PathBuf],
        grid: (usize, usize),
        canvas_size: (u32, u32),
        cell_size: Option<(u16, u16)>,
    ) -> Option<(DynamicImage, (u32, u32))> {
        use image::{GenericImage, Rgba, RgbaImage};

        let (cols, rows) = grid;
        let (canvas_w, canvas_h) = canvas_size;

        // Calculate tile dimensions
        let tile_w = canvas_w / cols as u32;
        let tile_h = canvas_h / rows as u32;

        // Padding around each thumbnail (leaves space for cursor border).
        // Cursor is 1 cell wide, so padding needs to be at least 1 cell in pixels.
        let tile_padding = match cell_size {
            Some((cell_w, cell_h)) => u32::from(cell_w.max(cell_h)),
            None => 16, // fallback
        };

        // Create canvas with transparent background
        let mut canvas = RgbaImage::from_pixel(canvas_w, canvas_h, Rgba([0, 0, 0, 0]));

        for (i, path) in paths.iter().enumerate() {
            if i >= cols * rows {
                break;
            }

            let col = i % cols;
            let row = i / cols;

            // Half padding on all sides (adjacent tiles share the border)
            let half_pad = tile_padding / 2;

            // Calculate inner size for this specific tile
            let inner_w = tile_w.saturating_sub(half_pad * 2);
            let inner_h = tile_h.saturating_sub(half_pad * 2);

            if inner_w == 0 || inner_h == 0 {
                continue;
            }

            // Decode and resize image to fit tile (with padding)
            let img = match Self::decode_image(path) {
                Some(img) => img,
                None => continue, // Skip failed images instead of returning None
            };
            let (orig_w, orig_h) = (img.width(), img.height());

            // Calculate scaled size to fit within inner area while preserving aspect ratio
            let scale_w = inner_w as f64 / orig_w as f64;
            let scale_h = inner_h as f64 / orig_h as f64;
            let scale = scale_w.min(scale_h).min(1.0); // Don't upscale

            let scaled_w = (orig_w as f64 * scale).floor().max(1.0) as u32;
            let scaled_h = (orig_h as f64 * scale).floor().max(1.0) as u32;

            let thumbnail = img.thumbnail(scaled_w, scaled_h);
            let rgba_thumb = thumbnail.to_rgba8();

            // Calculate position to center image in tile cell (with padding)
            let tile_x = col as u32 * tile_w + half_pad;
            let tile_y = row as u32 * tile_h + half_pad;
            let offset_x = (inner_w.saturating_sub(scaled_w)) / 2;
            let offset_y = (inner_h.saturating_sub(scaled_h)) / 2;

            let x = tile_x + offset_x;
            let y = tile_y + offset_y;

            // Copy thumbnail to canvas
            if x + scaled_w <= canvas_w && y + scaled_h <= canvas_h {
                let _ = canvas.copy_from(&rgba_thumb, x, y);
            }
        }

        let actual_size = (canvas_w, canvas_h);
        Some((DynamicImage::ImageRgba8(canvas), actual_size))
    }

    pub fn request(&self, req: ImageRequest) {
        let _ = self.request_tx.send(req);
    }

    pub fn try_recv(&self) -> Option<ImageResult> {
        self.result_rx.try_recv().ok()
    }
}
