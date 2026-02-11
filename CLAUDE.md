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

## sctk 0.20 notes

- Must explicitly call `get_keyboard_with_repeat()` in `new_capability` — requires calloop `LoopHandle` and `RepeatCallback`
- Must explicitly call `get_pointer()` in `new_capability` for pointer events
- `PointerHandler` has a single `pointer_frame()` method receiving `&[PointerEvent]`
- `PointerEventKind::Axis` has `absolute: f64` for scroll amount; BTN_LEFT = `0x110`
- `delegate_keyboard!`, `delegate_pointer!` macros generate dispatch glue
- `KeyboardHandler` requires `repeat_key` method (in addition to press/release)
- `CompositorHandler` requires `surface_enter` and `surface_leave` methods
- `ProvidesRegistryState` needs `registry_handlers!` macro

## cosmic-text rendering

`render_text()` wraps cosmic-text buffer creation and glyph blitting. Key detail: the `(x, y)` offset passed to `glyph.physical()` must include `run.line_y` (which contains the font's ascent offset). Without it, all text renders ~20-25px too high. The correct call is `glyph.physical((x, y + run.line_y), 1.0)`.

## Widgets

### wallrun

Image selection overlay for Hyprland — scans a directory of images, displays thumbnails in a filterable grid, prints the selected path to stdout.

**Stack:** smithay-client-toolkit 0.20, wayland-client, tiny-skia, cosmic-text, image, serde + toml

Single file: `src/main.rs`. Layer-shell overlay with keyboard + pointer input.

**Features:**
- Layer-shell overlay with configurable dimensions (fixed or `"fit"` to content)
- Directory scanning (`--dir`, `--ext` CLI flags) with thumbnail grid
- Fuzzy search — typed characters filter items, centered in the search bar
- Keyboard nav (arrow keys, Enter to select, Escape to exit)
- Mouse input (click to select, hover to highlight, scroll wheel to page)
- Thumbnail caching to `~/.cache/thumbnails/wallrun/` (keyed by path + mtime + dimensions)
- Scroll offset with auto-scroll to keep selection visible

**Architecture:**
- `App::draw()` renders to a `tiny_skia::Pixmap`, copies RGBA→BGRA into SHM buffer
- `App::handle_key()` handles all keyboard input (navigation, typing, selection)
- `PointerHandler` handles click-to-select, hover-to-highlight, and scroll
- `grid_metrics()` computes layout from window size and column count
- `load_items()` scans directory, loads/caches thumbnails
- calloop `EventLoop` + `WaylandSource` for keyboard repeat support

**Config** — `~/.config/widgets/wallrun.toml` (all optional):
```toml
columns = 3
window_width = 800        # or "fit" (auto-size from column count)
window_height = 600       # or "fit" (auto-size to show all items)
font_size = 20.0
label_font_size = 14.0
show_labels = true
color_file = "~/.cache/wal/colors-wallrun"
```

**Color keys:** `background`, `background_opacity`, `bar_bg`, `bar_border`, `text`, `text_placeholder`, `label`, `selection`, `selection_opacity`

### panel

Floating overlay panel for Hyprland — clock, pomodoro timers, volume control, theme toggle. Launched/killed to toggle (not persistent).

**Stack:** smithay-client-toolkit 0.20, wayland-client, tiny-skia, cosmic-text 0.17, libc, serde + toml

**Build:** `make install` installs `panel` and `panel_toggle` to `~/.local/bin/`. `panel_toggle` launches panel or kills existing instance.

Single file: `src/main.rs`. Layer-shell overlay with no anchors, pointer-only (no keyboard, `KeyboardInteractivity::None`).

**Features:**
- Clock (HH:MM:SS + "Month Day"), updates every second via calloop timer
- 2 pomodoro timers — click to start/pause, right-click to reset, scroll to adjust duration (+-60s)
- Timer state persists to `~/.local/state/widgets/panel/timers.toml` (survives panel close/reopen)
- Volume bar (0-200%) via `wpctl`, scroll to adjust
- Audio device icon (headphones/speaker), click to switch BT devices via `audio_switch.sh`
- Day/night toggle via `dim_toggle.sh`
- 6 color dots from walrs palette
- Hover highlighting on interactive tiles (toggle, timer1, timer2, audio)
- Font Awesome 7 Free (weight BLACK) for icons (toggle sun/moon, audio headphones/speaker)

**Architecture:**
- `App::draw()` renders to `tiny_skia::Pixmap`, copies RGBA→BGRA into SHM buffer
- `App::handle_click()` / `App::handle_scroll()` / `App::handle_right_click()` / `App::hover_tile_at()` dispatch pointer events via `layout()` + `Rect::contains()`
- Tile geometry computed by `layout(w, h) -> Layout` returning `Rect` structs for all 7 tiles: toggle, dots, clock, timer1, timer2, volume, audio
- Layout constants: `OUTER` (border), `INNER` (divider), `LEFT_W`, `RIGHT_W`, `TOGGLE_H`, `CLOCK_H`, `AUDIO_H`. Panel is 320×202.
- Audio control shells out to `wpctl` / scripts in `~/.config/quickshell/scripts/`
- calloop `Timer` fires every 1s for clock/timer redraws

**Config** — `~/.config/widgets/panel.toml` (all optional):
```toml
color_file = "~/.cache/wal/colors-panel.toml"
font_family = "Google Sans Code"
font_size = 30.0
timer1_duration = 3600
timer2_duration = 900
bt_device_1 = "AC:BF:71:08:A1:D6"
bt_device_2 = "EC:81:93:AC:8B:60"
```

**Color keys:** `background`, `background_opacity`, `border`, `divider`, `dot1`–`dot6`, `sun`, `clock`, `ui`

## Ideas

- **sysinfo** — neofetch/fastfetch-style system info overlay (host, kernel, CPU, RAM, GPU, uptime, packages, etc). Static snapshot on launch, not live monitoring.
- **workspaces** — thin edge-anchored bar showing Hyprland workspace state via IPC socket. Active/occupied/empty as colored dots or rectangles.
- **panel: timer alert** — when pomodoro timers hit zero, spawn a brief fullscreen flash or floating notification. Currently timers just go negative silently.
- **launcher** — rofi replacement. App launcher with fuzzy search, keyboard nav. Same list-picker pattern as wallrun. Could support modes for different sources (desktop entries, custom lists).
- **cliphistory** — clipboard history picker. Reads from cliphist (or similar wl-clipboard history), presents as a filterable list overlay. Same fuzzy-search-and-pick pattern.

## Todo

### panel
- [ ] Scrolling the timers to change duration should be persistent and saved. Right click reset should reset them to their current duration, not the default duration.

### wallrun
