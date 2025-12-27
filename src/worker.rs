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

/// Default capacity for the tile thumbnail LRU cache.
const THUMBNAIL_CACHE_SIZE: usize = 500;

/// Cache key for tile thumbnails: (path, width, height, filter)
type ThumbnailKey = (PathBuf, u32, u32, u8);

fn filter_cache_id(filter: image::imageops::FilterType) -> u8 {
    match filter {
        image::imageops::FilterType::Nearest => 0,
        image::imageops::FilterType::Triangle => 1,
        image::imageops::FilterType::CatmullRom => 2,
        image::imageops::FilterType::Gaussian => 3,
        image::imageops::FilterType::Lanczos3 => 4,
    }
}

/// LRU cache for tile thumbnails
struct ThumbnailCache {
    cache: HashMap<ThumbnailKey, Arc<RgbaImage>>,
    order: VecDeque<ThumbnailKey>,
    capacity: usize,
}

impl ThumbnailCache {
    fn new(capacity: usize) -> Self {
        Self {
            cache: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn get(&mut self, key: &ThumbnailKey) -> Option<Arc<RgbaImage>> {
        let img = self.cache.get(key)?;
        if !matches!(self.order.back(), Some(k) if k == key) {
            // Move to back (most recently used)
            self.order.retain(|k| k != key);
            self.order.push_back(key.clone());
        }
        Some(Arc::clone(img))
    }

    fn insert(&mut self, key: ThumbnailKey, img: Arc<RgbaImage>) {
        if self.cache.contains_key(&key) {
            if !matches!(self.order.back(), Some(k) if k == &key) {
                self.order.retain(|k| k != &key);
                self.order.push_back(key.clone());
            }
            self.cache.insert(key, img);
            return;
        }
        if self.cache.len() >= self.capacity {
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
    pub fn new(tile_threads: usize) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<ImageRequest>();
        let (result_tx, result_rx) = mpsc::channel::<ImageResult>();

        let handle = thread::spawn(move || {
            Self::worker_loop(request_rx, result_tx, tile_threads);
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

    fn worker_loop(
        request_rx: Receiver<ImageRequest>,
        result_tx: Sender<ImageResult>,
        tile_threads: usize,
    ) {
        let mut cache: Option<(PathBuf, Arc<DynamicImage>)> = None;
        let mut thumbnail_cache = ThumbnailCache::new(THUMBNAIL_CACHE_SIZE);
        let mut pending: Option<ImageRequest> = None;

        // Create dedicated thread pool for tile processing
        let tile_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(tile_threads)
            .build()
            .expect("Failed to create tile thread pool");

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
                        &tile_pool,
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
        tile_pool: &rayon::ThreadPool,
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
            tile_pool,
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

    pub fn compute_target(orig: (u32, u32), max: (u32, u32), fit_mode: FitMode) -> (u32, u32) {
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

    pub fn decode_image(path: &std::path::Path) -> Option<DynamicImage> {
        image::ImageReader::open(path).ok()?.decode().ok()
    }

    /// Composite multiple images into a single tile grid image (without cursor).
    /// Uses thumbnail cache and parallel processing for decode/resize operations.
    #[allow(clippy::too_many_arguments)]
    fn composite_tile_images(
        paths: &[PathBuf],
        grid: (usize, usize),
        canvas_size: (u32, u32),
        cell_size: Option<(u16, u16)>,
        filter: image::imageops::FilterType,
        thumbnail_cache: &mut ThumbnailCache,
        tile_pool: &rayon::ThreadPool,
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

        let filter_id = filter_cache_id(filter);
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

            let cache_key = (path.clone(), inner_w, inner_h, filter_id);
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

        // Parallel decode and resize for cache misses (using dedicated thread pool)
        let new_tiles: Vec<_> = tile_pool.install(|| {
            uncached_tiles
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

                    let img_x =
                        info.tile_x + half_pad_w + (info.inner_w.saturating_sub(scaled_w)) / 2;
                    let img_y =
                        info.tile_y + half_pad_h + (info.inner_h.saturating_sub(scaled_h)) / 2;

                    Some((
                        info.path.clone(),
                        info.inner_w,
                        info.inner_h,
                        img_x,
                        img_y,
                        rgba_thumb,
                    ))
                })
                .collect()
        });

        // Add new thumbnails to cache
        for (path, inner_w, inner_h, img_x, img_y, rgba_thumb) in new_tiles {
            let cache_key = (path, inner_w, inner_h, filter_id);
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

    /// Process a single image: decode → resize → encode.
    /// Used by both ImageWorker and PrefetchWorker.
    #[allow(clippy::too_many_arguments)]
    pub fn process_image(
        path: &std::path::Path,
        target: (u32, u32),
        fit_mode: FitMode,
        kgp_id: u32,
        is_tmux: bool,
        compress_level: Option<u32>,
        tmux_kitty_max_pixels: u64,
        resize_filter: image::imageops::FilterType,
    ) -> Option<ImageResult> {
        // Decode
        let decoded = Self::decode_image(path)?;
        let (orig_w, orig_h) = (decoded.width(), decoded.height());
        let (max_w, max_h) = target;

        // Compute target size
        let (mut target_w, mut target_h) =
            Self::compute_target((orig_w, orig_h), (max_w, max_h), fit_mode);

        // Apply max pixels limit (for tmux+kitty compatibility)
        if fit_mode != FitMode::Fit {
            let max_pixels = tmux_kitty_max_pixels;
            let target_pixels = (target_w as u64).saturating_mul(target_h as u64);
            if target_pixels > max_pixels {
                let down = (max_pixels as f64 / target_pixels as f64).sqrt();
                target_w = (target_w as f64 * down).floor().max(1.0) as u32;
                target_h = (target_h as f64 * down).floor().max(1.0) as u32;
            }
        }

        // Resize
        use std::borrow::Cow;
        let resized: Cow<'_, DynamicImage> = if target_w != orig_w || target_h != orig_h {
            Cow::Owned(decoded.resize(target_w, target_h, resize_filter))
        } else {
            Cow::Borrowed(&decoded)
        };
        let actual_size = (resized.width(), resized.height());

        // Encode
        let encoded_chunks = encode_chunks(&resized, kgp_id, is_tmux, compress_level);

        Some(ImageResult {
            path: path.to_path_buf(),
            target,
            fit_mode,
            original_size: (orig_w, orig_h),
            actual_size,
            encoded_chunks: Arc::new(encoded_chunks),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_image(w: u32, h: u32) -> Arc<RgbaImage> {
        Arc::new(RgbaImage::from_pixel(w, h, image::Rgba([255, 0, 0, 255])))
    }

    #[test]
    fn test_thumbnail_cache_basic_operations() {
        let mut cache = ThumbnailCache::new(3);
        let key1 = (PathBuf::from("a.png"), 100, 100, 0);
        let key2 = (PathBuf::from("b.png"), 100, 100, 0);

        let img1 = create_test_image(100, 100);
        let img2 = create_test_image(100, 100);

        // Insert and retrieve
        cache.insert(key1.clone(), Arc::clone(&img1));
        assert!(cache.get(&key1).is_some());
        assert!(cache.get(&key2).is_none());

        cache.insert(key2.clone(), Arc::clone(&img2));
        assert!(cache.get(&key1).is_some());
        assert!(cache.get(&key2).is_some());
    }

    #[test]
    fn test_thumbnail_cache_lru_eviction() {
        let mut cache = ThumbnailCache::new(2);
        let key1 = (PathBuf::from("a.png"), 100, 100, 0);
        let key2 = (PathBuf::from("b.png"), 100, 100, 0);
        let key3 = (PathBuf::from("c.png"), 100, 100, 0);

        let img = create_test_image(100, 100);

        cache.insert(key1.clone(), Arc::clone(&img));
        cache.insert(key2.clone(), Arc::clone(&img));

        // Cache is full (capacity=2), inserting key3 should evict key1 (oldest)
        cache.insert(key3.clone(), Arc::clone(&img));

        assert!(cache.get(&key1).is_none()); // Evicted
        assert!(cache.get(&key2).is_some());
        assert!(cache.get(&key3).is_some());
    }

    #[test]
    fn test_thumbnail_cache_lru_access_order() {
        let mut cache = ThumbnailCache::new(2);
        let key1 = (PathBuf::from("a.png"), 100, 100, 0);
        let key2 = (PathBuf::from("b.png"), 100, 100, 0);
        let key3 = (PathBuf::from("c.png"), 100, 100, 0);

        let img = create_test_image(100, 100);

        cache.insert(key1.clone(), Arc::clone(&img));
        cache.insert(key2.clone(), Arc::clone(&img));

        // Access key1 to move it to the back (most recently used)
        let _ = cache.get(&key1);

        // Insert key3 should evict key2 (now oldest)
        cache.insert(key3.clone(), Arc::clone(&img));

        assert!(cache.get(&key1).is_some()); // Still present (was accessed)
        assert!(cache.get(&key2).is_none()); // Evicted
        assert!(cache.get(&key3).is_some());
    }

    #[test]
    fn test_thumbnail_cache_update_existing() {
        let mut cache = ThumbnailCache::new(2);
        let key1 = (PathBuf::from("a.png"), 100, 100, 0);

        let img1 = create_test_image(100, 100);
        let img2 = create_test_image(50, 50);

        cache.insert(key1.clone(), Arc::clone(&img1));
        cache.insert(key1.clone(), Arc::clone(&img2));

        // Should still have only one entry
        assert_eq!(cache.cache.len(), 1);

        // Should return the updated image
        let retrieved = cache.get(&key1).unwrap();
        assert_eq!(retrieved.width(), 50);
    }

    #[test]
    fn test_filter_cache_id() {
        assert_eq!(filter_cache_id(image::imageops::FilterType::Nearest), 0);
        assert_eq!(filter_cache_id(image::imageops::FilterType::Triangle), 1);
        assert_eq!(filter_cache_id(image::imageops::FilterType::CatmullRom), 2);
        assert_eq!(filter_cache_id(image::imageops::FilterType::Gaussian), 3);
        assert_eq!(filter_cache_id(image::imageops::FilterType::Lanczos3), 4);
    }

    #[test]
    fn test_compute_target_normal_shrink() {
        // Large image should be shrunk to fit
        let result = ImageWorker::compute_target((2000, 1000), (800, 600), FitMode::Normal);
        assert!(result.0 <= 800);
        assert!(result.1 <= 600);
        // Aspect ratio preserved
        let orig_ratio = 2000.0 / 1000.0;
        let result_ratio = result.0 as f64 / result.1 as f64;
        assert!((orig_ratio - result_ratio).abs() < 0.01);
    }

    #[test]
    fn test_compute_target_normal_no_enlarge() {
        // Small image should not be enlarged in Normal mode
        let result = ImageWorker::compute_target((100, 50), (800, 600), FitMode::Normal);
        assert_eq!(result, (100, 50));
    }

    #[test]
    fn test_compute_target_fit_enlarge() {
        // Small image should be enlarged in Fit mode
        let result = ImageWorker::compute_target((100, 50), (800, 600), FitMode::Fit);
        assert!(result.0 > 100);
        assert!(result.1 > 50);
        assert!(result.0 <= 800);
        assert!(result.1 <= 600);
    }
}
