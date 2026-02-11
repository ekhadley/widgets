# widgets

Bespoke Wayland desktop widgets, written from scratch in Rust against sctk + tiny-skia rather than through widget frameworks like eww or quickshell.

## Why

Coding agents have made it practical to write and maintain bespoke native widgets. The schlep of wiring up Wayland protocols, pixel rendering, and input handling by hand used to make frameworks the obvious choice. Now that agents can just do that work, you get the upside of from-scratch widgets — tiny binaries, no runtime overhead, full control over every pixel — without the prohibitive cost of writing all the boilerplate yourself.

## Shared patterns

All widgets follow the same architecture:

- **Rust + smithay-client-toolkit 0.20** for Wayland layer-shell surfaces, pointer/keyboard handling, SHM buffers, calloop event loop
- **tiny-skia** for CPU rendering into the SHM buffer (RGBA pixmap, copied to BGRA for Wayland)
- **cosmic-text** for text shaping and glyph rendering
- **serde + toml** for config files
- **walrs** for colorscheme integration — each widget has a template in `~/.config/walrs/templates/` that generates a `key=value` color file in `~/.cache/wal/`
- Single `src/main.rs` per widget, typically 500-900 lines
- `make install` puts binaries in `~/.local/bin/`
- Config files go in `~/.config/widgets/<name>.toml`
- State files go in `~/.local/state/widgets/<name>/` (one file per widget)

## Widgets

- **wallrun** — image picker overlay (thumbnail grid, fuzzy search, keyboard + mouse nav)
- **panel** — floating status panel (clock, pomodoro timers, volume control, theme toggle)

## Ideas

- **sysinfo** — neofetch/fastfetch-style system info overlay (host, kernel, CPU, RAM, GPU, uptime, packages, etc). Static snapshot on launch, not live monitoring.
- **workspaces** — thin edge-anchored bar showing Hyprland workspace state via IPC socket. Active/occupied/empty as colored dots or rectangles.
- **panel: timer alert** — when pomodoro timers hit zero, spawn a brief fullscreen flash or floating notification. Currently timers just go negative silently.
- **launcher** — rofi replacement. App launcher with fuzzy search, keyboard nav. Same list-picker pattern as wallrun. Could support modes for different sources (desktop entries, custom lists).
- **cliphistory** — clipboard history picker. Reads from cliphist (or similar wl-clipboard history), presents as a filterable list overlay. Same fuzzy-search-and-pick pattern.
