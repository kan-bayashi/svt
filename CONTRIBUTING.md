# Contributing

Thanks for contributing to `svt`.

## Quick start

```bash
cargo build
cargo test
cargo clippy --all-targets --all-features
```

## Project layout

- `src/main.rs`: CLI + event loop
- `src/app.rs`: app state, cache orchestration
- `src/worker.rs`: image decode/resize/encode (thread)
- `src/sender.rs`: `TerminalWriter` (single stdout writer, status priority, cancel)
- `src/kgp.rs`: Kitty Graphics Protocol helpers
- `docs/architecture.md`: system overview

## Invariants (please keep)

- stdout must be written by **`TerminalWriter` only** (`src/sender.rs`)
- image output must be chunked at safe boundaries
  - KGP chunk boundaries for transmit (`encode_chunks`)
  - per-row boundaries for placement/erase (`place_rows` / `erase_rows`)
- navigation must stay responsive
  - cancel in-flight image output on navigation
  - avoid blocking the main loop on decode/encode or stdout I/O

## Environment variables (developer)

| Env | Description |
|-----|-------------|
| `SVT_TRACE_WORKER=1` | Write worker timing logs to `/tmp/svt_worker.log` |
| `SVT_DEBUG=1` | Add debug info to status bar |

## Tips

- If you change terminal escape handling, test with a large image and rapid navigation to verify cancellation.
- If tests print escape sequences, ensure the writer treats stdout as non-TTY during tests.

## Release

Push a tag like `v0.1.0` to build and attach binaries to a GitHub Release:

```bash
git tag v0.1.0
git push origin v0.1.0
```

Current workflow targets:

- macOS (Apple Silicon): `aarch64-apple-darwin`
- Linux: `x86_64-unknown-linux-gnu`
