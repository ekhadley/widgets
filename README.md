# widgets

Bespoke Wayland desktop widgets written from scratch in Rust against smithay-client-toolkit + tiny-skia. No widget frameworks.

## Widgets

### raven
Floating status overlay — clock, pomodoro timers, volume control, theme toggle.

![raven](screenshots/raven.png)

### wallrun
Image picker overlay — thumbnail grid, fuzzy search, keyboard + mouse nav.

![wallrun](screenshots/wallrun.png)

### grimoire
App launcher / dmenu replacement — filterable list with icons, fuzzy search.

![grimoire](screenshots/grimoire.png)

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
