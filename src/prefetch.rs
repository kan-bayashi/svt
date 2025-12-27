// Copyright 2025 Tomoki Hayashi
// MIT License (https://opensource.org/licenses/MIT)

//! Prefetch worker for parallel image pre-loading.
//!
//! This module provides a dedicated worker thread for prefetching images
//! in parallel using rayon. It runs independently from the main ImageWorker,
//! allowing prefetch operations to not block the main rendering.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use rayon::prelude::*;

use crate::fit::FitMode;
use crate::worker::{ImageResult, ImageWorker};

/// Epoch-based cancellation token.
/// Incremented on navigation to invalidate in-flight prefetch requests.
struct PrefetchEpoch(AtomicU64);

impl PrefetchEpoch {
    fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    fn increment(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// Batch prefetch request.
pub struct PrefetchRequest {
    pub paths: Vec<PathBuf>,
    pub target: (u32, u32),
    pub fit_mode: FitMode,
    pub epoch: u64,
    pub kgp_id: u32,
    pub is_tmux: bool,
    pub compress_level: Option<u32>,
    pub tmux_kitty_max_pixels: u64,
    pub resize_filter: image::imageops::FilterType,
}

/// Internal command for prefetch worker.
enum PrefetchCommand {
    Batch(PrefetchRequest),
    Shutdown,
}

/// Prefetch worker manages a dedicated thread for parallel image prefetching.
pub struct PrefetchWorker {
    command_tx: Sender<PrefetchCommand>,
    result_rx: Receiver<(u64, ImageResult)>,
    epoch: Arc<PrefetchEpoch>,
    _handle: JoinHandle<()>,
}

impl PrefetchWorker {
    /// Create a new prefetch worker with the specified thread count.
    pub fn new(thread_count: usize) -> Self {
        let (command_tx, command_rx) = mpsc::channel::<PrefetchCommand>();
        let (result_tx, result_rx) = mpsc::channel::<(u64, ImageResult)>();
        let epoch = Arc::new(PrefetchEpoch::new());
        let epoch_clone = Arc::clone(&epoch);

        let handle = thread::spawn(move || {
            Self::coordinator_loop(command_rx, result_tx, epoch_clone, thread_count);
        });

        Self {
            command_tx,
            result_rx,
            epoch,
            _handle: handle,
        }
    }

    /// Submit a batch of paths for prefetching.
    pub fn prefetch_batch(&self, req: PrefetchRequest) {
        let _ = self.command_tx.send(PrefetchCommand::Batch(req));
    }

    /// Cancel all pending prefetch requests by incrementing the epoch.
    pub fn cancel(&self) {
        self.epoch.increment();
    }

    /// Get current epoch for creating new requests.
    pub fn current_epoch(&self) -> u64 {
        self.epoch.current()
    }

    /// Poll for completed prefetch results.
    /// Returns results that match the current epoch, discarding stale ones.
    pub fn try_recv(&self) -> Option<ImageResult> {
        let current = self.current_epoch();
        while let Ok((epoch, result)) = self.result_rx.try_recv() {
            if epoch >= current {
                return Some(result);
            }
            // Discard stale results (epoch < current)
        }
        None
    }

    fn coordinator_loop(
        command_rx: Receiver<PrefetchCommand>,
        result_tx: Sender<(u64, ImageResult)>,
        epoch: Arc<PrefetchEpoch>,
        thread_count: usize,
    ) {
        // Create dedicated rayon thread pool for prefetch
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(thread_count)
            .build()
            .expect("Failed to create prefetch thread pool");

        while let Ok(cmd) = command_rx.recv() {
            match cmd {
                PrefetchCommand::Batch(req) => {
                    let current_epoch = epoch.current();
                    if req.epoch < current_epoch {
                        continue; // Stale request
                    }

                    let result_tx = result_tx.clone();
                    let epoch_ref = Arc::clone(&epoch);
                    let request_epoch = req.epoch;

                    pool.install(|| {
                        req.paths.par_iter().for_each(|path| {
                            // Check epoch before processing
                            if epoch_ref.current() > request_epoch {
                                return; // Cancelled
                            }

                            // Process image using shared function from ImageWorker
                            if let Some(result) = ImageWorker::process_image(
                                path,
                                req.target,
                                req.fit_mode,
                                req.kgp_id,
                                req.is_tmux,
                                req.compress_level,
                                req.tmux_kitty_max_pixels,
                                req.resize_filter,
                            ) {
                                // Check epoch again before sending
                                if epoch_ref.current() <= request_epoch {
                                    let _ = result_tx.send((request_epoch, result));
                                }
                            }
                        });
                    });
                }
                PrefetchCommand::Shutdown => break,
            }
        }
    }
}

impl Drop for PrefetchWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.send(PrefetchCommand::Shutdown);
    }
}
