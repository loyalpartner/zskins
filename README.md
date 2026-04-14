# zskins

A small suite of Wayland desktop components — a status bar and a launcher — designed to feel like a single, cohesive shell rather than a pile of independent widgets.

Built on [GPUI](https://github.com/zed-industries/zed), the GPU-accelerated UI framework behind the Zed editor.

## What's in the box

### zbar — status bar

A minimal, fast top bar for wlroots-based Wayland compositors. It shows the things you actually look at:

- Clock
- Workspaces (click to switch)
- Focused window title
- Volume, brightness, battery
- Network status
- CPU and memory

Workspace and window info come from the compositor directly — Sway over IPC, or any wlroots compositor via `ext-workspace-v1`.

### zofi — launcher

A keyboard-first, rofi-style launcher with multiple sources in one window:

- **Apps** — launch installed desktop applications.
- **Clipboard** — search and paste from your clipboard history.
- **Files** — browse and open files, with icons and syntax-highlighted previews.

One shortcut opens it; arrow keys and typing do the rest.

### zofi clipd — clipboard history

A lightweight background service that remembers what you've copied, so the launcher's clipboard source has something to search. It runs quietly in your session and stays out of the way.

## Design goals

- **Feel fast.** GPU-rendered, no visible lag when opening the launcher or updating the bar.
- **Stay out of the way.** Sensible defaults, little configuration, no surprises.
- **One aesthetic.** The bar and launcher share a look instead of clashing.
- **Wayland-native.** Uses compositor protocols directly — no X11 fallbacks, no shims.

## Status

Early and personal. It works on the author's setup (Sway on Arch Linux) and is evolving quickly. Expect rough edges.

## Requirements

- A wlroots-based Wayland compositor (Sway, Hyprland, river, …) with `wlr-layer-shell`.
- Running inside a real graphical Wayland session — it's not meant to be launched over SSH.

## License

MIT. See [LICENSE](LICENSE).
