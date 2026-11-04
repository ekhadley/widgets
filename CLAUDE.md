# widgets

Bespoke Wayland desktop widgets, written from scratch in Rust against sctk + tiny-skia rather than through widget frameworks like eww or quickshell.

## Why

Coding agents have made it practical to write and maintain bespoke native widgets. The schlep of wiring up Wayland protocols, pixel rendering, and input handling by hand used to make frameworks the obvious choice. Now that agents can just do that work, you get the upside of from-scratch widgets — tiny binaries, no runtime overhead, full control over every pixel — without the prohibitive cost of writing all the boilerplate yourself.

## Workflow

The project is a Cargo workspace. A single top-level Makefile builds and installs all widgets:

```
make install          # build + install all widgets
make install W=rail      # build + install just rail
```

Binaries go to `~/.local/bin/` (override with `PREFIX=`). The Makefile also generates a toggle script per widget: `rail_toggle` (`pkill -x rail || rail &`), `evoke_toggle` (`pkill -x -USR1 evoke || evoke &`), and `wallrun_toggle`/`grimoire_toggle` (run the widget unless an instance is already up).

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

Fonts are loaded by file path, not by family name. Each widget's config specifies `font = "/path/to/font.ttf"` (rail also has `font_bold`; its icons are its own SVGs, rasterized with resvg). At startup, font files are read into memory and loaded into a `fontdb::Database` via `load_font_data()`, then passed to `FontSystem::new_with_locale_and_db()`. This avoids the ~50-150ms cost of `FontSystem::new()` scanning all system fonts. The family name is extracted from the loaded font's metadata for use with `Family::Name(...)` in attrs.

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

### rail

Status bar for Hyprland across the top of one monitor — clock, date, weather, audio output and volume, pomodoro timers with an alarm, notification pause, dim toggle, colorscheme dots. Launched/killed to toggle (not persistent).

**Stack:** smithay-client-toolkit 0.20, wayland-client, tiny-skia, cosmic-text 0.17, resvg 0.45 (no default features), libc, serde + toml

**Build:** `make install W=rail` installs `rail` and `rail_toggle` to `~/.local/bin/`. `rail_toggle` launches rail or kills the existing instance.

Single file: `src/main.rs`. Layer-shell `Layer::Top` surface anchored top/left/right, `height` px tall, pointer-only (`KeyboardInteractivity::None`). Goes on the `output` named in the config, or on the focused monitor if none is set. One row: a left block from the left edge and a right block flush against the right edge, groups within a block separated by thin vertical rules:

`time | date | weather | output volume [brightness battery]` ... `| alarm t1 t2 | notif [dim] | palette`

**Items:**
- time (bold). date ("Tuesday, November 3", click opens Google Calendar via `xdg-open`). weather ("62° clear", then a dimmed "feels 58°"; omitted before the first fetch). Weather via open-meteo (lat/lon config, WMO codes mapped to an icon and a word by `weather_kind()`), day/night aware for clear skies. Fetched on launch via background curl (skipped if `weather_lat` is 0), cached in state for `weather_max_age` seconds (default 3600).
- Metered items (volume, brightness, t1, t2): icon, a round-capped meter, then the value.
  - volume: meter spans 0-200%. Click or drag the meter to set, scroll to adjust, click the icon/value to mute (muted shows a slashed, dimmed icon), middle-click launches `pavucontrol`. Uses `wpctl`.
  - brightness (machines with a backlight): sysfs backlight; click/drag the meter or scroll to set via `brightnessctl`.
  - t1/t2: the meter is the remaining time on the scale of the config duration. Click to start/pause, right-click to reset to the scroll-adjusted base, middle-click to reset to the config duration, scroll for ±60s. Click/drag the meter to set the time, snapped to whole minutes (a paused timer also takes it as its base; a running one keeps running from it). Stopped timers are dimmed. A finished timer keeps counting negative and its time breathes in `accentr` until clicked, which resets it to its base.
- battery (machines with one): icon whose interior fills with the charge, plus the percentage. While plugged in the fill is `battery_charging` with a bolt inverted against it; at or below 20% it is `battery_low`. Not interactive.
- output: headphones/speakers icon; click switches between `bt_device_1` and `audio_device_2` via `pactl`, each matched as a substring of the sink name with `:` → `_`; middle-click launches `pavucontrol`. Headphone detection matches `wpctl inspect` output against the `bt_device_1` MAC or "headphone"/"headset".
- alarm: toggle, persisted in state; turning it on plays `alert_sound` once as a preview, turning it off stops any playing alarm. When a timer reaches zero with the alarm on, a detached watcher process plays the sound with `pw-play` and launches `rail --pin` if it isn't up.
- notif: click toggles `dunstctl set-paused` (initial state from `dunstctl is-paused`). dim (machines without a backlight): sun/moon per weather is_day, runs `dim_toggle.sh`. Off toggles show the slashed icon, dimmed.
- palette: six dots in `color1`-`color6`; click runs `tincture` (no argument, so it edits the current wallpaper's scheme).
- Hover brightens the item under the pointer. Hit areas extend halfway into the gaps.
- State persists to `~/.local/state/widgets/rail.toml` (timers, alarm toggle and weather cache survive close/reopen).
- Peek vs pin: hold the toggle key >300ms to peek — the bar floats over windows (exclusive zone 0) and exits on release. A quick tap pins it: it sets its exclusive zone to `height`, so windows move down. `rail --pin` pins from the start. Uses SIGUSR2 from a Hyprland release binding.

**Architecture:**
- Sizes are design px for a 28px bar in `Sz::new()`, multiplied by `height / 28`, so the config `height` scales text, icons, meters and spacing together. If the content is wider than the monitor, rail prints an error on configure. Opacities `DIM`/`OFF`/`TRACK`/`RULE` are blended against the background color by `fade()`.
- Items are an `Item` enum. `App::groups()` lists the groups of each block (depending on which hardware the machine has), `item_w()` gives each item's width, and `place()` lays groups out with rules between them into a `Layout` of `Seg { item, r, meter }` plus rule x positions. Text widths come from one measured character advance (`cw`, `clock_cw`) since the font is monospace, so layout needs no shaping.
- `draw()`, click, scroll, middle/right click and drag all dispatch on the `Item` under the pointer (`Layout::at`). A left press on a meter sets `drag` and `drag_to()` follows pointer motion until release (`end_drag()` saves timer state).
- Event loop: `dispatch(next_timeout())` then `tick()`, so the tick also runs after every pointer event batch. The timeout is 33ms while dragging, during the long-press grace period or a peek, while a timer pulses, or while weather is fetching; otherwise it lands on the next whole second. `tick()` redraws only when `frame_key()` (a string of everything shown) differs from the last drawn frame, or a timer is pulsing.
- Scroll and drag only set `volume`/`brightness` and a dirty flag; `tick()` applies them with one blocking `wpctl`/`brightnessctl` call per event batch, so a fast drag never piles up processes. Volume/mute/headphone state and the backlight are polled every 2s, except during a drag and for 1s after a set.
- `App::draw()` paints a `tiny_skia::Pixmap` through `Painter` (`text`, `icon`, `meter`, `dot`, `fill`), then copies RGBA→BGRA into the SHM buffer.
- Icons are SVG bodies on a 24-unit grid in `ICON_*` / `WEATHER_ICONS` constants (filled, in the style of Font Awesome Solid; weather in outline). `Icons::new()` rasterizes them once at startup with resvg into alpha masks, which `Painter::icon` tints. Holes (`*_HOLES`) and the off-state slash are mask arithmetic (`cut()`), and the battery fill is built per frame by `battery_fill()`. Cut-outs are real transparency, not painted background, so they stay correct on the translucent bar.
- Alarm: `spawn_alarm(n, started)` runs when a timer starts and at launch for any timer still running — it pkills any existing `rail --alarm n` watcher and starts a fresh one, so there is never more than one per timer. `run_alarm()` renames its process to `rail-alarm` (so the toggle script's `pkill -x rail` leaves it alone), polls the state file every 500ms, exits as soon as the timer's `started` changes (pause, reset, restart), and fires when remaining ≤ 0 and `alert` is set. It keeps polling while the sound plays, so stopping the timer or switching the alarm off cuts the sound.
- Shells out to `wpctl`, `pactl`, `dunstctl`, `xdg-open`, `pavucontrol`, `pw-play`, `brightnessctl`, `pgrep`/`pkill`, `tincture`, and `dim_toggle.sh` (in `~/wgmn/scripts/`)
- Weather: background `curl` to open-meteo API, polled in `tick()`. Hand-parsed JSON (no serde_json dependency).
- Time/date via `libc::localtime_r` (no chrono dependency)
- Long press: `SIGUSR2` handler via `sigaction` + `AtomicBool` (same pattern as evoke's SIGUSR1). Grace period (`LONG_PRESS_GRACE_MS = 300`) delays the decision — if the signal arrives within it, short press (pin); otherwise long press (peek, exit on release). Requires a release binding sending `pkill -USR2 -x rail` in the Hyprland config.

**Config** — `~/.config/widgets/rail.toml` (all optional):
```toml
height = 32              # bar height in px; text, icons and spacing scale with it
output = "DP-1"          # pin to a monitor by name; omit for the focused one
color_file = "~/.cache/wal/colors-rail.toml"
font = "~/.local/share/fonts/GoogleSansCode-Regular.ttf"
font_bold = "~/.local/share/fonts/GoogleSansCode-Bold.ttf"   # same family; used for the time
timer1_duration = 3600   # also the full scale of the t1 meter
timer2_duration = 900
bt_device_1 = "AC:BF:71:08:A1:D6"
audio_device_2 = "EC:81:93:AC:8B:60"   # second output: BT MAC or a unique substring of the sink name (e.g. "HiFi__Speaker__sink")
weather_lat = 38.81
weather_lon = -89.95
weather_max_age = 1800   # seconds before the cached weather is refetched
alert_sound = "~/.config/widgets/rail-alarm.wav"   # played by pw-play when a timer hits zero with the alarm on
alert_volume = 1.0       # 0.0-1.0, linear gain passed to pw-play --volume (also applies to the preview)
```

**Color keys:** `background`, `background_opacity`, `foreground` (text, icons; dimmed text and tracks are faded from it), `divider` (rules), `clock`, `weather`, `volume`, `brightness`, `battery`, `battery_charging`, `battery_low`, `timer`, `accentr` (finished-timer pulse), `color1`-`color6` (palette dots). Other keys the walrs template emits are ignored.

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

Single file: `src/main.rs`. Layer-shell overlay with on-demand keyboard focus (focused only while hovered, per Hyprland focus-follows-mouse) + pointer input. Draws straight into the BGRA SHM buffer with one `blend()` helper (no tiny-skia).

**Features:**
- Records via `pw-record` (24 kHz mono s16le — the only rate the realtime API accepts); a small waveform strip shows the level
- One OpenAI realtime transcription session per dictation (`wss://api.openai.com/v1/realtime?intent=transcription`, model `gpt-live-transcribe`). Words arrive as `...transcription.delta` events while you speak and are appended at the end of the text box as normal editable text; the cursor only follows if it was already at the end
- Editable while dictating: cosmic-text `Editor` — arrows/Home/End (Shift selects, Ctrl by word/buffer), Backspace/Delete/Ctrl+Backspace, Ctrl+A/C/X/V (wl-copy / wl-paste), Ctrl+Z / Ctrl+Shift+Z (edits within 1s merge into one undo step), click/double/triple-click/drag. The box grows upward to `max_lines`, then scrolls with the cursor
- Labels in the top row: `copy` (copies and quits), `clean` (only shown with `[cleanup]`; dimmed once the text is cleaned), `send` (sends the text as it is). Enter (or SIGUSR1) cleans first if `[cleanup]` is set and the text isn't cleaned yet, otherwise sends; Ctrl+Enter sends as is. Any of these during dictation first stops the mic, waits for the final transcript and applies `[replace]` whole-word fixes. Shift+Enter inserts a newline, Esc quits
- Optional LLM cleanup (`[cleanup]` section): the first Enter sends the text to the Anthropic Messages API (Claude Haiku 5.5 by default, thinking disabled) and replaces it in place as one undoable change; Ctrl+Z restores the original, the next Enter sends. Editing is locked while it runs
- Sending: waits until no key is held and the pointer has been still for 500ms, drops the layer surface (Hyprland refocuses the previous window), then delivers. `send = "ime"` binds `zwp_input_method_v2` and `commit_string`s into the focused text field (no clipboard; newlines become spaces when the field reports content purpose Terminal). `send = "paste"` does `wl-copy` + `hl.dsp.send_shortcut` (Ctrl+Shift+V for terminals) to the window captured at launch. If delivery fails the overlay comes back with the error and the text intact
- Errors (network, API, cleanup) show in the top row in the border color and are printed to stderr; the text is kept
- Usage totals: the usage reports the API already returns (the transcription `completed` event and the cleanup response) are summed into `~/.local/state/widgets/evoke.toml` — e.g. `transcribe_seconds` (or token counts, depending on the model), `cleanup_input_tokens`, `cleanup_output_tokens`, and request counts. A dictation cancelled before its transcript completes gets no usage report, so it isn't counted

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
openai_api_key = "sk-..."         # transcription
anthropic_api_key = "sk-ant-..."  # cleanup
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
model = "claude-haiku-5-5"   # any Anthropic model that accepts thinking disabled
# prompt = "..."             # replaces the built-in cleanup prompt
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

### rail
- Network tile — wifi SSID + signal strength or ethernet indicator
- Hover popups with more info and interactivity per item
- Workspace indicators and window titles

### wallrun
- Flag for the launcher to only search over the filename rather than the full path string
