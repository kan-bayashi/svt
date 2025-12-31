# Contributing

Thanks for contributing to `stiv`.

## Quick Start

```bash
cargo build
cargo test
cargo clippy
cargo fmt --check
```

## Project Layout

See [CLAUDE.md](CLAUDE.md#project-structure).

## Architecture & Invariants

See [docs/architecture.md](docs/architecture.md).

## Configuration

See [README.md](README.md#configuration).

For debugging:

| Config | Env | Description |
|--------|-----|-------------|
| `debug` | `STIV_DEBUG` | Show debug info in status bar |
| `trace_worker` | `STIV_TRACE_WORKER` | Write timing logs to `/tmp/stiv_worker.log` |

## Testing Tips

- Test with large images and rapid navigation to verify cancellation behavior.
- Tests should not write escape sequences to stdout.

## Release

Push a tag to trigger GitHub Actions release:

```bash
git tag v25.12.3
git push origin v25.12.3
```

**Versioning:** `YY.MM.PATCH` (e.g., `25.12.2` = December 2025, patch 2)

**Targets:**
- macOS (Apple Silicon): `aarch64-apple-darwin`
- Linux (x86_64): `x86_64-unknown-linux-gnu`
