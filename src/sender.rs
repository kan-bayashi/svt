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
    #[allow(dead_code)]
    /// Place a previously transmitted image in the terminal area.
    ImagePlace {
        area: Rect,
        kgp_id: u32,
        old_area: Option<Rect>,
        epoch: u64,
    },
    /// Clear any KGP overlays (used on shutdown).
    ClearAll {
        area: Option<Rect>,
        is_tmux: bool,
    },
    /// Cancel an in-flight image task (best-effort).
    CancelImage {
        kgp_id: Option<u32>,
        is_tmux: bool,
        area: Option<Rect>,
        epoch: u64,
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
    PlaceDone { kgp_id: u32 },
}

struct Task {
    chunks: VecDeque<Vec<u8>>,
    complete: Option<WriterResultKind>,
    epoch: u64,
    clears_dirty: bool,
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

        let mut last_status: Option<(String, (u16, u16), StatusIndicator)> = None;
        let mut status_dirty = false;
        let mut current_task: Option<Task> = None;
        let mut current_epoch: u64 = 0;
        let mut should_quit = false;
        let mut dirty_area: Option<Rect> = None;
        let mut bytes_since_flush: usize = 0;
        const FLUSH_THRESHOLD: usize = 64 * 1024;

        loop {
            if should_quit {
                break;
            }

            if current_task.is_none() && !status_dirty {
                match request_rx.recv() {
                    Ok(msg) => Self::apply_msg(
                        msg,
                        &mut should_quit,
                        &mut last_status,
                        &mut status_dirty,
                        &mut current_task,
                        is_tty,
                        &mut out,
                        &mut current_epoch,
                        &mut dirty_area,
                    ),
                    Err(_) => break,
                }
            }

            while let Ok(msg) = request_rx.try_recv() {
                Self::apply_msg(
                    msg,
                    &mut should_quit,
                    &mut last_status,
                    &mut status_dirty,
                    &mut current_task,
                    is_tty,
                    &mut out,
                    &mut current_epoch,
                    &mut dirty_area,
                );
                if should_quit {
                    break;
                }
            }

            if status_dirty {
                if let Some((text, size, indicator)) = last_status.clone() {
                    if is_tty {
                        let _ = Self::render_status(&mut out, &text, size, indicator);
                        let _ = out.flush();
                    }
                    bytes_since_flush = 0;
                }
                status_dirty = false;
            }

            if let Some(task) = &mut current_task {
                if task.epoch != current_epoch {
                    current_task = None;
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
                        dirty_area = None;
                    }
                    current_task = None;
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
                        dirty_area = None;
                    }
                    current_task = None;
                }
            }
        }
    }

    fn apply_msg(
        msg: WriterRequest,
        should_quit: &mut bool,
        last_status: &mut Option<(String, (u16, u16), StatusIndicator)>,
        status_dirty: &mut bool,
        current_task: &mut Option<Task>,
        is_tty: bool,
        out: &mut impl Write,
        current_epoch: &mut u64,
        dirty_area: &mut Option<Rect>,
    ) {
        match msg {
            WriterRequest::Shutdown => {
                *should_quit = true;
            }
            WriterRequest::Status {
                text,
                size,
                indicator,
            } => {
                *last_status = Some((text, size, indicator));
                *status_dirty = true;
            }
            WriterRequest::ClearAll { area, is_tmux } => {
                // Preempt current image work.
                *current_task = None;
                *dirty_area = None;
                if is_tty {
                    let _ = Self::clear_all(out, area, is_tmux);
                    let _ = out.flush();
                }
            }
            WriterRequest::CancelImage {
                kgp_id,
                is_tmux,
                area,
                epoch,
            } => {
                if epoch >= *current_epoch {
                    *current_epoch = epoch;
                    *current_task = None;
                }
                if let Some(cancel_area) = area {
                    let next = match dirty_area.take() {
                        Some(prev) => union_rect(prev, cancel_area),
                        None => cancel_area,
                    };
                    *dirty_area = Some(next);
                }
                if is_tty {
                    // Do not delete ids here: it's racy (the transmit may already have completed)
                    // and can lead to "Ready but not displayed" until a resize forces a re-send.
                    //
                    // Cancellation is best-effort: stop writing further chunks so status updates
                    // remain responsive.
                    //
                    // Note: We do NOT erase here. Erasing on cancel causes blank screen when
                    // navigation resumes before a new ImagePlace is sent (navigation latch).
                    // Instead, task_place/task_transmit always erase old_area to clean up.
                    let _ = kgp_id;
                    let _ = is_tmux;
                    let _ = area;
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
                if epoch < *current_epoch {
                    return;
                }
                *current_epoch = epoch;
                let cleanup_area = *dirty_area;
                *current_task = Some(Self::task_transmit(
                    encoded_chunks,
                    area,
                    kgp_id,
                    old_area,
                    cleanup_area,
                    epoch,
                    is_tmux,
                ));
            }
            WriterRequest::ImagePlace {
                area,
                kgp_id,
                old_area,
                epoch,
            } => {
                if epoch < *current_epoch {
                    return;
                }
                *current_epoch = epoch;
                let cleanup_area = *dirty_area;
                *current_task = Some(Self::task_place(
                    area,
                    kgp_id,
                    old_area,
                    cleanup_area,
                    epoch,
                ));
            }
        }
    }

    fn task_place(
        area: Rect,
        kgp_id: u32,
        old_area: Option<Rect>,
        dirty_area: Option<Rect>,
        epoch: u64,
    ) -> Task {
        let mut chunks = VecDeque::new();

        // Step 1: Erase old area FIRST (yazi pattern: hide -> show)
        if let Some(old) = old_area {
            for row in erase_rows(old) {
                chunks.push_back(row);
            }
        }
        for cleanup in Self::cleanup_rects(area, None, dirty_area) {
            for row in erase_rows(cleanup) {
                chunks.push_back(row);
            }
        }

        // Step 2: Place new image
        for row in place_rows(area, kgp_id) {
            chunks.push_back(row);
        }

        Task {
            chunks,
            complete: Some(WriterResultKind::PlaceDone { kgp_id }),
            epoch,
            clears_dirty: dirty_area.is_some(),
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
        for cleanup in Self::cleanup_rects(area, None, dirty_area) {
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

    fn cleanup_rects(
        area: Rect,
        old_area: Option<Rect>,
        dirty_area: Option<Rect>,
    ) -> Vec<Rect> {
        let mut out = Vec::new();
        if let Some(old) = old_area {
            out.extend(rect_diff(old, area));
        }
        if let Some(dirty) = dirty_area {
            out.extend(rect_diff(dirty, area));
        }
        out
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

        let row_1based = h;
        // Reserve 2 columns for "● " prefix.
        let available = w.saturating_sub(2);
        let clipped = clip_utf8(status_text, available as usize);

        // Background first, then ECH so the cleared cells inherit the background.
        write!(out, "\x1b[{row_1based};1H\x1b[37;100m\x1b[{w}X")?;
        write!(out, "\x1b[{row_1based};1H")?;
        match indicator {
            StatusIndicator::Ready => write!(out, "\x1b[32m●")?, // green
            StatusIndicator::Busy => write!(out, "\x1b[31m●")?,  // red
        }
        write!(out, "\x1b[37;100m {clipped}\x1b[0m")?;
        Ok(())
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

    Rect::new(
        x0 as u16,
        y0 as u16,
        (x1 - x0) as u16,
        (y1 - y0) as u16,
    )
}
