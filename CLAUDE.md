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
- Live-install iteration loop: `cargo build --release -p zbar && sudo install -m 755 target/release/zbar /usr/bin/zbar && pkill -x zbar; nohup env RUST_LOG=zbar=debug /usr/bin/zbar >/tmp/zbar.log 2>&1 & disown` — then `grep -E "pattern" /tmp/zbar.log` to inspect.

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
- Multi-bar shared state: resources coordinating with the OS (wayland handles, DBus SNI host) must be single-instance. Pattern: create `Entity<T>` once in `main.rs`, clone into each Bar; or spawn the session via `std::sync::Once` on first `run()` and broadcast events to per-bar sinks (see `ExtWorkspaceBackend`). Per-bar instantiation of these will silently break after the first bar.
- Generalized: any module opening an external IPC channel (DBus, sway socket, `niri msg` subprocess, wayland protocol handle) must be a single per-process instance. Create the `Entity<T>` once in `main.rs` and clone into each `Bar`. Per-bar instantiation spawns N copies of the connection/subprocess and usually misroutes events.
- niri IPC: `niri msg --json event-stream` is a line-delimited JSON event stream (events include `WindowsChanged`, `WindowOpenedOrChanged`, `WindowFocusChanged`, `WorkspacesChanged`, `WorkspaceActivated`). One-shot queries: `niri msg --json focused-window`, `focused-output`, `workspaces`.
- wayland-client `Dispatch` for any event carrying `new_id` MUST include `event_created_child!` — otherwise runtime panic "Missing event_created_child specialization". Covers ext_workspace_manager_v1, data_device, etc.
- wayland-client proxies are `Send + Sync`; call request methods from any thread. Don't funnel requests through the event-loop thread via a mutex — `blocking_dispatch` won't wake on the outside change.
- GPUI globals: `cx.refresh_windows()` only marks windows dirty — it does NOT re-render child Entities. Every Entity that depends on a `Global` must register `cx.observe_global::<T>(|_, cx| cx.notify()).detach()` in its `new()`, otherwise `cx.set_global` is silent for that subtree.
- Theme: shared via `crates/ztheme/` (16-token `Theme` as gpui Global, Catppuccin Mocha/Latte presets, atomic toml IO + `notify` watcher). Per-crate `theme.rs` only keeps product-specific tokens (BAR_HEIGHT, PANEL_W, kind_*, kbd_*, category()). Config: `$XDG_CONFIG_HOME/zskins/config.toml` with `[theme] name = "..."`.
- Cross-thread global propagation (GPUI main → background): bridge via `Arc<RwLock<T>>` for state + bounded `async_channel<()>` for signal. Background thread reads state on signal; bounded channel + dedup on writer means dropped signals are harmless. See `crates/zbar/src/modules/tray.rs` `FgHexState` for a worked example.
- GPUI source for API lookup (no public docs): `~/.cargo/git/checkouts/zed-*/crates/gpui/src/{app.rs,window.rs,app/context.rs}`
- Click-outside dismissal for layer-shell popups: open one transparent catcher per `cx.displays()` (`Layer::Top` + four-way anchor + `exclusive_zone: Some(px(-1.0))` + `Bounds::maximized(Some(d_id), cx)`); place the popup itself on `Layer::Overlay` so its body eats clicks before the catcher. Catcher's `on_mouse_down` sends a Close message; the owner closes popup + all catchers together. See `crates/zbar/src/modules/tray_menu.rs::open_menu_popup`.
- Sharing a single `Entity<T>` across multiple `Bar`s but needing per-bar context (e.g. `DisplayId` for popup placement): give the shared entity a `set_render_display(...)` setter, call it from each `Bar::render` *before* painting the child, and have the child's event closures capture a local copy. GPUI rebuilds the element tree (and its closures) every render, so each bar's hitboxes carry the correct value.
- Disable hover tooltips on bar items while a popup is open: GPUI tooltips paint in the *bar's* window (not a separate surface) and aren't z-ordered under a new layer-shell popup. Track an "is open" flag, skip `.tooltip(...)` registration while true, and `cx.notify()` on both open and close.

## Gotchas
- GPUI `.cached()` API requires explicit size styles (e.g. `size_full()`); content-sized views collapse
- `/sys` (sysfs) does not reliably support inotify — use polling for brightness/battery
- xkbcommon Compose warnings suppressed via `XKB_COMPOSE_DISABLE=1` (set before threads spawn)
- `std::env::set_var` is unsound in multi-threaded contexts — call at top of main()
- GPUI's idle CPU baseline is ~2% due to Wayland event loop + wgpu swapchain
- Per-toplevel capture (`ext_image_copy_capture`) with fractional scale (e.g. scale=1.5) causes visible window blur — sway bug, no code workaround; scale=1 works fine
- `ext_foreign_toplevel_list_v1` does NOT report XWayland windows (WeChat, Feishu); only `zwlr_foreign_toplevel_manager_v1` sees them — handle types are incompatible between the two protocols
- Per-toplevel capture is sequential (~150ms/window) — budget enough timeout (currently 5s) unlike the old whole-screen screencopy (~100ms total)
- GPUI `DisplayId` and our backend's `wl_output` are on separate wayland connections — protocol IDs and enumeration order differ. Match by UUID: `display.uuid()` returns `Uuid::new_v5(NAMESPACE_DNS, name.as_bytes())`; compute the same in backend from `wl_output.name` (v4+, bind with `version.min(4)`).
- niri uses ext-workspace-v1 with per-output groups; workspace `name` is the idx string ("1"…"N"). `$XDG_CURRENT_DESKTOP=niri`. `niri msg --json workspaces` dumps per-output state for debugging.
- Multi-output ext-workspace: same workspace "name" exists in each group. Key handles by `(name, output)`, not `name` alone, and track `ExtWorkspaceGroupHandleV1::OutputEnter` + `WorkspaceEnter` to assemble the mapping.
- `SWAYSOCK` env var often lingers from a previous sway session pointing at a dead socket. `env::var("SWAYSOCK").is_err()` is NOT enough — actually `UnixStream::connect(&path)` to verify before using sway IPC, otherwise fall through to the next backend.
- GPUI's layer-shell window never knows its display: `Window::display(&App)` returns `None` because `wayland::window::state.display` is only set in the legacy `wl_surface` scale path (`primary_output_scale`); modern protocols use `PreferredBufferScale` and skip it. Track the bar's `DisplayId` yourself and route it through the entity tree — don't rely on `window.display(cx)` inside listeners.
- GPUI's wayland `handle_layersurface_event` treats `Configure { width: 0, height: 0, .. }` as `None` and skips `resize()`, leaving the viewport at the initial bounds. For full-screen overlays seed `WindowBounds::Windowed(Bounds::maximized(Some(d_id), cx))`. Don't pass `(0, 0)` expecting the compositor to fill in size; don't pass `(1, 1)` as a stub either — sway honors it literally.
- GPUI's wayland backend ignores `wl_output.geometry.transform`, so `display.bounds()` reports raw mode size on rotated outputs. For fixed-size popups, use a single-edge anchor + `margin` and let the compositor handle rotated coordinates — never compute screen-relative positions from `display.bounds()`.
- sway briefly grants then immediately revokes keyboard focus on a freshly mapped `OnDemand` layer-shell surface (Enter+Leave within ~100µs); `observe_window_activation` is therefore unusable as a "user clicked away" signal. Use a catcher overlay instead.
- wlr-layer-shell `exclusive_zone`: `Some(px(-1.0))` = "cover the whole output, ignore other surfaces' reserved space" (right for full-screen catchers that need to overlap the bar). `None`/`0` = "don't reserve space *and avoid others' reservations*" — the opposite of what an overlay wants.

## Worktree & Git
- Root disk is tight; when running agents in git worktrees share target dir: `export CARGO_TARGET_DIR="$(git rev-parse --show-toplevel)/target"` (run from the main repo, before entering the worktree)
- Commits are auto-pushed via a hook; `git status` shows "up to date with origin" right after commit — no manual `git push` needed. Caveat: the hook does NOT push tags, and occasionally misses merge commits — push those explicitly (`git push origin <tag>`, `git push`).
