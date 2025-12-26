# Architecture

`svt` is a terminal image viewer built around Kitty Graphics Protocol (KGP).
The core goal is: keep navigation/status updates responsive even when image rendering or terminal I/O is slow.

## High-level pipeline

There are three concurrent “lanes”:

1. **Main thread** (`src/main.rs`)
   - Reads key events.
   - Updates application state.
   - Decides when to request rendering.
   - Sends status updates.

2. **Worker thread** (`src/worker.rs`)
   - Decodes the image file.
   - Resizes to a target size based on the current terminal size and `Fit`/`Normal`.
   - Encodes the resized image to KGP chunks (`_G ...`) suitable for sending to the terminal.

3. **Terminal writer thread** (`src/sender.rs`)
   - The only component allowed to write to stdout.
   - Prioritizes status updates over image output.
   - Writes image output in “safe boundaries” (KGP chunk boundaries and per-row placement).

## Why a single stdout writer exists

Terminal output is a single ordered stream. If multiple threads write to stdout:

- escape sequences can interleave and corrupt the screen
- cursor/save/restore can be violated
- large image writes can block unrelated status updates

`TerminalWriter` centralizes output, so status writes can preempt image writes safely.

## Output boundaries and preemption

Image output is chunked so the writer can yield between boundaries:

- **Transmit**: KGP encode is split into multiple independent escape sequences (`encode_chunks`).
- **Place / erase**: generated per terminal row (`place_rows` / `erase_rows`).

This allows the writer to:

- flush the status row immediately
- continue image output incrementally

## Cancellation

When the user navigates while an image transmission is in-flight:

- **If not transmitting**: the main thread sends `CancelImage` to the writer, dropping the current task.
- **If transmitting**: cancellation is skipped to ensure the terminal receives complete image data.

This design prevents "blank screen" issues that occur when image data is partially transmitted.

## KGP ID Strategy

`svt` uses a single KGP ID per process (inspired by Yazi):

- The ID is generated at startup based on the process ID.
- RGB components are ensured to be >= 16 to avoid terminal color quantization issues.
- Before each transmit, `delete_by_id` clears any existing image data for this ID.

This approach:
- Avoids "wrong image" issues from stale terminal-side cache.
- Simplifies cache management (no per-image ID tracking needed).

## Transmit Sequence

1. **Erase** old placement area (if any).
2. **Delete** existing image data for this ID (`delete_by_id`).
3. **Transmit** new image data (`encoded_chunks`).
4. **Place** the image using Unicode placeholders.

## Caching

`svt` uses a **client-side render cache** only:

- **Render cache** (`render_cache` in `App`): Stores decoded/resized/encoded image data.
- Size controlled by `SVT_RENDER_CACHE_SIZE` (default: 15).
- LRU eviction when cache is full.

The terminal-side cache is **not** relied upon. Each transmit starts with `delete_by_id` to ensure a clean slate. This trades some bandwidth for simplicity and correctness.
