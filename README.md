# SVT

**S**imple **V**iewer in **T**erminal

A minimal & fast terminal image viewer written in Rust with sxiv-like keybindings. Works over SSH with Tmux.

![](./assets/demo.gif)

## Features

- **Fast** - Zlib compression, prefetch, and render cache for instant navigation
- **Keyboard-driven** - sxiv/vim-like keybindings with count support
- **Flexible** - Fit/Normal display modes, works over SSH with Tmux
- **KGP** - Kitty Graphics Protocol for high-quality image rendering

## Requirements

**Supported Terminals:**
- [Ghostty](https://ghostty.org/) (Recommended)
- [Kitty](https://sw.kovidgoyal.net/kitty/)
- [WezTerm](https://wezfurlong.org/wezterm/)
- Other terminals with [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) support

**tmux:** Works with tmux. Passthrough is enabled automatically.

**Building from source:** Rust 1.75+ required.

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

## Configuration

Settings can be configured via config file or environment variables.

**Priority:** Environment variables > Config file > Defaults

### Config File

Create `~/.config/svt/config.toml`:

```toml
nav_latch_ms = 150
render_cache_size = 100
prefetch_count = 5
compress_level = 6
```

### Options

| Config Key | Env | Default | Description |
|------------|-----|---------|-------------|
| `nav_latch_ms` | `SVT_NAV_LATCH_MS` | `150` | Navigation latch (ms) before drawing images |
| `render_cache_size` | `SVT_RENDER_CACHE_SIZE` | `100` | Render cache entries |
| `prefetch_count` | `SVT_PREFETCH_COUNT` | `5` | Number of images to prefetch ahead/behind |
| `compress_level` | `SVT_COMPRESS_LEVEL` | `6` | Zlib compression level 0-9 |
| `kgp_no_compress` | `SVT_KGP_NO_COMPRESS` | `false` | Disable zlib compression |
| `tmux_kitty_max_pixels` | `SVT_TMUX_KITTY_MAX_PIXELS` | `2000000` | Max pixels in `Normal` mode (tmux+kitty) |
| `force_alt_screen` | `SVT_FORCE_ALT_SCREEN` | `false` | Force alternate screen |
| `no_alt_screen` | `SVT_NO_ALT_SCREEN` | `false` | Disable alternate screen |
| `debug` | `SVT_DEBUG` | `false` | Show debug info in status bar |
| `trace_worker` | `SVT_TRACE_WORKER` | `false` | Write worker timing logs to `/tmp/svt_worker.log` |

## Contributing

See `CONTRIBUTING.md`.

## References

- [yazi](https://github.com/sxyazi/yazi) - Kitty Graphics Protocol implementation reference
- [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)

## License

MIT
