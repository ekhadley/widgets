# widgets

Bespoke Wayland desktop widgets written from scratch in Rust against smithay-client-toolkit + tiny-skia. No widget frameworks.

## Widgets

- **panel** — floating status panel (clock, pomodoro timers, volume control, theme toggle)
- **wallrun** — image picker overlay (thumbnail grid, fuzzy search, keyboard + mouse nav)

## Stack

- smithay-client-toolkit 0.20 (Wayland layer-shell, input, SHM buffers, calloop)
- tiny-skia (CPU rendering)
- cosmic-text (text shaping/glyph rendering)
- walrs (colorscheme integration from wallpaper)

## Build

Each widget is its own Cargo project. From a widget directory:

```
make install   # builds release and copies to ~/.local/bin/
```

## Config

- Config files: `~/.config/widgets/<name>.toml`
- State files: `~/.local/state/widgets/<name>/`
- Color templates: `~/.config/walrs/templates/`
