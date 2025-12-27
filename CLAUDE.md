# svt - Project Guide

## Overview

SVT (Simple Viewer in Terminal) - A terminal-based image viewer with sxiv-like keybindings.

## Tech Stack

- Language: Rust (Edition 2024)
- TUI Framework: ratatui + ratatui-image
- Terminal Backend: crossterm
- Image Processing: image crate
- CLI: clap (derive)
- Config: serde + toml + dirs

## Project Structure

```
svt/
├── Cargo.toml
├── docs/
│   └── architecture.md
├── src/
│   ├── main.rs    # Entry point, CLI parsing, event loop
│   ├── app.rs     # App state, navigation, cache orchestration, tile grid
│   ├── config.rs  # Config loading (file + env, priority: env > file > default)
│   ├── fit.rs     # Fit mode (Normal/Fit) and View mode (Single/Tile)
│   ├── kgp.rs     # Kitty Graphics Protocol helpers (encode/place/erase)
│   ├── sender.rs  # TerminalWriter (single stdout writer, status priority, tile cursor)
│   └── worker.rs  # ImageWorker (decode/resize/encode, tile composite)
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

See [README.md](README.md#keybindings).

## Configuration

Settings: `~/.config/svt/config.toml` or environment variables (`SVT_*`).

**Priority:** Environment variables > Config file > Defaults

See [README.md](README.md#configuration) for all options.

## Coding Conventions

- Follow Rust standard naming conventions (snake_case for functions/variables, PascalCase for types)
- Use `anyhow::Result` for error handling
- Keep functions small and focused
- Write tests for public functions

## Architecture & Invariants

See [docs/architecture.md](docs/architecture.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## References

- [ratatui-image](https://github.com/benjajaja/ratatui-image)
- [ratatui](https://ratatui.rs/)
- [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
