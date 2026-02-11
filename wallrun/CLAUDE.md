# wallrun

Image selection overlay for Hyprland — scans a directory of images, displays thumbnails in a filterable grid, prints the selected path to stdout.

## Stack

- **smithay-client-toolkit 0.20** — Wayland layer-shell surface, seat/keyboard/pointer handling, SHM buffers, calloop integration for key repeat
- **wayland-client** — direct dependency needed alongside sctk
- **tiny-skia** — CPU rasterizer for compositing into SHM buffer
- **cosmic-text** — text shaping/layout (search bar, labels)
- **image** — thumbnail decoding and resizing
- **serde + toml** — config file parsing

## Current state

Fully functional image picker. Single file: `src/main.rs` (~700 lines).

### Features
- Layer-shell overlay with configurable dimensions (fixed or `"fit"` to content)
- Directory scanning (`--dir`, `--ext` CLI flags) with thumbnail grid
- Fuzzy search — typed characters filter items, centered in the search bar
- Keyboard nav (arrow keys, Enter to select, Escape to exit)
- Mouse input (click to select, scroll wheel to page)
- Thumbnail caching to `~/.cache/thumbnails/wallrun/` (keyed by path + mtime + dimensions)
- Scroll offset with auto-scroll to keep selection visible

### Architecture
- `App::draw()` renders to a `tiny_skia::Pixmap`, copies RGBA→BGRA into SHM buffer
- `App::handle_key()` handles all keyboard input (navigation, typing, selection)
- `PointerHandler` handles click-to-select and scroll
- `grid_metrics()` computes layout from window size and column count
- `load_items()` scans directory, loads/caches thumbnails
- calloop `EventLoop` + `WaylandSource` for keyboard repeat support

## Configuration

### TOML config — `~/.config/wallrun/config.toml`

All fields optional, defaults shown:

```toml
columns = 3
window_width = 800        # or "fit" (auto-size from column count)
window_height = 600       # or "fit" (auto-size to show all items)
font_size = 20.0
label_font_size = 14.0
color_file = "~/.cache/wal/colors-wallrun"
```

`window_width = "fit"` sizes to 256px per column. `window_height = "fit"` sizes to fit all items with no scrolling.

### Color theming — walrs integration

wallrun reads colors from a file specified by `color_file` in config. Format is `key=value` with hex colors:

```
background=#1a1a2e
bar_bg=#2a2a4e
bar_border=#4a4a6e
text=#e0e0e0
text_placeholder=#808080
label=#c0c0c0
selection=#404090
```

A walrs template at `~/.config/walrs/templates/colors-wallrun` generates this file automatically when `walrs -i <image>` runs. Missing file or keys fall back to compiled-in defaults.

## sctk 0.20 notes

- Must explicitly call `get_keyboard_with_repeat()` in `new_capability` — requires calloop `LoopHandle` and `RepeatCallback`
- Must explicitly call `get_pointer()` in `new_capability` for pointer events
- `PointerHandler` has a single `pointer_frame()` method receiving `&[PointerEvent]`
- `PointerEventKind::Axis` has `absolute: f64` for scroll amount; BTN_LEFT = `0x110`
- `delegate_keyboard!`, `delegate_pointer!` macros generate dispatch glue
- `KeyboardHandler` requires `repeat_key` method (in addition to press/release)
- `CompositorHandler` requires `surface_enter` and `surface_leave` methods
- `ProvidesRegistryState` needs `registry_handlers!` macro
