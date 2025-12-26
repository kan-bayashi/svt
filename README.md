# SVT

**S**imple **V**iewer in **T**erminal

A blazing fast terminal image viewer written in Rust with sxiv-like keybindings. Works over SSH with Tmux.

![](./samples/svt.png)

## Features

- **Fast** - Zlib compression, prefetch, and render cache for instant navigation
- **Keyboard-driven** - sxiv/vim-like keybindings with count support
- **Flexible** - Fit/Normal display modes, works over SSH with Tmux
- **KGP** - Kitty Graphics Protocol for high-quality image rendering

## Requirements

- Kitty Graphics Protocol supported terminal
- Optional: tmux (uses `allow-passthrough=on`, `svt` attempts to set it automatically)
- Rust 1.75+

Tested: Ghostty + tmux.

## Installation

### From Release

Download the latest binary from [Releases](https://github.com/kan-bayashi/svt/releases):

```bash
# macOS (Apple Silicon)
curl -L https://github.com/kan-bayashi/svt/releases/latest/download/svt-aarch64-apple-darwin.tar.gz | tar xz
sudo mv svt /usr/local/bin/

# Linux (x86_64)
curl -L https://github.com/kan-bayashi/svt/releases/latest/download/svt-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo mv svt /usr/local/bin/
```

### From Source

```bash
cargo install --path .
```

## Usage

```bash
svt image.png
svt ~/photos/
svt *.png
svt ~/photos/*.jpg
```

## Keybindings

| Key | Action |
|-----|--------|
| `j` / `Space` / `l` | Next image |
| `k` / `Backspace` / `h` | Previous image |
| `g` | First image |
| `G` | Last image |
| `f` | Toggle fit |
| `r` | Reload (clear cache) |
| `q` | Quit |

Vim-like counts are supported (e.g. `5j`, `10G`).

## Options

| Env | Default | Description |
|-----|---------|-------------|
| `SVT_NAV_LATCH_MS` | `150` | Navigation latch (ms) before drawing images |
| `SVT_RENDER_CACHE_SIZE` | `100` | Render cache entries |
| `SVT_PREFETCH_COUNT` | `5` | Number of images to prefetch ahead/behind |
| `SVT_COMPRESS_LEVEL` | `6` | Zlib compression level 0-9 |
| `SVT_KGP_NO_COMPRESS` | unset | Disable zlib compression |
| `SVT_TMUX_KITTY_MAX_PIXELS` | `2000000` | Max pixels in `Normal` mode (tmux+kitty) |
| `SVT_FORCE_ALT_SCREEN` | unset | Force alternate screen |
| `SVT_NO_ALT_SCREEN` | unset | Disable alternate screen |

## Contributing

See `CONTRIBUTING.md`.

## References

- [yazi](https://github.com/sxyazi/yazi) - Kitty Graphics Protocol implementation reference
- [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)

## License

MIT
