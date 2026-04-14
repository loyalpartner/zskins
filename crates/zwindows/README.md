# zwindows

Wayland client wrapper for `zwlr_foreign_toplevel_manager_v1` — list and
focus open windows on wlroots-based compositors (Sway, Hyprland, niri, …).

Exposes an event stream of `Added` / `Updated` / `Removed` toplevels plus a
handle map so callers can `activate()` a window without touching the event
loop. UI-agnostic; intended for zofi's window-switcher mode and future
zbar reuse.

Run the smoke example from a Wayland session:
`RUST_LOG=info cargo run -p zwindows --example dump`.
