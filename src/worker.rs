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

use crate::fit::FitMode;
use crate::kgp::encode_chunks;

pub struct ImageRequest {
    pub path: PathBuf,
    pub target: (u32, u32),
    pub fit_mode: FitMode,
    pub kgp_id: u32,
    pub is_tmux: bool,
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

            // Decode (with cache)
            let decode_start = std::time::Instant::now();
            let decoded = if let Some((ref cached_path, ref img)) = cache {
                if cached_path == &req.path {
                    img.clone()
                } else {
                    match Self::decode_image(&req.path) {
                        Some(img) => {
                            cache = Some((req.path.clone(), img.clone()));
                            img
                        }
                        None => continue,
                    }
                }
            } else {
                match Self::decode_image(&req.path) {
                    Some(img) => {
                        cache = Some((req.path.clone(), img.clone()));
                        img
                    }
                    None => continue,
                }
            };
            let decode_elapsed = decode_start.elapsed();

            // Check for newer request after decode (most expensive step)
            if let Ok(newer) = request_rx.try_recv() {
                pending = Some(Self::drain_to_latest(&request_rx, newer));
                continue; // Abandon current work
            }

            let (orig_w, orig_h) = (decoded.width(), decoded.height());
            let (max_w, max_h) = req.target;
            let (mut target_w, mut target_h) =
                Self::compute_target((orig_w, orig_h), (max_w, max_h), req.fit_mode);

            // Apply max pixels limit (for tmux+kitty compatibility).
            // In `Fit` mode we allow larger images (may be slower / unsupported in some setups).
            if req.fit_mode != FitMode::Fit {
                let max_pixels: u64 = std::env::var("SVT_TMUX_KITTY_MAX_PIXELS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(2_000_000);
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
                pending = Some(Self::drain_to_latest(&request_rx, newer));
                continue;
            }

            // Encode
            let encode_start = std::time::Instant::now();
            let encoded_chunks = encode_chunks(&resized, req.kgp_id, req.is_tmux);
            let encode_elapsed = encode_start.elapsed();

            if std::env::var_os("SVT_TRACE_WORKER").is_some() {
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
                path: req.path,
                target: req.target,
                fit_mode: req.fit_mode,
                original_size: (orig_w, orig_h),
                actual_size,
                encoded_chunks,
            });
        }
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

    pub fn request(&self, req: ImageRequest) {
        let _ = self.request_tx.send(req);
    }

    pub fn try_recv(&self) -> Option<ImageResult> {
        self.result_rx.try_recv().ok()
    }
}
