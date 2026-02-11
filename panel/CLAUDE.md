# panel

Floating overlay panel for Hyprland — clock, pomodoro timers, volume control, theme toggle. Launched/killed to toggle (not persistent).

## Stack

- **smithay-client-toolkit 0.20** — Wayland layer-shell surface, seat/pointer handling, SHM buffers, calloop with 1-second timer
- **wayland-client** — direct dependency needed alongside sctk
- **tiny-skia** — CPU rasterizer for compositing into SHM buffer
- **cosmic-text 0.17** — text shaping/layout (clock, timers, icons)
- **libc** — `localtime_r` for timezone-aware clock
- **serde + toml** — config + timer state persistence

## Build & run

- `make install` — release build, installs `panel` and `panel_toggle` to `~/.local/bin/`
- `panel_toggle` — launches panel or kills existing instance (pkill -x panel || panel &)
- After making code changes, run `make install` yourself before reporting done

## Current state

Single file: `src/main.rs`. Layer-shell overlay with no anchors, pointer-only (no keyboard).

### Features
- Clock (HH:MM:SS + "Month Day"), updates every second via calloop timer
- 2 pomodoro timers — click to start/pause, right-click to reset, scroll to adjust duration (+-60s)
- Timer state persists to `~/.local/state/widgets/panel/timers.toml` (survives panel close/reopen)
- Volume bar (0-200%) via `wpctl`, scroll to adjust
- Audio device icon (headphones/speaker), click to switch BT devices via `audio_switch.sh`
- Day/night toggle via `dim_toggle.sh`
- 6 color dots from walrs palette
- Hover highlighting on interactive tiles (toggle, timer1, timer2, audio)

### Layout system

Tile geometry is computed by a single `layout(w, h) -> Layout` function that returns `Rect` structs for all 7 tiles: toggle, dots, clock, timer1, timer2, volume, audio. Hit-testing uses `Rect::contains()`. Text centering uses `center_x()` / `center_y()` helpers.

Layout constants define the grid: `OUTER` (border), `INNER` (divider), `LEFT_W`, `RIGHT_W`, `TOGGLE_H`, `CLOCK_H`, `AUDIO_H`. The panel is 320×202.

### Architecture
- `App::draw()` renders to `tiny_skia::Pixmap`, copies RGBA→BGRA into SHM buffer
- `App::handle_click()` / `App::handle_scroll()` / `App::handle_right_click()` / `App::hover_tile_at()` dispatch pointer events via `layout()` + `Rect::contains()`
- Audio control shells out to `wpctl` / scripts in `~/.config/quickshell/scripts/`
- calloop `Timer` fires every 1s for clock/timer redraws

### cosmic-text rendering

`render_text()` wraps cosmic-text buffer creation and glyph blitting. Key detail: the `(x, y)` offset passed to `glyph.physical()` must include `run.line_y` (which contains the font's ascent offset). Without it, all text renders ~20-25px too high. The correct call is `glyph.physical((x, y + run.line_y), 1.0)`.

Font Awesome 7 Free (weight BLACK) is used for icons (toggle sun/moon, audio headphones/speaker).

## Configuration

### TOML config — `~/.config/widgets/panel.toml`

All fields optional, defaults shown:

```toml
color_file = "~/.cache/wal/colors-panel.toml"
font_family = "Google Sans Code"
font_size = 30.0
timer1_duration = 3600
timer2_duration = 900
bt_device_1 = "AC:BF:71:08:A1:D6"
bt_device_2 = "EC:81:93:AC:8B:60"
```

### Color theming — walrs integration

Reads colors from `color_file`. Format is `key=value` with hex colors. A walrs template at `~/.config/walrs/templates/colors-panel.toml` generates the output file. Keys: `background`, `border`, `divider`, `dot1`–`dot6`, `sun` (day-mode toggle icon), `clock`, `ui` (timers/volume/audio), `background_opacity`.

## sctk 0.20 notes

Same patterns as wallrun. No keyboard handler needed (pointer-only). Uses `KeyboardInteractivity::None`.
