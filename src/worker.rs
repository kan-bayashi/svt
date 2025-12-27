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

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use image::{DynamicImage, RgbaImage};

use crate::fit::{FitMode, ViewMode};
use crate::kgp::encode_chunks;

/// Cache key for tile thumbnails: (path, width, height)
type ThumbnailKey = (PathBuf, u32, u32);

/// LRU cache for tile thumbnails
struct ThumbnailCache {
    cache: HashMap<ThumbnailKey, Arc<RgbaImage>>,
    order: VecDeque<ThumbnailKey>,
    capacity: usize,
}

impl ThumbnailCache {
    fn new(capacity: usize) -> Self {
        Self {
            cache: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn get(&mut self, key: &ThumbnailKey) -> Option<Arc<RgbaImage>> {
        if let Some(img) = self.cache.get(key) {
            // Move to back (most recently used)
            self.order.retain(|k| k != key);
            self.order.push_back(key.clone());
            Some(Arc::clone(img))
        } else {
            None
        }
    }

    fn insert(&mut self, key: ThumbnailKey, img: Arc<RgbaImage>) {
        if self.cache.contains_key(&key) {
            self.order.retain(|k| k != &key);
        } else if self.cache.len() >= self.capacity {
            // Evict oldest
            if let Some(oldest) = self.order.pop_front() {
                self.cache.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.cache.insert(key, img);
    }
}

pub struct ImageRequest {
    pub path: PathBuf,
    pub target: (u32, u32),
    pub fit_mode: FitMode,
    pub kgp_id: u32,
    pub is_tmux: bool,
    pub compress_level: Option<u32>,
    pub tmux_kitty_max_pixels: u64,
    pub trace_worker: bool,
    // Resize filter for Single mode
    pub resize_filter: image::imageops::FilterType,
    // Tile mode fields
    pub view_mode: ViewMode,
    pub tile_paths: Option<Vec<PathBuf>>,
    pub tile_grid: Option<(usize, usize)>,
    pub cell_size: Option<(u16, u16)>, // (width, height) in pixels for padding calculation
    pub tile_filter: image::imageops::FilterType,
}

pub struct ImageResult {
    pub path: PathBuf,
    pub target: (u32, u32),
    pub fit_mode: FitMode,
    pub original_size: (u32, u32),
    pub actual_size: (u32, u32),
    pub encoded_chunks: Arc<Vec<Vec<u8>>>,
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
        let mut cache: Option<(PathBuf, Arc<DynamicImage>)> = None;
        let mut thumbnail_cache = ThumbnailCache::new(500); // Cache up to 500 thumbnails
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
                    Self::process_tile_request(
                        &req,
                        &mut thumbnail_cache,
                        &mut pending,
                        &request_rx,
                        &result_tx,
                    );
                }
            }
        }
    }

    fn process_single_request(
        req: &ImageRequest,
        cache: &mut Option<(PathBuf, Arc<DynamicImage>)>,
        pending: &mut Option<ImageRequest>,
        request_rx: &Receiver<ImageRequest>,
        result_tx: &Sender<ImageResult>,
    ) {
        // Decode (with cache) - Arc clone is cheap (reference count only)
        let decode_start = std::time::Instant::now();
        let decoded: Arc<DynamicImage> = if let Some((cached_path, img)) = cache.as_ref() {
            if cached_path == &req.path {
                Arc::clone(img)
            } else {
                match Self::decode_image(&req.path) {
                    Some(img) => {
                        let arc_img = Arc::new(img);
                        *cache = Some((req.path.clone(), Arc::clone(&arc_img)));
                        arc_img
                    }
                    None => return,
                }
            }
        } else {
            match Self::decode_image(&req.path) {
                Some(img) => {
                    let arc_img = Arc::new(img);
                    *cache = Some((req.path.clone(), Arc::clone(&arc_img)));
                    arc_img
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

        // Resize - use Cow to avoid clone when no resize needed
        use std::borrow::Cow;
        let resize_start = std::time::Instant::now();
        let resized: Cow<'_, DynamicImage> = if target_w != orig_w || target_h != orig_h {
            Cow::Owned(decoded.resize(target_w, target_h, req.resize_filter))
        } else {
            Cow::Borrowed(&*decoded)
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
            encoded_chunks: Arc::new(encoded_chunks),
        });
    }

    fn process_tile_request(
        req: &ImageRequest,
        thumbnail_cache: &mut ThumbnailCache,
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
        let Some((composite, actual_size)) = Self::composite_tile_images(
            tile_paths,
            grid,
            req.target,
            req.cell_size,
            req.tile_filter,
            thumbnail_cache,
            req.trace_worker,
        ) else {
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
            encoded_chunks: Arc::new(encoded_chunks),
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

    fn decode_image(path: &std::path::Path) -> Option<DynamicImage> {
        image::ImageReader::open(path).ok()?.decode().ok()
    }

    /// Composite multiple images into a single tile grid image (without cursor).
    /// Uses thumbnail cache and parallel processing for decode/resize operations.
    fn composite_tile_images(
        paths: &[PathBuf],
        grid: (usize, usize),
        canvas_size: (u32, u32),
        cell_size: Option<(u16, u16)>,
        filter: image::imageops::FilterType,
        thumbnail_cache: &mut ThumbnailCache,
        trace_worker: bool,
    ) -> Option<(DynamicImage, (u32, u32))> {
        use image::{GenericImage, Rgba};
        use rayon::prelude::*;

        let (cols, rows) = grid;
        let (canvas_w, canvas_h) = canvas_size;

        // Get cell dimensions for alignment
        let (cell_w, cell_h) = cell_size.unwrap_or((8, 16));
        let cell_w = u32::from(cell_w);
        let cell_h = u32::from(cell_h);

        // Calculate canvas size in cells (for cell-aligned tile boundaries)
        let canvas_w_cells = canvas_w / cell_w;
        let canvas_h_cells = canvas_h / cell_h;

        // Padding around each thumbnail (leaves space for cursor border).
        let half_pad_w = cell_w;
        let half_pad_h = cell_h;

        // Prepare tile info and check cache
        struct TileInfo {
            path: PathBuf,
            tile_x: u32,
            tile_y: u32,
            inner_w: u32,
            inner_h: u32,
        }

        let mut cached_tiles: Vec<(u32, u32, Arc<RgbaImage>)> = Vec::new();
        let mut uncached_tiles: Vec<TileInfo> = Vec::new();

        for (i, path) in paths.iter().take(cols * rows).enumerate() {
            let col = i % cols;
            let row = i / cols;

            let tile_x_cells = (col as u32 * canvas_w_cells) / cols as u32;
            let tile_y_cells = (row as u32 * canvas_h_cells) / rows as u32;
            let next_tile_x_cells = ((col + 1) as u32 * canvas_w_cells) / cols as u32;
            let next_tile_y_cells = ((row + 1) as u32 * canvas_h_cells) / rows as u32;

            let tile_x = tile_x_cells * cell_w;
            let tile_y = tile_y_cells * cell_h;
            let tile_w = (next_tile_x_cells - tile_x_cells) * cell_w;
            let tile_h = (next_tile_y_cells - tile_y_cells) * cell_h;

            let inner_w = tile_w.saturating_sub(half_pad_w * 2);
            let inner_h = tile_h.saturating_sub(half_pad_h * 2);

            if inner_w == 0 || inner_h == 0 {
                continue;
            }

            let cache_key = (path.clone(), inner_w, inner_h);
            if let Some(cached_thumb) = thumbnail_cache.get(&cache_key) {
                // Cache hit: calculate position and add to cached_tiles
                let scaled_w = cached_thumb.width();
                let scaled_h = cached_thumb.height();
                let img_x = tile_x + half_pad_w + (inner_w.saturating_sub(scaled_w)) / 2;
                let img_y = tile_y + half_pad_h + (inner_h.saturating_sub(scaled_h)) / 2;
                cached_tiles.push((img_x, img_y, cached_thumb));
            } else {
                // Cache miss: add to uncached_tiles for parallel processing
                uncached_tiles.push(TileInfo {
                    path: path.clone(),
                    tile_x,
                    tile_y,
                    inner_w,
                    inner_h,
                });
            }
        }

        // Parallel decode and resize for cache misses
        let new_tiles: Vec<_> = uncached_tiles
            .par_iter()
            .filter_map(|info| {
                let img = match Self::decode_image(&info.path) {
                    Some(img) => img,
                    None => {
                        if trace_worker {
                            use std::io::Write as _;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open("/tmp/svt_worker.log")
                            {
                                let _ = writeln!(f, "tile decode failed: {:?}", info.path);
                            }
                        }
                        return None;
                    }
                };
                let (orig_w, orig_h) = (img.width(), img.height());

                let scale_w = info.inner_w as f64 / orig_w as f64;
                let scale_h = info.inner_h as f64 / orig_h as f64;
                let scale = scale_w.min(scale_h).min(1.0);

                let scaled_w = (orig_w as f64 * scale).floor().max(1.0) as u32;
                let scaled_h = (orig_h as f64 * scale).floor().max(1.0) as u32;

                let thumbnail = img.resize(scaled_w, scaled_h, filter);
                let rgba_thumb = Arc::new(thumbnail.to_rgba8());

                let img_x = info.tile_x + half_pad_w + (info.inner_w.saturating_sub(scaled_w)) / 2;
                let img_y = info.tile_y + half_pad_h + (info.inner_h.saturating_sub(scaled_h)) / 2;

                Some((
                    info.path.clone(),
                    info.inner_w,
                    info.inner_h,
                    img_x,
                    img_y,
                    rgba_thumb,
                ))
            })
            .collect();

        // Add new thumbnails to cache
        for (path, inner_w, inner_h, img_x, img_y, rgba_thumb) in new_tiles {
            let cache_key = (path, inner_w, inner_h);
            thumbnail_cache.insert(cache_key, Arc::clone(&rgba_thumb));
            cached_tiles.push((img_x, img_y, rgba_thumb));
        }

        // Sequential copy to canvas
        let mut canvas = RgbaImage::from_pixel(canvas_w, canvas_h, Rgba([0, 0, 0, 0]));
        for (img_x, img_y, rgba_thumb) in cached_tiles {
            let scaled_w = rgba_thumb.width();
            let scaled_h = rgba_thumb.height();
            if img_x + scaled_w <= canvas_w && img_y + scaled_h <= canvas_h {
                let _ = canvas.copy_from(&*rgba_thumb, img_x, img_y);
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
