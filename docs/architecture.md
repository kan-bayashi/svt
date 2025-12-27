# Architecture

`svt` is a terminal image viewer built around Kitty Graphics Protocol (KGP).
The core goal is: keep navigation/status updates responsive even when image rendering or terminal I/O is slow.

## High-level pipeline

There are three concurrent "lanes":

1. **Main thread** (`src/main.rs`)
   - Loads configuration (`src/config.rs`).
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
   - Writes image output in "safe boundaries" (KGP chunk boundaries and per-row placement).

## View Modes

`svt` supports two view modes:

### Single Mode (default)
- Displays one image at a time
- Full-size image with Fit/Normal display options
- Navigation: `h/j/k/l` moves between images

### Tile Mode
- Displays multiple images as a grid of thumbnails
- Grid size is calculated from terminal dimensions and `cell_aspect_ratio`
- Cursor navigation within the grid
- Press `t` to toggle between modes

### Tile Rendering Architecture

Tile mode uses a **composite image approach**:

1. **Worker thread** (`src/worker.rs`):
   - Decodes all images for the current page
   - Resizes each to fit a tile cell (with padding)
   - Composites all tiles onto a single canvas
   - Encodes the composite as a single KGP image

2. **Cursor overlay** (`src/sender.rs`):
   - Cursor border is drawn using ANSI escape sequences
   - Separate from the composite image for fast cursor movement
   - Unicode box-drawing characters (┌─┐│└┘) in cyan color

This design ensures:
- Fast cursor movement (no image re-render needed)
- Single KGP ID maintained (existing architecture preserved)
- Efficient caching (cursor position not in cache key)

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
- Size controlled by `render_cache_size` config (default: 100).
- LRU eviction when cache is full.

The terminal-side cache is **not** relied upon. Each transmit starts with `delete_by_id` to ensure a clean slate. This trades some bandwidth for simplicity and correctness.

## Configuration

Settings are loaded at startup by `Config::load()` (`src/config.rs`):

1. Load from `~/.config/svt/config.toml` (if exists).
2. Override with environment variables (`SVT_*`).
3. Apply defaults for missing values.

**Priority:** Environment variables > Config file > Defaults

The `Config` struct is passed to `App` and propagated to worker requests as needed.

### Tile Mode Settings

| Key | Default | Description |
|-----|---------|-------------|
| `cell_aspect_ratio` | `2.0` | Terminal cell height/width ratio for square tiles |

## Invariants

These invariants must be preserved when modifying the codebase:

1. **stdout via `TerminalWriter` only** (`src/sender.rs`)
   - No other component may write to stdout directly.

2. **Image output chunked at safe boundaries**
   - KGP chunk boundaries for transmit (`encode_chunks`)
   - Per-row boundaries for placement/erase (`place_rows` / `erase_rows`)

3. **Navigation stays responsive**
   - Cancel in-flight image output on navigation (except during transmit)
   - Avoid blocking the main loop on decode/encode or stdout I/O

4. **Single KGP ID per process**
   - `delete_by_id` before each transmit to clear terminal-side cache
   - Prevents "wrong image" and "blank screen" issues

5. **Transmit must complete once started**
   - Skip cancellation during active transmission (`is_transmitting()`)
   - Ensures terminal receives complete image data

6. **Tile cursor via ANSI overlay**
   - Cursor is drawn separately from tile composite
   - Cursor movement does not trigger image re-render
   - Only page changes invalidate tile cache

## Clipboard Support

`svt` provides two clipboard copy methods via `y` and `Y` keys:

### Path Copy (`y` key)

Uses **OSC 52** escape sequence to copy the image path as text:

- Format: `\x1b]52;c;<base64>\x07`
- Works over SSH (terminal interprets the sequence locally)
- Tmux-aware: wraps in `\x1bPtmux;...\x1b\\` when `$TMUX` is set

Implementation: `WriterRequest::CopyToClipboard` in `src/sender.rs`

### Image Copy (`Y` key)

Uses **arboard** crate to copy image data via OS clipboard API:

- Converts image to RGBA and sends to system clipboard
- Works on local machine and X11-forwarded SSH sessions
- Does NOT work on headless SSH (no display server)

Implementation: `copy_image_to_clipboard()` in `src/app.rs`
