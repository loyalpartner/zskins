# zskins

Wayland status bar built on GPUI (Zed's UI framework). Cargo workspace.

## Structure
- `crates/zbar/` — main status bar crate (binary + library)
- Modules: clock, workspaces, window_title, volume, network, brightness, battery, cpu_mem
- Backends: sway (IPC), ext-workspace-v1 (Wayland protocol)

## Build & Test
- `cargo check` — fast compile check
- `cargo clippy -- -D warnings` — lint (treat warnings as errors)
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
- Logging: `tracing` crate (not `log`). Use `tracing::info!`, `tracing::warn!`, etc.
- Async timers: `cx.background_executor().timer(Duration).await` (not `std::thread::sleep`)
- Event channels: `async_channel::bounded` (not unbounded) to prevent memory growth
- Module updates: only call `cx.notify()` when state actually changed
- Backends use blocking I/O on `background_executor().spawn()` threads — `std::thread::sleep` is acceptable there
- Volume uses `pactl subscribe` for event-driven updates (not polling)

## Gotchas
- GPUI `.cached()` API requires explicit size styles (e.g. `size_full()`); content-sized views collapse
- `/sys` (sysfs) does not reliably support inotify — use polling for brightness/battery
- xkbcommon Compose warnings suppressed via `XKB_COMPOSE_DISABLE=1` (set before threads spawn)
- `std::env::set_var` is unsound in multi-threaded contexts — call at top of main()
- GPUI's idle CPU baseline is ~2% due to Wayland event loop + wgpu swapchain
