# widgets

Bespoke Wayland desktop widgets, written from scratch in Rust against sctk + tiny-skia rather than through widget frameworks like eww or quickshell.

## Why

Coding agents have made it practical to write and maintain bespoke native widgets. The schlep of wiring up Wayland protocols, pixel rendering, and input handling by hand used to make frameworks the obvious choice. Now that agents can just do that work, you get the upside of from-scratch widgets — tiny binaries, no runtime overhead, full control over every pixel — without the prohibitive cost of writing all the boilerplate yourself.

## Workflow

The project is a Cargo workspace. A single top-level Makefile builds and installs all widgets:

```
make install          # build + install all widgets
make install W=raven  # build + install just raven
```

Binaries go to `~/.local/bin/` (override with `PREFIX=`). The Makefile also generates `raven_toggle` (a shell script that toggles raven on/off via `pkill -x raven || raven &`).

`bench.sh` measures startup latency (time until process enters epoll_wait sleep state) over 1000 runs. Usage: `./bench.sh <widget> [args...]`.

## Shared patterns

All widgets follow the same architecture:

- **Rust + smithay-client-toolkit 0.20** for Wayland layer-shell surfaces, pointer/keyboard handling, SHM buffers, calloop event loop
- **tiny-skia** for CPU rendering into the SHM buffer (RGBA pixmap, copied to BGRA for Wayland)
- **cosmic-text** for text shaping and glyph rendering — fonts are loaded by file path (not system font scanning) via `FontSystem::new_with_locale_and_db()` with a manually built `fontdb::Database`
- **serde + toml** for config files
- **walrs** for colorscheme integration — each widget has a template in `~/.config/walrs/templates/` that generates a `key=value` color file in `~/.cache/wal/`
- Single `src/main.rs` per widget, typically 800-1400 lines
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

Fonts are loaded by file path, not by family name. Each widget's config specifies `font = "/path/to/font.ttf"` (raven also has `icon_font` for Font Awesome). At startup, font files are read into memory and loaded into a `fontdb::Database` via `load_font_data()`, then passed to `FontSystem::new_with_locale_and_db()`. This avoids the ~50-150ms cost of `FontSystem::new()` scanning all system fonts. The family name is extracted from the loaded font's metadata for use with `Family::Name(...)` in attrs.

Note: cosmic-text rejects `fontdb::Source::File` — fonts must be loaded as `Source::Binary` (i.e. via `load_font_data(Vec<u8>)`, not `load_font_file(path)`).

`render_text()` wraps cosmic-text buffer creation and glyph blitting. Key detail: the `(x, y)` offset passed to `glyph.physical()` must include `run.line_y` (which contains the font's ascent offset). Without it, all text renders ~20-25px too high. The correct call is `glyph.physical((x, y + run.line_y), 1.0)`.

`fill_triangle()` is a general-purpose scanline triangle rasterizer taking 3 float vertices, color, alpha, and y-clip range. Uses pixel-center sampling for both x and y to avoid off-by-one stray pixels. Used for bevelled volume bar endpoints and available for future decorative geometry.

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
font = "/usr/share/fonts/TTF/SomeFont-Regular.ttf"
font_size = 20.0
label_font_size = 14.0
show_labels = true
color_file = "~/.cache/wal/colors-wallrun"
```

**Color keys:** `background`, `background_opacity`, `bar_bg`, `bar_border`, `text`, `text_placeholder`, `label`, `selection`, `selection_opacity`

### raven

Floating overlay for Hyprland — clock, weather, pomodoro timers, volume control, theme toggle. Launched/killed to toggle (not persistent).

**Stack:** smithay-client-toolkit 0.20, wayland-client, tiny-skia, cosmic-text 0.17, libc, serde + toml

**Build:** `make install W=raven` installs `raven` and `raven_toggle` to `~/.local/bin/`. `raven_toggle` launches raven or kills existing instance.

Single file: `src/main.rs`. Layer-shell overlay with no anchors, pointer-only (no keyboard, `KeyboardInteractivity::None`).

**Features:**
- Clock (HH:MM:SS + "Month Day"), updates every second via calloop timer
- Weather tile — current temp + feels-like + condition icon via open-meteo API (lat/lon config, WMO weather codes). Day/night aware (sun/moon icon for clear skies). Fetched on launch via background curl, cached in state for 1 hour.
- 2 pomodoro timers — click to start/pause, right-click to reset, scroll to adjust duration (+-60s)
- State persists to `~/.local/state/widgets/raven.toml` (timers + weather cache survive close/reopen)
- Volume bar (0-200%) via `wpctl`, scroll to adjust, bevelled top/bottom (45° points via `fill_triangle`)
- Audio device icon (headphones/speaker), click to switch BT devices via `audio_switch.sh`
- Day/night toggle via `dim_toggle.sh`
- 14 color dots from walrs palette (7×2 grid)
- Hover highlighting on interactive tiles (toggle, timer1, timer2, audio)
- Font Awesome 7 Free Solid (weight BLACK) for filled icons, FA Regular (weight NORMAL) for outline icons. Toggle uses filled sun/moon, weather uses outline.

**Architecture:**
- `App::draw()` renders to `tiny_skia::Pixmap`, copies RGBA→BGRA into SHM buffer
- `App::handle_click()` / `App::handle_scroll()` / `App::handle_right_click()` / `App::hover_tile_at()` dispatch pointer events via `layout()` + `Rect::contains()`
- Tile geometry computed by `layout(w, h) -> Layout` returning `Rect` structs for 8 tiles: toggle, dots, clock, weather, timer1, timer2, volume, audio
- Layout: top center row split 2/3 clock | 1/3 empty tile; bottom center row split 2/5 weather | 3/5 timers (horizontally flipped from top). 20px bevelled corners with border on all 4 outside corners.
- Layout constants: `OUTER` (border), `INNER` (divider), `LEFT_W`, `RIGHT_W`, `TOGGLE_H`, `CLOCK_H`, `AUDIO_H`, `CORNER_BEVEL`. Raven is 410×230.
- Audio control shells out to `wpctl` / scripts in `~/.config/quickshell/scripts/`
- Weather: background `curl` to open-meteo API, polled via calloop tick. Hand-parsed JSON (no serde_json dependency). `weather_icon()` maps WMO codes to FA icons.
- calloop `Timer` fires every 100ms for clock/timer/weather redraws

**Config** — `~/.config/widgets/raven.toml` (all optional):
```toml
color_file = "~/.cache/wal/colors-raven.toml"
font = "~/.local/share/fonts/GoogleSansCode-Bold.ttf"
icon_font = "/usr/share/fonts/OTF/Font Awesome 7 Free-Solid-900.otf"
font_size = 39.0
timer1_duration = 3600
timer2_duration = 900
bt_device_1 = "AC:BF:71:08:A1:D6"
bt_device_2 = "EC:81:93:AC:8B:60"
weather_lat = 38.81
weather_lon = -89.95
```

**Color keys:** `background`, `background_opacity`, `border`, `divider`, `sun`, `clock`, `weather`, `ui`, `foreground`, `color1`–`color15`

### grimoire

App launcher and dmenu replacement — scans .desktop files, displays apps with icons in a filterable list, or reads lines from stdin in dmenu mode.

**Stack:** smithay-client-toolkit 0.20, wayland-client, tiny-skia, cosmic-text, image, resvg, serde + toml

Single file: `src/main.rs`. Layer-shell overlay with keyboard + pointer input.

**Features:**
- Two modes: `--drun` (default, app launcher) and `--dmenu` (stdin lines, prints selection to stdout)
- Layer-shell overlay with configurable dimensions
- Fuzzy search — typed characters filter items by name and comment
- Configurable multi-column grid layout (items flow left-to-right, top-to-bottom, centered when fewer items than columns)
- Keyboard nav (Left/Right across columns, Up/Down across rows, Enter to select, Escape to exit, Backspace to delete)
- Mouse input (click to select, hover to highlight, scroll wheel)
- Icon support: PNG and SVG via hicolor theme (both `/usr/share/icons/hicolor/` and `~/.local/share/icons/hicolor/`, including `scalable/` and `symbolic/` SVG dirs, sizes up to 512x512) + `/usr/share/pixmaps`, cached to `~/.cache/thumbnails/grimoire/`. Does NOT do full icon theme lookup (Adwaita, breeze, etc.) — apps using generic themed icon names (e.g. `network-wired`, `preferences-system-network`) won't resolve.
- .desktop file parsing with field code stripping, Terminal=true support, NoDisplay/Hidden filtering
- Comment text displayed next to app name in dimmer color
- Frecency sorting — tracks launch counts and timestamps in `~/.local/state/widgets/grimoire.toml`, scores by `count / (1 + hours_since_last / 72)`, falls back to alphabetical

**Architecture:**
- `App::draw()` renders to `tiny_skia::Pixmap`, copies RGBA→BGRA into SHM buffer
- `App::handle_key()` handles all keyboard input (navigation, typing, selection)
- `App::select_item()` — drun: fork+exec via `sh -c`; dmenu: println to stdout
- `PointerHandler` handles click-to-select, hover-to-highlight, and scroll
- `load_desktop_entries()` scans ~/.local/share/applications, /usr/local/share/applications, /usr/share/applications
- `resolve_icon()` finds and caches icons from hicolor theme dirs

**Config** — `~/.config/widgets/grimoire.toml` (all optional):
```toml
color_file = "~/.cache/wal/colors-grimoire"
font = "/usr/share/fonts/TTF/SomeFont-Regular.ttf"
font_size = 18.0
comment_font_size = 14.0
icon_size = 32
window_width = 600
window_height = 400
terminal = "ghostty -e"
columns = 1
show_comments = true
search_comments = false
```

**Color keys:** `background`, `background_opacity`, `border`, `bar_bg`, `bar_border`, `text`, `text_comment`, `text_placeholder`, `selection`, `selection_opacity`

## Ideas

- **sysinfo** — neofetch/fastfetch-style system info overlay (host, kernel, CPU, RAM, GPU, uptime, packages, etc). Static snapshot on launch, not live monitoring.
- **workspaces** — thin edge-anchored bar showing Hyprland workspace state via IPC socket. Active/occupied/empty as colored dots or rectangles.
- **cliphistory** — clipboard history picker. Reads from cliphist (or similar wl-clipboard history), presents as a filterable list overlay. Same fuzzy-search-and-pick pattern.

## Future applications

Larger projects that go beyond simple overlays — closer to full applications, but still built from scratch with the same stack.

- **systemd-center** — systemd unit manager. List/filter units, show status, start/stop/restart/enable/disable. Shells out to `systemctl`. Tabbed or sectioned view for services, timers, sockets. Maybe a journal tail view per unit.
- **notif** — notification daemon implementing the freedesktop notification spec. Receives notifications over D-Bus, renders them as transient layer-shell popups. Configurable timeout, action buttons, history recall overlay.
- **blueman** — Bluetooth manager. Scans/pairs/connects/disconnects devices via `bluetoothctl` or D-Bus. Shows device list with connection state, battery level where available. Quick-switch between paired audio devices.

## Todo

### raven
- pausing a timer that is negative sets it to 0:00
- Timer alert — when pomodoro timers hit zero, spawn a brief fullscreen flash or floating notification. Currently timers just go negative silently.
- Network tile — wifi SSID + signal strength or ethernet indicator
- Notification toggle — click to enable/disable `makoctl` (or similar) do-not-disturb mode

### wallrun
