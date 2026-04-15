# zskins

Wayland status bar built on GPUI (Zed's UI framework). Cargo workspace.

## Structure
- `crates/zbar/` — main status bar crate (binary + library)
- `crates/zofi/` — keyboard-first launcher (like rofi), multi-source search
- `crates/zwindows/` — Wayland client for toplevel window management and capture
- Modules: clock, workspaces, window_title, volume, network, brightness, battery, cpu_mem
- Backends: sway (IPC), ext-workspace-v1 (Wayland protocol)

## Build & Test
- `cargo check` — fast compile check
- `cargo clippy -- -D warnings` — lint (treat warnings as errors)
- `cargo clippy --all-targets` fails on preexisting `bool_assert_comparison` in `tests/sway_parse.rs` — unrelated; use the non-`--all-targets` form for lib/bin lints
- `cargo fmt --check` — format check
- `cargo test` — run tests (integration tests in `crates/zbar/tests/`)
- `cargo build --release` — release build (~5 min, LTO enabled)
- Release binary: `target/release/zbar`

## Running
- Must run from a Wayland graphical session (needs WAYLAND_DISPLAY)
- Cannot be launched from a headless/SSH shell — GPUI will panic
- `RUST_LOG=info target/release/zbar` — run with logging
- `RUST_LOG=debug` for verbose output including workspace click events

## Key Patterns
- Errors: `thiserror` for typed errors (not `anyhow`). Define per-module error enums with `#[derive(thiserror::Error)]`.
- Logging: `tracing` crate (not `log`). Use `tracing::info!`, `tracing::warn!`, etc.
- Async timers: `cx.background_executor().timer(Duration).await` (not `std::thread::sleep`)
- Event channels: `async_channel::bounded` (not unbounded) to prevent memory growth
- Module updates: only call `cx.notify()` when state actually changed
- Backends use blocking I/O on `background_executor().spawn()` threads — `std::thread::sleep` is acceptable there
- Volume uses `pactl subscribe` for event-driven updates (not polling)
- DBus property reads: use `PropertiesProxy.get` / `get_all` directly instead of zbus cached accessors — avoids stale values during signal handling (`NewIcon`/`NewStatus`/`NewToolTip`)
- Multi-property DBus fetches: prefer one `Properties.GetAll(interface)` over N separate `Get` calls on the same object
- Wayland protocol objects: always explicitly call `.destroy()` before dropping the connection — proxy `Drop` does NOT send destroy requests; compositor may retain rendering state
- Wayland capture: `cargo run --example capture -p zwindows` to test per-toplevel capture without starting zofi
- wayland-protocols crate: ext staging protocols live under `wayland_protocols::ext::` with `staging` feature flag (already enabled in workspace)

## Gotchas
- GPUI `.cached()` API requires explicit size styles (e.g. `size_full()`); content-sized views collapse
- `/sys` (sysfs) does not reliably support inotify — use polling for brightness/battery
- xkbcommon Compose warnings suppressed via `XKB_COMPOSE_DISABLE=1` (set before threads spawn)
- `std::env::set_var` is unsound in multi-threaded contexts — call at top of main()
- GPUI's idle CPU baseline is ~2% due to Wayland event loop + wgpu swapchain
- Per-toplevel capture (`ext_image_copy_capture`) with fractional scale (e.g. scale=1.5) causes visible window blur — sway bug, no code workaround; scale=1 works fine
- `ext_foreign_toplevel_list_v1` does NOT report XWayland windows (WeChat, Feishu); only `zwlr_foreign_toplevel_manager_v1` sees them — handle types are incompatible between the two protocols
- Per-toplevel capture is sequential (~150ms/window) — budget enough timeout (currently 5s) unlike the old whole-screen screencopy (~100ms total)

## Worktree & Git
- Root disk is tight; when running agents in git worktrees share target dir: `export CARGO_TARGET_DIR="$(git rev-parse --show-toplevel)/target"` (run from the main repo, before entering the worktree)
- Commits are auto-pushed via a hook; `git status` shows "up to date with origin" right after commit — no manual `git push` needed
