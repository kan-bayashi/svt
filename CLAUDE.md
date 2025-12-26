# svt - Project Guide

## Overview

SVT (Simple Viewer in Terminal) - A terminal-based image viewer with sxiv-like keybindings.

## Tech Stack

- Language: Rust (Edition 2024)
- TUI Framework: ratatui + ratatui-image
- Terminal Backend: crossterm
- Image Processing: image crate
- CLI: clap (derive)

## Project Structure

```
svt/
├── Cargo.toml
├── docs/
│   └── architecture.md
├── src/
│   ├── main.rs    # Entry point, CLI parsing, event loop
│   ├── app.rs     # App state, navigation, cache orchestration
│   ├── fit.rs     # Fit mode (Normal/Fit)
│   ├── kgp.rs     # Kitty Graphics Protocol helpers (encode/place/erase)
│   ├── sender.rs  # TerminalWriter (single stdout writer, status priority, cancel)
│   └── worker.rs  # ImageWorker (decode/resize/encode)
```

## Development Commands

```bash
cargo build          # Build
cargo run            # Run
cargo test           # Test
cargo fmt            # Format
cargo clippy         # Lint
```

## Keybindings

- `q` - Quit
- `j` / `Space` / `l` - Next image
- `k` / `Backspace` / `h` - Previous image
- `g` - First image
- `G` - Last image
- `f` - Toggle fit
- `r` - Reload (clear cache)
- Counts supported (e.g. `5j`, `10G`)

## Environment Variables

- `SVT_NAV_LATCH_MS` - Navigation latch (ms) before drawing images (default: 150)
- `SVT_RENDER_CACHE_SIZE` - Client-side render cache size (encoded images in memory, default: 100)
- `SVT_TMUX_KITTY_MAX_PIXELS` - Max pixels for tmux+kitty in `Normal` mode (default: 2000000)
- `SVT_FORCE_ALT_SCREEN` - Force alternate screen mode
- `SVT_NO_ALT_SCREEN` - Disable alternate screen mode
- `SVT_DEBUG` - Enable debug info in status bar
- `SVT_TRACE_WORKER` - Write worker timing logs to `/tmp/svt_worker.log`
- `SVT_KGP_NO_COMPRESS` - Disable zlib compression for KGP transmission
- `SVT_COMPRESS_LEVEL` - Zlib compression level 0-9 (default: 6, higher = smaller but slower)
- `SVT_PREFETCH_COUNT` - Number of images to prefetch ahead/behind (default: 5)

## Coding Conventions

- Follow Rust standard naming conventions (snake_case for functions/variables, PascalCase for types)
- Use `anyhow::Result` for error handling
- Keep functions small and focused
- Write tests for public functions

### Critical Invariants

- **stdout is written by `TerminalWriter` only** (`src/sender.rs`)
- **Image output must be chunked at safe boundaries**
  - KGP chunk boundaries for transmit (`encode_chunks`)
  - per-row boundaries for placement/erase (`place_rows` / `erase_rows`)
- **Navigation must stay responsive**
  - cancel in-flight image output on navigation (only when not transmitting)
  - avoid blocking the main loop on decode/encode or stdout I/O
- **Single KGP ID per process**
  - `delete_by_id` before each transmit to clear terminal-side cache
  - prevents "wrong image" and "blank screen" issues
- **Transmit must complete once started**
  - skip cancellation during active transmission (`is_transmitting()`)
  - ensures terminal receives complete image data

## Architecture Notes

- See `docs/architecture.md`.

## Contributing

See `CONTRIBUTING.md`.

## References

- [ratatui-image](https://github.com/benjajaja/ratatui-image)
- [ratatui](https://ratatui.rs/)
- [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
