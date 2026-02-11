use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;
use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent};
use serde::Deserialize;
use smithay_client_toolkit as sctk;
use sctk::reexports::calloop::{EventLoop, LoopHandle};
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::registry_handlers;
use sctk::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use sctk::shell::WaylandSurface;
use sctk::shm::slot::SlotPool;
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use tiny_skia::Pixmap;
use wayland_client::{Connection, QueueHandle};

// --- Config ---

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Dimension {
    Fixed(u32),
    Auto(#[allow(dead_code)] String),
}

impl Default for Dimension {
    fn default() -> Self { Dimension::Fixed(0) }
}

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    columns: usize,
    window_width: Dimension,
    window_height: Dimension,
    font_size: f32,
    label_font_size: f32,
    color_file: Option<String>,
    show_labels: bool,
    font_family: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self { columns: 3, window_width: Dimension::Fixed(800), window_height: Dimension::Fixed(600),
               font_size: 20.0, label_font_size: 14.0, color_file: None, show_labels: true,
               font_family: None }
    }
}

fn load_config() -> Config {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".config"));
    let path = base.join("widgets/wallrun.toml");
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Config::default(),
    };
    match toml::from_str(&content) {
        Ok(cfg) => {
            eprintln!("wallrun: loaded config from {}", path.display());
            cfg
        }
        Err(e) => {
            eprintln!("wallrun: failed to parse {}: {e}", path.display());
            Config::default()
        }
    }
}

// --- Colors ---

struct Colors {
    background: [u8; 3],
    background_alpha: u8,
    bar_bg: [u8; 3],
    bar_border: [u8; 3],
    text: [u8; 3],
    text_placeholder: [u8; 3],
    label: [u8; 3],
    selection: [u8; 3],
    selection_alpha: u8,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            background: [0x1a, 0x1a, 0x2e], background_alpha: 0xff,
            bar_bg: [0x2a, 0x2a, 0x4e],
            bar_border: [0x4a, 0x4a, 0x6e], text: [0xe0, 0xe0, 0xe0],
            text_placeholder: [0x80, 0x80, 0x80], label: [0xc0, 0xc0, 0xc0],
            selection: [0x40, 0x40, 0x90], selection_alpha: 0xff,
        }
    }
}

fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 { return None; }
    Some([u8::from_str_radix(&s[0..2], 16).ok()?,
          u8::from_str_radix(&s[2..4], 16).ok()?,
          u8::from_str_radix(&s[4..6], 16).ok()?])
}

fn expand_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME").unwrap()).join(rest)
    } else { PathBuf::from(p) }
}

fn load_colors(path: Option<&str>) -> Colors {
    let mut colors = Colors::default();
    let content = match path {
        Some(p) => std::fs::read_to_string(expand_path(p)).unwrap_or_default(),
        None => return colors,
    };
    for line in content.lines() {
        if let Some((key, val)) = line.split_once('=') {
            let (key, val) = (key.trim(), val.trim());
            match key {
                "background_opacity" | "selection_opacity" => {
                    if let Ok(f) = val.parse::<f32>() {
                        let a = (f.clamp(0.0, 1.0) * 255.0) as u8;
                        match key {
                            "background_opacity" => colors.background_alpha = a,
                            _ => colors.selection_alpha = a,
                        }
                    }
                }
                _ => {
                    if let Some(c) = parse_hex(val) {
                        match key {
                            "background" => colors.background = c,
                            "bar_bg" => colors.bar_bg = c,
                            "bar_border" => colors.bar_border = c,
                            "text" => colors.text = c,
                            "text_placeholder" => colors.text_placeholder = c,
                            "label" => colors.label = c,
                            "selection" => colors.selection = c,
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    colors
}

// --- App ---

struct Item {
    path: PathBuf,
    label: String,
    thumb_data: Vec<u8>,
    thumb_w: u32,
    thumb_h: u32,
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    pool: SlotPool,
    width: u32,
    height: u32,
    exit: bool,
    input: String,
    font_system: FontSystem,
    swash_cache: SwashCache,
    loop_handle: LoopHandle<'static, App>,
    items: Vec<Item>,
    filtered: Vec<usize>,
    selected: usize,
    scroll_offset: usize,
    cols: usize,
    colors: Colors,
    font_size: f32,
    label_font_size: f32,
    show_labels: bool,
    font_family: Option<String>,
}

const PAD: f32 = 16.0;
const CELL_PAD: f32 = 12.0;
const BAR_H: u32 = 50;

impl App {
    fn effective_cols(&self) -> usize {
        let n = self.filtered.len();
        if n == 0 { return self.cols; }
        let sqrt = (n as f32).sqrt().ceil() as usize;
        sqrt.min(self.cols).max(1)
    }

    fn grid_metrics(&self) -> (f32, f32, u32, u32, f32, f32, usize) {
        let grid_top = BAR_H as f32 + 12.0;
        let cell_w = (self.width as f32 - PAD * 2.0) / self.cols as f32;
        let thumb_w = (cell_w - CELL_PAD) as u32;
        let thumb_h = (thumb_w as f32 * 0.67) as u32;
        let label_h = if self.show_labels { 28.0f32 } else { 0.0 };
        let cell_h = thumb_h as f32 + label_h + CELL_PAD;
        let rows = ((self.height as f32 - grid_top) / cell_h).max(0.0) as usize;
        let visible = rows * self.effective_cols();
        (grid_top, cell_w, thumb_w, thumb_h, label_h, cell_h, visible)
    }

    fn grid_offsets(&self) -> (f32, f32) {
        let (grid_top, cell_w, _, _, _, cell_h, visible) = self.grid_metrics();
        let ecols = self.effective_cols();
        let start = self.scroll_offset;
        let on_screen = (start + visible).min(self.filtered.len()) - start;
        let eff_cols = if on_screen == 0 { ecols } else { on_screen.min(ecols) };
        let x_off = (self.width as f32 - eff_cols as f32 * cell_w) / 2.0;
        let total_rows = if on_screen == 0 { 0 } else { (on_screen + ecols - 1) / ecols };
        let avail_h = self.height as f32 - grid_top;
        let grid_h = total_rows as f32 * cell_h;
        let y_off = if grid_h < avail_h { (avail_h - grid_h) / 2.0 } else { 0.0 };
        (x_off, y_off)
    }

    fn ensure_visible(&mut self) {
        let (_, _, _, _, _, _, visible) = self.grid_metrics();
        if visible == 0 { return; }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        if self.selected >= self.scroll_offset + visible {
            self.scroll_offset = self.selected - visible + 1;
        }
    }

    fn refilter(&mut self) {
        self.filtered = if self.input.is_empty() {
            (0..self.items.len()).collect()
        } else {
            (0..self.items.len())
                .filter(|&i| fuzzy_match(&self.items[i].label, &self.input))
                .collect()
        };
        self.selected = 0;
        self.scroll_offset = 0;
    }

    fn handle_key(&mut self, event: &KeyEvent) {
        if event.keysym == Keysym::Escape {
            self.exit = true;
            return;
        }
        if event.keysym == Keysym::Return && !self.filtered.is_empty() {
            println!("{}", self.items[self.filtered[self.selected]].path.display());
            self.exit = true;
            return;
        }
        let n = self.filtered.len();
        let cols = self.effective_cols();
        let changed = match event.keysym {
            Keysym::BackSpace => {
                if self.input.pop().is_some() { self.refilter(); true } else { false }
            }
            Keysym::Left if self.selected > 0 => { self.selected -= 1; true }
            Keysym::Right if self.selected + 1 < n => { self.selected += 1; true }
            Keysym::Up if self.selected >= cols => { self.selected -= cols; true }
            Keysym::Down if self.selected + cols < n => { self.selected += cols; true }
            _ => match event.utf8 {
                Some(ref text) if !text.is_empty() && text.chars().all(|c| !c.is_control()) => {
                    self.input.push_str(text);
                    self.refilter();
                    true
                }
                _ => false,
            },
        };
        if changed {
            self.ensure_visible();
            self.draw();
        }
    }

    fn draw(&mut self) {
        let (grid_top, cell_w, thumb_w, thumb_h, label_h, cell_h, visible) = self.grid_metrics();
        let (x_off, y_off) = self.grid_offsets();
        let cols = self.effective_cols();
        let c = &self.colors;
        let bg = c.background;
        let bar_bg = c.bar_bg;
        let bar_border = c.bar_border;
        let text_color = c.text;
        let label_color = c.label;
        let sel_color = c.selection;

        let stride = self.width as i32 * 4;
        let (wl_buf, canvas) = self.pool
            .create_buffer(self.width as i32, self.height as i32, stride, wl_shm::Format::Argb8888)
            .unwrap();

        let mut pixmap = Pixmap::new(self.width, self.height).unwrap();
        pixmap.fill(tiny_skia::Color::from_rgba8(bg[0], bg[1], bg[2], c.background_alpha));

        let pw = pixmap.width();
        let ph = pixmap.height();

        // Search bar
        fill_rect(pixmap.data_mut(), pw, ph, 0, 0, self.width, BAR_H, bar_bg);

        // Window outline
        fill_rect(pixmap.data_mut(), pw, ph, 0, 0, self.width, 2, bar_border);
        fill_rect(pixmap.data_mut(), pw, ph, 0, self.height - 2, self.width, 2, bar_border);
        fill_rect(pixmap.data_mut(), pw, ph, 0, 0, 2, self.height, bar_border);
        fill_rect(pixmap.data_mut(), pw, ph, self.width - 2, 0, 2, self.height, bar_border);

        if !self.input.is_empty() {
            let text_y = (BAR_H as f32 + self.font_size) / 2.0;
            let text_w = measure_text(&mut self.font_system, &self.input, self.font_size, &self.font_family);
            let text_x = (self.width as f32 - text_w) / 2.0;
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                &self.input, text_x, text_y, self.font_size, self.width as f32, BAR_H as f32, text_color,
                &self.font_family);
        }

        // Grid
        let start = self.scroll_offset;
        let end = (start + visible).min(self.filtered.len());

        for i in start..end {
            let vis_pos = (i - start) as u32;
            let item_idx = self.filtered[i];
            let col = vis_pos % cols as u32;
            let row = vis_pos / cols as u32;
            let cx = x_off + col as f32 * cell_w + CELL_PAD / 2.0;
            let cy = grid_top + y_off + row as f32 * cell_h;

            let tw = self.items[item_idx].thumb_w;
            let th = self.items[item_idx].thumb_h;
            let tx = cx + (thumb_w as f32 - tw as f32) / 2.0;
            let ty = cy + (thumb_h as f32 - th as f32) / 2.0;
            blit_rgba(pixmap.data_mut(), pw as i32, ph as i32,
                tx as i32, ty as i32, tw as i32, th as i32, &self.items[item_idx].thumb_data);

            if i == self.selected {
                let bw: u32 = 2;
                let bx = (tx as u32).saturating_sub(bw);
                let by = (ty as u32).saturating_sub(bw);
                let bwidth = tw + bw * 2;
                let bheight = th + bw * 2;
                // top
                fill_rect(pixmap.data_mut(), pw, ph, bx, by, bwidth, bw, sel_color);
                // bottom
                fill_rect(pixmap.data_mut(), pw, ph, bx, by + bheight - bw, bwidth, bw, sel_color);
                // left
                fill_rect(pixmap.data_mut(), pw, ph, bx, by, bw, bheight, sel_color);
                // right
                fill_rect(pixmap.data_mut(), pw, ph, bx + bwidth - bw, by, bw, bheight, sel_color);
            }

            if self.show_labels {
                render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                    &self.items[item_idx].label, cx, cy + thumb_h as f32 + 4.0,
                    self.label_font_size, thumb_w as f32, label_h, label_color,
                    &self.font_family);
            }
        }

        // Copy RGBA premul -> BGRA (ARGB8888 on LE)
        for (dst, src) in canvas.chunks_exact_mut(4).zip(pixmap.data().chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }

        wl_buf.attach_to(self.layer.wl_surface()).unwrap();
        self.layer.wl_surface().damage_buffer(0, 0, self.width as i32, self.height as i32);
        self.layer.wl_surface().commit();
    }
}

// --- Rendering helpers ---

fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let h = haystack.to_lowercase();
    let n = needle.to_lowercase();
    let mut hi = h.chars();
    for nc in n.chars() {
        if !hi.any(|hc| hc == nc) { return false; }
    }
    true
}

fn fill_rect_alpha(data: &mut [u8], pw: u32, ph: u32, x: u32, y: u32, w: u32, h: u32, c: [u8; 3], a: u8) {
    if a == 0xff { return fill_rect(data, pw, ph, x, y, w, h, c); }
    let a = a as u32;
    let inv = 255 - a;
    for py in y..y.saturating_add(h).min(ph) {
        for px in x..x.saturating_add(w).min(pw) {
            let i = (py as usize * pw as usize + px as usize) * 4;
            data[i]     = ((c[0] as u32 * a + data[i] as u32 * inv) / 255) as u8;
            data[i + 1] = ((c[1] as u32 * a + data[i + 1] as u32 * inv) / 255) as u8;
            data[i + 2] = ((c[2] as u32 * a + data[i + 2] as u32 * inv) / 255) as u8;
            data[i + 3] = ((a + data[i + 3] as u32 * inv / 255)) as u8;
        }
    }
}

fn fill_rect(data: &mut [u8], pw: u32, ph: u32, x: u32, y: u32, w: u32, h: u32, c: [u8; 3]) {
    for py in y..y.saturating_add(h).min(ph) {
        for px in x..x.saturating_add(w).min(pw) {
            let i = (py as usize * pw as usize + px as usize) * 4;
            data[i] = c[0];
            data[i + 1] = c[1];
            data[i + 2] = c[2];
            data[i + 3] = 0xff;
        }
    }
}

fn make_attrs<'a>(family: &'a Option<String>) -> Attrs<'a> {
    match family {
        Some(name) => Attrs::new().family(Family::Name(name)),
        None => Attrs::new(),
    }
}

fn measure_text(font_system: &mut FontSystem, text: &str, font_size: f32, family: &Option<String>) -> f32 {
    let mut buf = Buffer::new(font_system, Metrics::new(font_size, font_size * 1.2));
    buf.set_size(font_system, None, None);
    buf.set_text(font_system, text, &make_attrs(family), Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);
    buf.layout_runs().next().map_or(0.0, |r| r.line_w)
}

fn render_text(
    pixmap: &mut Pixmap, font_system: &mut FontSystem, swash_cache: &mut SwashCache,
    text: &str, x: f32, y: f32, font_size: f32, max_w: f32, max_h: f32, color: [u8; 3],
    family: &Option<String>,
) {
    let line_h = font_size * 1.2;
    let mut buf = Buffer::new(font_system, Metrics::new(font_size, line_h));
    buf.set_size(font_system, Some(max_w), Some(max_h));
    buf.set_text(font_system, text, &make_attrs(family), Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);

    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    for run in buf.layout_runs() {
        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((x, y), 1.0);
            if let Some(image) = swash_cache.get_image_uncached(font_system, physical.cache_key) {
                let x0 = physical.x + image.placement.left;
                let y0 = physical.y - image.placement.top;
                let w = image.placement.width as i32;
                let h = image.placement.height as i32;
                match image.content {
                    SwashContent::Mask => blit_mask(pixmap.data_mut(), pw, ph, x0, y0, w, h, &image.data, &color),
                    SwashContent::Color => blit_color(pixmap.data_mut(), pw, ph, x0, y0, w, h, &image.data),
                    SwashContent::SubpixelMask => {}
                }
            }
        }
    }
}

fn blit_mask(data: &mut [u8], pw: i32, ph: i32, x0: i32, y0: i32, w: i32, h: i32, mask: &[u8], color: &[u8; 3]) {
    for gy in 0..h {
        let py = y0 + gy;
        if py < 0 || py >= ph { continue; }
        for gx in 0..w {
            let px = x0 + gx;
            if px < 0 || px >= pw { continue; }
            let a = mask[(gy * w + gx) as usize] as u32;
            if a == 0 { continue; }
            let i = (py * pw + px) as usize * 4;
            let inv = 255 - a;
            data[i]     = ((color[0] as u32 * a + data[i] as u32 * inv) / 255) as u8;
            data[i + 1] = ((color[1] as u32 * a + data[i + 1] as u32 * inv) / 255) as u8;
            data[i + 2] = ((color[2] as u32 * a + data[i + 2] as u32 * inv) / 255) as u8;
            data[i + 3] = ((a + data[i + 3] as u32 * inv / 255)) as u8;
        }
    }
}

fn blit_rgba(data: &mut [u8], pw: i32, ph: i32, x0: i32, y0: i32, w: i32, h: i32, src: &[u8]) {
    for gy in 0..h {
        let py = y0 + gy;
        if py < 0 || py >= ph { continue; }
        for gx in 0..w {
            let px = x0 + gx;
            if px < 0 || px >= pw { continue; }
            let si = (gy * w + gx) as usize * 4;
            let di = (py * pw + px) as usize * 4;
            data[di] = src[si];
            data[di + 1] = src[si + 1];
            data[di + 2] = src[si + 2];
            data[di + 3] = src[si + 3];
        }
    }
}

fn blit_color(data: &mut [u8], pw: i32, ph: i32, x0: i32, y0: i32, w: i32, h: i32, rgba: &[u8]) {
    for gy in 0..h {
        let py = y0 + gy;
        if py < 0 || py >= ph { continue; }
        for gx in 0..w {
            let px = x0 + gx;
            if px < 0 || px >= pw { continue; }
            let si = (gy * w + gx) as usize * 4;
            let a = rgba[si + 3] as u32;
            if a == 0 { continue; }
            let i = (py * pw + px) as usize * 4;
            let inv = 255 - a;
            data[i]     = (rgba[si] as u32 * a / 255 + data[i] as u32 * inv / 255) as u8;
            data[i + 1] = (rgba[si + 1] as u32 * a / 255 + data[i + 1] as u32 * inv / 255) as u8;
            data[i + 2] = (rgba[si + 2] as u32 * a / 255 + data[i + 2] as u32 * inv / 255) as u8;
            data[i + 3] = (a + data[i + 3] as u32 * inv / 255) as u8;
        }
    }
}

// --- Wayland handler boilerplate ---

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = Some(self.seat_state.get_keyboard_with_repeat(
                qh, &seat, None,
                self.loop_handle.clone(),
                Box::new(|state, _wl_kbd, event| {
                    state.handle_key(&event);
                }),
            ).unwrap());
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = Some(self.seat_state.get_pointer(qh, &seat).unwrap());
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.handle_key(&event);
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.handle_key(&event);
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _: RawModifiers, _: u32) {}
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        let mut redraw = false;
        for event in events {
            match event.kind {
                PointerEventKind::Press { button: 0x110, .. } => {
                    let (grid_top, cell_w, _, _, _, cell_h, _) = self.grid_metrics();
                    let (x_off, y_off) = self.grid_offsets();
                    let ecols = self.effective_cols();
                    let (mx, my) = (event.position.0 as f32, event.position.1 as f32);
                    if mx >= x_off && my > grid_top + y_off {
                        let row = ((my - grid_top - y_off) / cell_h) as usize;
                        let col = ((mx - x_off) / cell_w) as usize;
                        if col < ecols {
                            let idx = self.scroll_offset + row * ecols + col;
                            if idx < self.filtered.len() {
                                self.selected = idx;
                                println!("{}", self.items[self.filtered[idx]].path.display());
                                self.exit = true;
                                return;
                            }
                        }
                    }
                }
                PointerEventKind::Motion { .. } => {
                    let (grid_top, cell_w, _, _, _, cell_h, _) = self.grid_metrics();
                    let (x_off, y_off) = self.grid_offsets();
                    let ecols = self.effective_cols();
                    let (mx, my) = (event.position.0 as f32, event.position.1 as f32);
                    if mx >= x_off && my > grid_top + y_off {
                        let row = ((my - grid_top - y_off) / cell_h) as usize;
                        let col = ((mx - x_off) / cell_w) as usize;
                        if col < ecols {
                            let idx = self.scroll_offset + row * ecols + col;
                            if idx < self.filtered.len() && idx != self.selected {
                                self.selected = idx;
                                redraw = true;
                            }
                        }
                    }
                }
                PointerEventKind::Axis { ref vertical, .. } => {
                    let (_, _, _, _, _, _, visible) = self.grid_metrics();
                    let cols = self.effective_cols();
                    if vertical.absolute > 0.0 && self.scroll_offset + visible < self.filtered.len() {
                        self.scroll_offset = (self.scroll_offset + cols)
                            .min(self.filtered.len().saturating_sub(visible));
                        redraw = true;
                    } else if vertical.absolute < 0.0 && self.scroll_offset > 0 {
                        self.scroll_offset = self.scroll_offset.saturating_sub(cols);
                        redraw = true;
                    }
                }
                _ => {}
            }
        }
        if redraw { self.draw(); }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        if configure.new_size.0 > 0 { self.width = configure.new_size.0; }
        if configure.new_size.1 > 0 { self.height = configure.new_size.1; }
        self.draw();
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);

// --- Thumbnail loading ---

fn cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".cache"));
    base.join("thumbnails/wallrun")
}

fn cache_key(path: &Path, thumb_w: u32, thumb_h: u32) -> Option<String> {
    let mtime = path.metadata().ok()?.modified().ok()?;
    let canonical = path.canonicalize().ok()?;
    let mut h = DefaultHasher::new();
    canonical.hash(&mut h);
    mtime.hash(&mut h);
    thumb_w.hash(&mut h);
    thumb_h.hash(&mut h);
    Some(format!("{:016x}", h.finish()))
}

fn load_thumbnail(path: &Path, cache_dir: &Path, thumb_w: u32, thumb_h: u32) -> Option<(Vec<u8>, u32, u32)> {
    let key = cache_key(path, thumb_w, thumb_h)?;
    let cached = cache_dir.join(format!("{key}.png"));

    if cached.exists() {
        if let Ok(img) = image::open(&cached) {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            return Some((rgba.into_raw(), w, h));
        }
    }

    let img = image::open(path).ok()?;
    let thumb = img.resize(thumb_w, thumb_h, image::imageops::FilterType::Triangle);
    let rgba = thumb.to_rgba8();
    let (w, h) = rgba.dimensions();
    rgba.save(&cached).ok();
    Some((rgba.into_raw(), w, h))
}

fn load_items(dir: &str, exts: &[String], thumb_w: u32, thumb_h: u32) -> Vec<Item> {
    let cd = cache_dir();
    std::fs::create_dir_all(&cd).ok();

    let mut items = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => { eprintln!("wallrun: cannot read {dir}: {e}"); return items; }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = match path.extension() {
            Some(e) => e.to_string_lossy().to_lowercase(),
            None => continue,
        };
        if !exts.iter().any(|e| e.eq_ignore_ascii_case(&ext)) { continue; }
        let label = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        match load_thumbnail(&path, &cd, thumb_w, thumb_h) {
            Some((data, tw, th)) => items.push(Item { path, label, thumb_data: data, thumb_w: tw, thumb_h: th }),
            None => eprintln!("wallrun: skip {}", path.display()),
        }
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

// --- Main ---

fn main() {
    let cfg = load_config();
    let colors = load_colors(cfg.color_file.as_deref());

    let args: Vec<String> = std::env::args().collect();
    let mut dir: Option<String> = None;
    let mut exts: Vec<String> = ["png", "jpg", "jpeg", "webp"].iter().map(|s| s.to_string()).collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" if i + 1 < args.len() => { dir = Some(args[i + 1].clone()); i += 2; }
            "--ext" if i + 1 < args.len() => { exts = args[i + 1].split(',').map(String::from).collect(); i += 2; }
            _ => { eprintln!("wallrun: unknown arg: {}", args[i]); i += 1; }
        }
    }

    let cols = cfg.columns;

    // Resolve width (fit = auto-size based on column count)
    let width = match cfg.window_width {
        Dimension::Fixed(w) => w,
        Dimension::Auto(_) => (256.0 * cols as f32 + PAD * 2.0) as u32,
    };

    let cell_w = (width as f32 - PAD * 2.0) / cols as f32;
    let thumb_w = (cell_w - CELL_PAD) as u32;
    let thumb_h = (thumb_w as f32 * 0.67) as u32;

    let items = match dir {
        Some(ref d) => load_items(d, &exts, thumb_w, thumb_h),
        None => Vec::new(),
    };

    // Resolve height (fit = auto-size to show all items)
    let height = match cfg.window_height {
        Dimension::Fixed(h) => h,
        Dimension::Auto(_) => {
            let rows = if items.is_empty() { 1 } else { (items.len() + cols - 1) / cols };
            let grid_top = BAR_H as f32 + 12.0;
            let label_h = if cfg.show_labels { 28.0 } else { 0.0 };
            let cell_h = thumb_h as f32 + label_h + CELL_PAD;
            (grid_top + rows as f32 * cell_h + CELL_PAD) as u32
        }
    };

    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<App> = EventLoop::try_new().unwrap();
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue).insert(loop_handle).unwrap();

    let compositor = CompositorState::bind(&globals, &qh).unwrap();
    let layer_shell = LayerShell::bind(&globals, &qh).unwrap();
    let shm = Shm::bind(&globals, &qh).unwrap();

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("wallrun"), None);
    layer.set_size(width, height);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.wl_surface().commit();

    let pool = SlotPool::new((width * height * 4) as usize, &shm).unwrap();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        keyboard: None,
        pointer: None,
        pool,
        width,
        height,
        exit: false,
        input: String::new(),
        font_system: FontSystem::new(),
        swash_cache: SwashCache::new(),
        loop_handle: event_loop.handle(),
        filtered: (0..items.len()).collect(),
        items,
        selected: 0,
        scroll_offset: 0,
        cols,
        colors,
        font_size: cfg.font_size,
        label_font_size: cfg.label_font_size,
        show_labels: cfg.show_labels,
        font_family: cfg.font_family,
    };

    loop {
        event_loop.dispatch(Duration::from_millis(16), &mut app).unwrap();
        if app.exit { break; }
    }
}
