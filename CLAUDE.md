# widgets

Bespoke Wayland desktop widgets, written from scratch in Rust against sctk + tiny-skia rather than through widget frameworks like eww or quickshell.

## Why

Coding agents have made it practical to write and maintain bespoke native widgets. The schlep of wiring up Wayland protocols, pixel rendering, and input handling by hand used to make frameworks the obvious choice. Now that agents can just do that work, you get the upside of from-scratch widgets — tiny binaries, no runtime overhead, full control over every pixel — without the prohibitive cost of writing all the boilerplate yourself.

## Workflow

The project is a Cargo workspace. A single top-level Makefile builds and installs all widgets:

```
make install          # build + install all widgets
make install W=wavedash  # build + install just wavedash
```

Binaries go to `~/.local/bin/` (override with `PREFIX=`). The Makefile also generates a toggle script per widget: `wavedash_toggle` (`pkill -x wavedash || wavedash &`), `evoke_toggle` (`pkill -x -USR1 evoke || evoke &`), and `wallrun_toggle`/`grimoire_toggle` (run the widget unless an instance is already up).

`bench.sh` measures startup latency (time until process enters epoll_wait sleep state) over 1000 runs. Usage: `./bench.sh <widget> [args...]`.

## Shared patterns

All widgets follow the same architecture:

- **Rust + smithay-client-toolkit 0.20** for Wayland layer-shell surfaces (tincture is the exception — an xdg-toplevel window), pointer/keyboard handling, SHM buffers, calloop event loop
- **tiny-skia** for CPU rendering into the SHM buffer (RGBA pixmap, copied to BGRA for Wayland)
- **cosmic-text** for text shaping and glyph rendering — fonts are loaded by file path (not system font scanning) via `FontSystem::new_with_locale_and_db()` with a manually built `fontdb::Database`
- **serde + toml** for config files
- **walrs** for colorscheme integration — each widget has a template in `~/.config/walrs/templates/` that generates a `key=value` color file in `~/.cache/wal/`
- Single `src/main.rs` per widget, typically 800-1400 lines
- `make install` puts binaries in `~/.local/bin/`
- Config files go in `~/.config/widgets/<name>.toml`
- State files go in `~/.local/state/widgets/<name>.toml` (one file per widget)

## sctk 0.20 notes

- Must explicitly call `get_keyboard_with_repeat()` in `new_capability` — requires calloop `LoopHandle` and `RepeatCallback`
- Must explicitly call `get_pointer()` in `new_capability` for pointer events
- `PointerHandler` has a single `pointer_frame()` method receiving `&[PointerEvent]`
- `PointerEventKind::Axis` has `absolute: f64` for scroll amount; BTN_LEFT = `0x110`
- `delegate_keyboard!`, `delegate_pointer!` macros generate dispatch glue
- `KeyboardHandler` requires `repeat_key` method (in addition to press/release)
- `CompositorHandler` requires `surface_enter` and `surface_leave` methods
- `ProvidesRegistryState` needs `registry_handlers!` macro
- `CursorShapeManager` (cursor-shape-v1) sets the pointer cursor — create a shape device on `PointerEventKind::Enter` and call `set_shape(serial, Shape::Default)`

## cosmic-text rendering

Fonts are loaded by file path, not by family name. Each widget's config specifies `font = "/path/to/font.ttf"` (wavedash also has `icon_font` for Font Awesome). At startup, font files are read into memory and loaded into a `fontdb::Database` via `load_font_data()`, then passed to `FontSystem::new_with_locale_and_db()`. This avoids the ~50-150ms cost of `FontSystem::new()` scanning all system fonts. The family name is extracted from the loaded font's metadata for use with `Family::Name(...)` in attrs.

Note: cosmic-text rejects `fontdb::Source::File` — fonts must be loaded as `Source::Binary` (i.e. via `load_font_data(Vec<u8>)`, not `load_font_file(path)`).

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
font = "/usr/share/fonts/TTF/SomeFont-Regular.ttf"
font_size = 20.0
label_font_size = 14.0
show_labels = true
color_file = "~/.cache/wal/colors-wallrun"
```

**Color keys:** `background`, `background_opacity`, `bar_bg`, `bar_border`, `text`, `text_placeholder`, `label`, `selection`, `selection_opacity`

### wavedash

Floating overlay for Hyprland — clock, weather, pomodoro timers, volume control, notification pause, theme toggle. Launched/killed to toggle (not persistent).

**Stack:** smithay-client-toolkit 0.20, wayland-client, tiny-skia, cosmic-text 0.17, libc, serde + toml

**Build:** `make install W=wavedash` installs `wavedash` and `wavedash_toggle` to `~/.local/bin/`. `wavedash_toggle` launches wavedash or kills existing instance.

Single file: `src/main.rs`. Layer-shell overlay with no anchors, pointer-only (no keyboard, `KeyboardInteractivity::None`). 440×235, typographic layout — text directly on a full-bleed background with 4px accent bars along the left and right edges, no tile borders or dividers.

**Features:**
- Clock — hero 12-hour H:MM with smaller AM/PM suffix, top-left. Date below ("Mon, July 21"), clickable — opens Google Calendar via `xdg-open`.
- Weather (top-right) — condition icon + current temp, feels-like below, via open-meteo API (lat/lon config, WMO weather codes). Day/night aware (sun/moon icon for clear skies). Fetched on launch via background curl (skipped if `weather_lat` is 0), cached in state for `weather_max_age` seconds (default 3600).
- Left icon column, top to bottom: day/night toggle (sun/moon per weather is_day, click runs `dim_toggle.sh`), notification pause (bell/bell-slash, click toggles `dunstctl set-paused`, initial state from `dunstctl is-paused`), audio device (headphones/speaker)
- Audio icon — click switches BT devices via `audio_switch.sh`, middle-click launches `pavucontrol`. Headphone detection matches `wpctl inspect` output against the `bt_device_1` MAC or "headphone"/"headset".
- Volume bar (0-200%) beside the audio icon via `wpctl`, scroll to adjust. Rounded pill-shaped fill bar (track + fill via `fill_rounded_rect_alpha`). Muted dims both icon and bar.
- 2 pomodoro timers stacked bottom-right (short timer on top), right-aligned, dimmed when paused — click to start/pause, right-click to reset to scroll-adjusted base, middle-click to reset to config duration, scroll to adjust duration (+-60s)
- State persists to `~/.local/state/widgets/wavedash.toml` (timers + weather cache survive close/reopen)
- Hover highlighting on interactive elements (toggle, notif, audio, volume, timers, date)
- Font Awesome 7 Free Solid (weight BLACK) for filled icons, FA Regular (weight NORMAL) for outline icons. Toggle uses filled sun/moon, weather uses outline.
- Long press detection — hold the toggle key >300ms to peek (dismisses on release), tap quickly for persistent toggle mode. Uses SIGUSR2 from a Hyprland `bindr` release binding.

**Architecture:**
- `App::draw()` renders to `tiny_skia::Pixmap`, copies RGBA→BGRA into SHM buffer
- `App::handle_click()` / `handle_scroll()` / `handle_right_click()` / `handle_middle_click()` / `hover_tile_at()` dispatch pointer events via `layout()` + `Rect::contains()`
- Hit geometry computed by `layout(w, h) -> Layout` returning `Rect` structs for toggle, clock, date, notif, weather, timer1, timer2, volume, audio
- Layout constants: `WIDTH`/`HEIGHT` (440×235), `ACCENT_W`, `LEFT_MARGIN`, plus a type scale (`CLOCK_HM_SIZE`, `DATE_SIZE`, `WEATHER_ICON_SIZE`, `WEATHER_TEMP_SIZE`, `WEATHER_FEELS_SIZE`, `TIMER_SIZE`, `UTIL_ICON_SIZE`)
- Shells out to `wpctl`, `dunstctl`, `xdg-open`, `pavucontrol`, `audio_switch.sh` (in `~/.config/quickshell/scripts/`), and `dim_toggle.sh` (in `~/wgmn/scripts/`)
- Weather: background `curl` to open-meteo API, polled via calloop tick. Hand-parsed JSON (no serde_json dependency). `weather_icon()` maps WMO codes to FA icons.
- Time/date via `libc::localtime_r` (no chrono dependency)
- calloop `Timer` fires every 100ms for clock/timer/weather redraws; also refreshes volume/mute/headphone state, skipped for 1s after a scroll-set to avoid fighting `wpctl`
- Long press: `SIGUSR2` handler via `sigaction` + `AtomicBool` (same pattern as evoke's SIGUSR1). Grace period (`LONG_PRESS_GRACE_MS = 300`) delays the decision — if signal arrives within grace period, short press (persistent); otherwise long press (exit on release). Requires `bindr = CTRL, Control_R, exec, pkill -USR2 -x wavedash` in Hyprland config.

**Config** — `~/.config/widgets/wavedash.toml` (all optional):
```toml
color_file = "~/.cache/wal/colors-wavedash.toml"
font = "~/.local/share/fonts/GoogleSansCode-Bold.ttf"
icon_font = "/usr/share/fonts/OTF/Font Awesome 7 Free-Solid-900.otf"
timer1_duration = 3600
timer2_duration = 900
bt_device_1 = "AC:BF:71:08:A1:D6"
bt_device_2 = "EC:81:93:AC:8B:60"
weather_lat = 38.81
weather_lon = -89.95
weather_max_age = 1800   # seconds before the cached weather is refetched
```
(`font_size` is also accepted but unused — all text sizes come from the type scale constants.)

**Color keys:** `background`, `background_opacity`, `sun`, `clock`, `accentl`, `accentr`, `weather`, `audio`, `volume`, `notif`, `timer`. Also parsed (and emitted by the walrs template) but not currently rendered: `border`, `divider`, `foreground`, `color1`–`color15`.

### grimoire

App launcher and dmenu replacement — scans .desktop files, displays apps with icons in a filterable list, or reads lines from stdin in dmenu mode.

**Stack:** smithay-client-toolkit 0.20, wayland-client, tiny-skia, cosmic-text, image, resvg, serde + toml

Single file: `src/main.rs`. Layer-shell overlay with keyboard + pointer input.

**Features:**
- Two modes: `--drun` (default, app launcher) and `--dmenu` (stdin lines, prints selection to stdout)
- Layer-shell overlay with configurable dimensions
- Fuzzy search — typed characters filter items by name and comment. `fuzzy_score` is a DP over every alignment of the query into the string (not a greedy first-occurrence scan): extending a contiguous run is free, starting a new run costs the characters skipped plus a penalty that shrinks at a word boundary (`/ _ - . space`, or a lower→upper transition). Lower score wins, and the sort is stable, so equal scores keep frecency order
- Configurable multi-column grid layout (items flow left-to-right, top-to-bottom, centered when fewer items than columns)
- Keyboard nav (Left/Right across columns, Up/Down across rows, Enter to select, Escape to exit, Backspace to delete, Ctrl+Backspace to clear the search)
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
center_items = false
```

**CLI flags:** `--drun`, `--dmenu`, `--columns N`, `--center-items`, `--relative-paths <prefix>` (strips prefix from stdin lines for display, preserves full path for output)

**Color keys:** `background`, `background_opacity`, `border`, `bar_bg`, `bar_border`, `text`, `text_comment`, `text_placeholder`, `selection`, `selection_opacity`

### evoke

Speech-to-text overlay — streams the mic to OpenAI's realtime transcription API, shows the words live in an editable text box, optionally cleans them up with an LLM, then types the result into the window you were in (or copies it).

**Stack:** smithay-client-toolkit 0.20, wayland-client, cosmic-text, tungstenite (rustls/ring), ureq, serde_json, data-encoding, serde + toml

Single file: `src/main.rs`. Layer-shell overlay with exclusive keyboard + pointer input. Draws straight into the BGRA SHM buffer with one `blend()` helper (no tiny-skia).

**Features:**
- Records via `pw-record` (24 kHz mono s16le — the only rate the realtime API accepts); a small waveform strip shows the level
- One OpenAI realtime transcription session per dictation (`wss://api.openai.com/v1/realtime?intent=transcription`, model `gpt-live-transcribe`). Words arrive as `...transcription.delta` events while you speak and are appended at the end of the text box as normal editable text; the cursor only follows if it was already at the end
- Editable while dictating: cosmic-text `Editor` — arrows/Home/End (Shift selects, Ctrl by word/buffer), Backspace/Delete/Ctrl+Backspace, Ctrl+A/C/X/V (wl-copy / wl-paste), Ctrl+Z / Ctrl+Shift+Z (edits within 1s merge into one undo step), click/double/triple-click/drag. The box grows upward to `max_lines`, then scrolls with the cursor
- Enter (or the `send` label, or SIGUSR1) finishes: stops the mic, waits for the final transcript, applies `[replace]` whole-word fixes, then sends. Shift+Enter inserts a newline, Esc quits, the `copy` label copies and quits
- Optional LLM cleanup (`[cleanup]` section): the first Enter sends the text to `{base_url}/chat/completions` and replaces it in place as one undoable change (label shows `clean`, then `send`); Ctrl+Z restores the original, the next Enter sends. Editing is locked while it runs
- Sending: waits until no key is held and the pointer has been still for 500ms, drops the layer surface (Hyprland refocuses the previous window), then delivers. `send = "ime"` binds `zwp_input_method_v2` and `commit_string`s into the focused text field (no clipboard; newlines become spaces when the field reports content purpose Terminal). `send = "paste"` does `wl-copy` + `hl.dsp.send_shortcut` (Ctrl+Shift+V for terminals) to the window captured at launch. If delivery fails the overlay comes back with the error and the text intact
- Errors (network, API, cleanup) show in the top row in the border color and are printed to stderr; the text is kept
- Usage totals: the usage reports the API already returns (the transcription `completed` event and the cleanup response) are summed into `~/.local/state/widgets/evoke.toml` — e.g. `transcribe_seconds` (or token counts, depending on the model), `cleanup_prompt_tokens`, `cleanup_completion_tokens`, and request counts. A dictation cancelled before its transcript completes gets no usage report, so it isn't counted

**Architecture:**
- `Phase` state machine: `Recording` → `Finishing` (mic stopped, waiting for `completed`) → `Idle` (editing) → `Cleaning` / `Sending`. `request(Then)` advances it from Enter, the buttons and SIGUSR1; `on_down()` handles messages from worker threads
- Network thread `stream()`: tungstenite over a `TcpStream` with a 10ms read timeout, so one loop alternates reading events and draining the audio `mpsc` channel. The channel doubles as the queue for audio recorded before `session.updated`, so launch stays instant. Events come back over a calloop `channel`
- Stopping kills pw-record; the pipe EOF in the calloop `Generic` source sends the remaining audio plus a single `input_audio_buffer.commit`. The `completed` event is only the "done" signal — the text already in the box is kept (the user may have edited it)
- `cleanup()` runs in a thread (ureq, blocking) and reports back through the same channel
- calloop `Timer` at ~30fps: SIGUSR1 flag, the 10s finishing timeout, the 2s input-method deadline, and redraws (every tick while recording, otherwise only when `dirty`)

**Realtime API facts (measured):** `gpt-live-transcribe` rejects server VAD (`turn_detection` must be null — the client commits); deltas are never revised (the final transcript equals the concatenated deltas apart from a leading space); committing at every pause hurts accuracy (context is lost across commits), so evoke commits once at the end; commit → `completed` takes ~0.4-0.6s even for a 99s turn; `delay = "minimal"` makes words appear ~0.7s sooner than the default; `keywords` helps but is not reliable (hence `[replace]`); sessions expire 60 minutes after creation. Costs $0.017/min.

**Input-method notes:** Hyprland 0.56.2 ignores the input-method commit serial and relays each commit immediately, so long text is committed in 2000-byte pieces (Wayland messages are capped at 4 KiB). Only one input method can be bound per seat — a running fcitx5/ibus IME makes evoke get `unavailable`.

**Toggle script** (`evoke_toggle`): `pkill -x -USR1 evoke || evoke &` — a second SUPER+G sends SIGUSR1, which acts like Enter (evoke waits for Super to be released before handing focus back). Must use `pkill -x` (exact match) to avoid matching the toggle script process itself.

**Config** — `~/.config/widgets/evoke.toml`:
```toml
color_file = "~/.cache/wal/colors-evoke.toml"
openai_api_key = "sk-..."
font = "~/.local/share/fonts/GoogleSansCode-Regular.ttf"
font_size = 16.0
width = 600
max_lines = 8
margin = 0.1             # bottom margin as fraction of screen height
border_width = 2
bar_width = 2
bar_gap = 1
scale = 20.0             # amplitude multiplier for the waveform
model = "gpt-live-transcribe"
delay = "minimal"        # minimal|low|medium|high|xhigh
language = "en"
keywords = ["Hyprland"]  # sent to the transcription model and the cleanup prompt
send = "ime"             # or "paste"

[replace]                # whole-word, case-sensitive, applied when the transcript is final
Hyperland = "Hyprland"

[cleanup]                # optional; omit to send the raw text
base_url = "https://api.openai.com/v1"   # any OpenAI-compatible chat/completions endpoint
model = "gpt-6-luna"
reasoning_effort = "none"                # only sent if set
# api_key = "..."                        # defaults to openai_api_key
# prompt = "..."                         # replaces the built-in cleanup prompt
```

**Color keys:** `background`, `background_opacity`, `border` (also used for errors), `waveform`, `text`, `dim` (labels, status, placeholder), `selection`

### tincture

Colorscheme editor — drags the 16 extracted colors of a walrs scheme around an OKLCH wheel and reapplies the theme live.

**Stack:** smithay-client-toolkit 0.20, wayland-client, tiny-skia, cosmic-text, serde + toml

Single file: `src/main.rs`. Ordinary xdg-toplevel window (app_id `tincture`) rather than a layer-shell surface, so it can be moved and focused like any other window — min and max size are both pinned to 984×672 (an 820×560 design space times the global scale `S`). Pair it with `windowrule = float, class:^(tincture)$` in the Hyprland config (add `pin` to keep it above other windows).

**Features:**
- Edits `~/.cache/wal/cache/<sanitized_image_path>` — the walrs cache file (16 hex lines + alpha). Path sanitizing mirrors walrs: strip leading `/`, then `/`→`_` and `.`→`_`. Takes an image path as the sole positional arg; with no arg it reads the current wallpaper from `~/.cache/wal/wal`.
- OKLCH wheel — hue by angle, chroma by radius, sliced at the primary selection's lightness. Out-of-gamut regions are rendered dimmed instead of clipped, so the sRGB boundary is visible.
- Lightness strip beside the wheel (neutral ramp) with all 16 colors plotted on it.
- Alpha slider in the top-left corner edits the trailing alpha line of the cache file (0-100, what templates see as `{alpha}` / `{opacity}`). Drag or scroll to set. A `preview` toggle beneath it paints tincture's own background at that alpha (only the base fill, not the wheel/strip/text) so the value can be eyeballed against the desktop. Preview is off at launch and not persisted.
- Drag a dot to move hue/chroma; drag the strip to move lightness. Multi-selection drags relatively — the grabbed color follows the cursor and the rest move by the same delta.
- Selection: click / shift+click a dot or a rail chip, digits `0`–`9`, Tab / Shift+Tab to cycle. Group keys: `a` all, `n` normal (1–7), `b` bright (9–15), `g` bg+fg (0, 15).
- Keyboard nudges apply to the whole selection: arrows for lightness/hue, `-`/`=` for chroma, Shift for 5× steps.
- Applies by writing the cache file and running `walrs -i <image> -W -q` (walrs reads its cache verbatim without `--regen`, so it refills every template, repaints terminals, and runs walrs scripts). Fires on drag release / keypress; a run in flight sets a pending flag polled by a 150ms calloop timer, so edits coalesce instead of stacking walrs invocations.
- Self-themed: all chrome (background, text, dim text) is painted from the palette being edited, so the window restyles live as you drag.
- Two text buttons in the top right, both of which close the window: `revert` restores the launch snapshot and applies it, `keep` leaves the current palette on disk. Closing via the compositor keeps. No key quits — Ctrl+Z / Ctrl+Shift+Z undo and redo (snapshots coalesced at 400ms, redo stack cleared by any new edit) and Ctrl+R resets to the launch snapshot without closing. Every exit path flushes synchronously, so the applied theme always matches what was on screen.

**Architecture:**
- Colors live as `[Lch; 16]` (OKLCH floats) so repeated edits don't quantize through u8; converted to sRGB only for rendering and for writing the cache file. `Scheme = ([Lch; 16], u8)` pairs them with alpha and is the unit of undo/redo/revert/reset.
- `blit_mask` / `blit_rgba` composite with source-over alpha (pixmap is premultiplied RGBA), so glyph and disc edges stay clean when the background is translucent.
- `lch_to_rgb()` gamut-maps by binary-searching chroma down until the color fits sRGB.
- `ensure_disc()` rasterizes the wheel into an RGBA buffer, cached and keyed by quantized lightness.
- All layout constants, hit tests and font sizes are in a fixed 820×560 design space. The `S` constant scales design units to pixels inside the drawing primitives (`circle`, `fill_rect`, `render_text`, `measure_text`) and pointer coords are divided by `S` on the way in, so resizing the whole widget is a one-line change and nothing else has to know about it.
- `App::draw()` renders to `tiny_skia::Pixmap`, copies RGBA→BGRA into SHM buffer. Dots via `tiny_skia` paths (anti-aliased), everything else via direct pixel writes.
- `handle_press()` / `handle_motion()` / `handle_key()` dispatch input; drag state captures the pre-drag palette so motion is always computed as a delta from the press point.
- `dot_order()` (unselected, then selected, primary last) is shared by the draw loop and the wheel hit test, which walks it backwards — so a click on overlapping dots always resolves to the one painted on top rather than the highest index.

**Config** — `~/.config/widgets/tincture.toml`:
```toml
font = "~/.local/share/fonts/GoogleSansCode-Regular.ttf"
```

No color file — it themes itself from the palette under edit.

## External scripts

Scripts in `../scripts/` that use these widgets:

- **`paper`** — wallpaper setter. `paper --launcher` invokes `wallrun` to pick an image from `$WALLPAPER_DIR` (default `~/wallpapers`), then sets it via `hyprctl hyprpaper wallpaper`, updates `~/.config/hypr/hyprpaper.conf` for persistence, and runs `walrs` to regenerate the colorscheme. Also accepts a path directly (`paper /path/to/image`).
- **`tome`** — PDF launcher. `tome --library` pipes `find ~/books -name '*.pdf'` into `grimoire --dmenu` to pick a PDF, then opens it in zathura. Also accepts a path directly (`tome file.pdf`).

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
### grimoire
- bug: `load_desktop_entries` dedupe direction is inverted. Comment says "local overrides system", but `desktop_dirs()` yields `~/.local/share/applications` first, so the local entry is inserted into `seen` first and then the system entry overwrites it via `items[idx] = item`. Fix: either reverse `desktop_dirs()` order, or skip when `seen` already contains the filename.
- doesn't parse `Keywords=` from .desktop files, so e.g. searching "discord" won't match `vesktop.desktop` (which has `Keywords=discord;...`).

### wavedash
- Timer alert — when pomodoro timers hit zero, notify somehow. Probably just bring up the panel.
- Network tile — wifi SSID + signal strength or ethernet indicator
- Better layout abstractions — reduce overhead while adding consistency and styling

### wallrun
- Flag for the launcher to only search over the filename rather than the full path string
