# wallrun

A lightweight dmenu-style launcher for Hyprland, built with smithay-client-toolkit and tiny-skia. Replaces wofi for use cases requiring image previews and grid layout — starting with a wallpaper picker.

## Core behavior

wallrun is a layer-shell overlay that presents a filterable grid of options, each optionally backed by a thumbnail. It reads candidates from stdin (or a directory scan mode), renders them in a configurable grid with fuzzy search, and prints the selection to stdout. It exits on selection or Escape.

## Wallpaper picker mode

Replicates the `paper --launcher` workflow: scan a wallpaper directory for images, display thumbnails in a 3-column grid, and output the selected path. Downstream, the existing `paper` script handles hyprctl IPC, hyprpaper config persistence, walrs colorscheme generation, and dunst reload.

## Stack

- **smithay-client-toolkit** — Wayland client bindings with first-class `wlr-layer-shell` support via `layer` module
- **tiny-skia** — CPU software rasterizer (Skia path-rendering subset, no GPU dependency)
- **cosmic-text** — text shaping and layout (handles font fallback, subpixel positioning)
- **image** — thumbnail decoding and scaling

## Rendering

All rendering is software-rasterized to an SHM buffer. No EGL/Vulkan. Frame damage is tracked per-region so only the search bar and affected grid cells are redrawn on input.

## Input

- Keyboard: typing filters candidates (fuzzy match on filename/label), arrow keys navigate grid, Enter selects, Escape exits
- Mouse: click to select, scroll to page through results

## Interface layout

```
┌──────────────────────────────────┐
│  [search field]                  │
├──────────┬──────────┬────────────┤
│ ┌──────┐ │ ┌──────┐ │ ┌──────┐  │
│ │thumb │ │ │thumb │ │ │thumb │  │
│ └──────┘ │ └──────┘ │ └──────┘  │
│  label   │  label   │  label    │
├──────────┼──────────┼────────────┤
│ ┌──────┐ │ ┌──────┐ │ ┌──────┐  │
│ │thumb │ │ │thumb │ │ │thumb │  │
│ └──────┘ │ └──────┘ │ └──────┘  │
│  label   │  label   │  label    │
└──────────┴──────────┴────────────┘
```

## Configuration

CLI flags or a TOML config for: column count, thumbnail size, window dimensions, fonts, colors.

### Color theming — walrs integration

wallrun's color scheme must be composable with [walrs](~/wgmn/walrs). walrs generates colorscheme files from wallpapers into `~/.cache/wal/` via templates in `~/.config/walrs/templates/`. wallrun participates in this pipeline:

1. **A walrs template** (`~/.config/walrs/templates/colors-wallrun`) defines wallrun's colors using walrs template variables (e.g. `{background}`, `{foreground}`, `{color0}`–`{color15}`). The template outputs a file that wallrun can read at runtime.
2. **At launch**, wallrun reads its colors from the walrs-generated file at `~/.cache/wal/colors-wallrun` (falling back to compiled-in defaults if the file is missing).
3. This means running `walrs -i <image>` (via the `paper` script) automatically updates wallrun's colors for the next launch — no rebuild or manual config needed.

## Modes

- **Directory mode** (`wallrun --dir ~/wallpapers --ext png,jpg,webp`): scan directory, generate thumbnails, output selected path
- **Stdin mode** (`echo -e "opt1\nopt2" | wallrun`): plain text dmenu-style, no thumbnails
- **Stdin with images** (`printf 'img:/path\tlabel\n' | wallrun`): tab-separated image path + label, matching wofi's `--allow-images` protocol
