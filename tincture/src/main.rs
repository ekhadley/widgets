use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping, SwashCache, SwashContent};
use serde::Deserialize;
use smithay_client_toolkit as sctk;
use sctk::reexports::calloop::timer::{TimeoutAction, Timer};
use sctk::reexports::calloop::{EventLoop, LoopHandle};
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::registry_handlers;
use sctk::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::pointer::cursor_shape::CursorShapeManager;
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape;
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shell::xdg::XdgShell;
use sctk::shell::WaylandSurface;
use sctk::shm::slot::SlotPool;
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};
use wayland_client::{Connection, QueueHandle};

// --- Layout ---

// Everything below is design space; S scales it to pixels at the drawing primitives, so
// layout numbers, hit tests and font sizes all stay in one coordinate system.
const S: f32 = 1.2;
const WIDTH: u32 = 820;
const HEIGHT: u32 = 560;
const PW: u32 = (WIDTH as f32 * S) as u32;
const PH: u32 = (HEIGHT as f32 * S) as u32;
const PAD: f32 = 20.0;

const WHEEL_CX: f32 = 250.0;
const WHEEL_CY: f32 = 250.0;
const WHEEL_R: f32 = 210.0;
const MAX_C: f32 = 0.33;

const STRIP_X: f32 = 480.0;
const STRIP_W: f32 = 36.0;
const STRIP_Y: f32 = 40.0;
const STRIP_H: f32 = 420.0;

const PANEL_X: f32 = 548.0;

const RAIL_Y: f32 = 484.0;
const RAIL_H: f32 = 52.0;
const CHIP_GAP: f32 = 5.0;

const DOT_R: f32 = 11.0;
const STRIP_DOT_R: f32 = 5.6;

/// Both buttons close the window; `revert` restores the palette we launched with first.
const BUTTONS: [&str; 2] = ["revert", "keep"];
const BTN_SIZE: f32 = 14.0;
const BTN_H: f32 = 26.0;
const BTN_TOP: f32 = 10.0;
const BTN_RIGHT: f32 = WIDTH as f32 - 10.0;
/// Baseline that centers the text's cap-height box inside the BTN_H hover box.
const BTN_BASE: f32 = BTN_TOP + BTN_H / 2.0 + BTN_SIZE * 0.36;

// --- Config ---

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    font: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { font: "~/.local/share/fonts/GoogleSansCode-Regular.ttf".into() }
    }
}

fn expand_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME").unwrap()).join(rest)
    } else { PathBuf::from(p) }
}

fn load_config() -> Config {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".config"));
    let path = base.join("widgets/tincture.toml");
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Config::default(),
    };
    match toml::from_str(&content) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("tincture: failed to parse {}: {e}", path.display());
            Config::default()
        }
    }
}

// --- Color: sRGB <-> OKLCH ---

#[derive(Clone, Copy)]
struct Lch { l: f32, c: f32, h: f32 }

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

fn rgb_to_lch(rgb: [u8; 3]) -> Lch {
    let r = srgb_to_linear(rgb[0] as f32 / 255.0);
    let g = srgb_to_linear(rgb[1] as f32 / 255.0);
    let b = srgb_to_linear(rgb[2] as f32 / 255.0);
    let l_ = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m_ = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s_ = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    let l = 0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_;
    let a = 1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_;
    let bb = 0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_;
    Lch { l, c: (a * a + bb * bb).sqrt(), h: bb.atan2(a).to_degrees().rem_euclid(360.0) }
}

/// OKLab -> sRGB floats, plus whether the result was inside the sRGB gamut.
fn oklab_to_srgb(l: f32, a: f32, b: f32) -> ([f32; 3], bool) {
    let l_ = (l + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let m_ = (l - 0.1055613458 * a - 0.0638541728 * b).powi(3);
    let s_ = (l - 0.0894841775 * a - 1.2914855480 * b).powi(3);
    let lin = [
         4.0767416621 * l_ - 3.3077115913 * m_ + 0.2309699292 * s_,
        -1.2684380046 * l_ + 2.6097574011 * m_ - 0.3413193965 * s_,
        -0.0041960863 * l_ - 0.7034186147 * m_ + 1.7076147010 * s_,
    ];
    let inside = lin.iter().all(|v| *v >= -0.001 && *v <= 1.001);
    let out = [
        linear_to_srgb(lin[0].clamp(0.0, 1.0)),
        linear_to_srgb(lin[1].clamp(0.0, 1.0)),
        linear_to_srgb(lin[2].clamp(0.0, 1.0)),
    ];
    (out, inside)
}

/// Convert to sRGB, reducing chroma until the color fits in the gamut.
fn lch_to_rgb(col: Lch) -> [u8; 3] {
    let (rad, l) = (col.h.to_radians(), col.l.clamp(0.0, 1.0));
    let (mut lo, mut hi) = (0.0, col.c);
    if !oklab_to_srgb(l, hi * rad.cos(), hi * rad.sin()).1 {
        for _ in 0..16 {
            let mid = (lo + hi) / 2.0;
            if oklab_to_srgb(l, mid * rad.cos(), mid * rad.sin()).1 { lo = mid } else { hi = mid }
        }
    } else { lo = hi }
    let (f, _) = oklab_to_srgb(l, lo * rad.cos(), lo * rad.sin());
    [(f[0] * 255.0 + 0.5) as u8, (f[1] * 255.0 + 0.5) as u8, (f[2] * 255.0 + 0.5) as u8]
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

// --- Scheme file ---

fn sanitize_path(image_path: &str) -> String {
    image_path.trim_start_matches('/').replace('/', "_").replace('.', "_")
}

fn cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".cache"))
        .join("wal")
}

fn read_scheme(path: &PathBuf) -> ([Lch; 16], u8) {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| { eprintln!("tincture: cannot read {}: {e}", path.display()); std::process::exit(1) });
    let mut colors = Vec::new();
    let mut alpha = 100u8;
    for line in content.lines() {
        if let Some(hex) = line.trim().strip_prefix('#') {
            if hex.len() >= 6 {
                let v = u32::from_str_radix(&hex[0..6], 16).unwrap();
                colors.push(rgb_to_lch([(v >> 16) as u8, (v >> 8 & 0xff) as u8, (v & 0xff) as u8]));
            }
        } else if let Ok(v) = line.trim().parse::<u8>() {
            alpha = v.min(100);
        }
    }
    if colors.len() < 16 {
        eprintln!("tincture: {} has only {} colors", path.display(), colors.len());
        std::process::exit(1);
    }
    (std::array::from_fn(|i| colors[i]), alpha)
}

// --- App ---

enum Drag {
    Wheel { start: [Lch; 16], press: (f32, f32) },
    Strip { start: [Lch; 16], press: f32 },
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    window: Window,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_shape_manager: CursorShapeManager,
    pool: SlotPool,
    exit: bool,
    font_system: FontSystem,
    swash_cache: SwashCache,
    font_family: String,
    loop_handle: LoopHandle<'static, App>,

    image: String,
    scheme_path: PathBuf,
    colors: [Lch; 16],
    original: [Lch; 16],
    alpha: u8,
    undos: Vec<[Lch; 16]>,
    redos: Vec<[Lch; 16]>,
    last_undo: Instant,
    sel: Vec<usize>,
    primary: usize,
    hover: Option<usize>,
    hover_btn: Option<usize>,
    btn_rects: [(f32, f32, f32, f32); 2],
    drag: Option<Drag>,
    shift: bool,
    ctrl: bool,
    disc: Option<(i32, Vec<u8>)>,
    child: Option<Child>,
    pending: bool,
}

fn chip_w() -> f32 { (WIDTH as f32 - PAD * 2.0 - CHIP_GAP * 15.0) / 16.0 }

fn wheel_pos(c: Lch) -> (f32, f32) {
    let (rad, r) = (c.h.to_radians(), c.c.min(MAX_C) / MAX_C * WHEEL_R);
    (WHEEL_CX + r * rad.cos(), WHEEL_CY - r * rad.sin())
}

fn wheel_ab(x: f32, y: f32) -> (f32, f32) {
    ((x - WHEEL_CX) / WHEEL_R * MAX_C, -(y - WHEEL_CY) / WHEEL_R * MAX_C)
}

fn strip_y(l: f32) -> f32 { STRIP_Y + (1.0 - l.clamp(0.0, 1.0)) * STRIP_H }
fn strip_l(y: f32) -> f32 { (1.0 - (y - STRIP_Y) / STRIP_H).clamp(0.0, 1.0) }

impl App {
    fn rgb(&self) -> [[u8; 3]; 16] { std::array::from_fn(|i| lch_to_rgb(self.colors[i])) }

    // --- editing ---

    fn push_undo(&mut self) {
        self.redos.clear();
        if self.last_undo.elapsed() < Duration::from_millis(400) { return; }
        self.undos.push(self.colors);
        self.last_undo = Instant::now();
        if self.undos.len() > 100 { self.undos.remove(0); }
    }

    fn step_history(&mut self, redo: bool) {
        let (from, to) = if redo { (&mut self.redos, &mut self.undos) } else { (&mut self.undos, &mut self.redos) };
        let Some(colors) = from.pop() else { return };
        to.push(self.colors);
        self.colors = colors;
        self.last_undo = Instant::now() - Duration::from_secs(1);
        self.apply();
    }

    fn nudge(&mut self, dl: f32, dc: f32, dh: f32) {
        self.push_undo();
        for &i in &self.sel {
            let c = &mut self.colors[i];
            c.l = (c.l + dl).clamp(0.0, 1.0);
            c.c = (c.c + dc).clamp(0.0, MAX_C);
            c.h = (c.h + dh).rem_euclid(360.0);
        }
        self.apply();
    }

    fn select(&mut self, idx: usize, add: bool) {
        if add {
            if let Some(p) = self.sel.iter().position(|&s| s == idx) {
                if self.sel.len() > 1 { self.sel.remove(p); }
            } else { self.sel.push(idx); }
        } else if !self.sel.contains(&idx) {
            self.sel = vec![idx];
        }
        self.primary = idx;
    }

    // --- applying ---

    fn write_scheme(&self) {
        let mut s = String::new();
        for c in self.colors {
            let [r, g, b] = lch_to_rgb(c);
            s.push_str(&format!("#{r:02x}{g:02x}{b:02x}\n"));
        }
        s.push_str(&self.alpha.to_string());
        std::fs::write(&self.scheme_path, s).unwrap();
    }

    fn apply(&mut self) {
        self.write_scheme();
        if self.child.is_some() {
            self.pending = true;
        } else {
            self.child = Some(Command::new("walrs").args(["-i", &self.image, "-W", "-q"]).spawn().unwrap());
        }
    }

    fn poll_child(&mut self) {
        if let Some(c) = &mut self.child {
            if c.try_wait().unwrap().is_some() {
                self.child = None;
                if self.pending { self.pending = false; self.apply(); }
            }
        }
    }

    /// Write the current palette and make sure walrs has run against it before we exit.
    fn flush(&mut self) {
        self.write_scheme();
        if let Some(c) = &mut self.child { c.wait().unwrap(); }
        Command::new("walrs").args(["-i", &self.image, "-W", "-q"]).status().unwrap();
    }

    // --- input ---

    /// Painting order: unselected, then selected, primary last. Hit testing walks it backwards,
    /// so a click on overlapping dots lands on whichever one is drawn on top.
    fn dot_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..16).collect();
        order.sort_by_key(|i| (self.sel.contains(i), *i == self.primary));
        order
    }

    fn dot_at(&self, x: f32, y: f32) -> Option<usize> {
        self.dot_order().into_iter().rev().find(|&i| {
            let (dx, dy) = wheel_pos(self.colors[i]);
            (x - dx).hypot(y - dy) <= DOT_R + 3.0
        })
    }

    fn chip_at(&self, x: f32, y: f32) -> Option<usize> {
        if y < RAIL_Y || y > RAIL_Y + RAIL_H { return None; }
        let i = ((x - PAD) / (chip_w() + CHIP_GAP)) as i32;
        (0..16).contains(&i).then_some(i as usize)
    }

    fn in_strip(&self, x: f32, y: f32) -> bool {
        x >= STRIP_X && x <= STRIP_X + STRIP_W && y >= STRIP_Y && y <= STRIP_Y + STRIP_H
    }

    fn btn_at(&self, x: f32, y: f32) -> Option<usize> {
        (0..2).find(|&i| {
            let (bx, by, bw, bh) = self.btn_rects[i];
            x >= bx && x <= bx + bw && y >= by && y <= by + bh
        })
    }

    fn close(&mut self, revert: bool) {
        if revert { self.colors = self.original; }
        self.flush();
        self.exit = true;
    }

    fn handle_key(&mut self, event: &KeyEvent) {
        let step = if self.shift { 5.0 } else { 1.0 };
        if self.ctrl {
            match event.keysym {
                Keysym::z => self.step_history(false),
                Keysym::Z => self.step_history(true),
                Keysym::r => { self.push_undo(); self.colors = self.original; self.apply(); }
                _ => return,
            }
            self.draw();
            return;
        }
        match event.keysym {
            Keysym::Up => self.nudge(0.01 * step, 0.0, 0.0),
            Keysym::Down => self.nudge(-0.01 * step, 0.0, 0.0),
            Keysym::Right => self.nudge(0.0, 0.0, 2.0 * step),
            Keysym::Left => self.nudge(0.0, 0.0, -2.0 * step),
            Keysym::Tab => { let i = (self.primary + 1) % 16; self.select(i, false); }
            Keysym::ISO_Left_Tab => { let i = (self.primary + 15) % 16; self.select(i, false); }
            _ => match event.utf8.as_deref().unwrap_or("") {
                "=" | "+" => self.nudge(0.0, 0.005 * step, 0.0),
                "-" | "_" => self.nudge(0.0, -0.005 * step, 0.0),
                "a" => self.sel = (0..16).collect(),
                "n" => self.sel = (1..=7).collect(),
                "b" => self.sel = (9..=15).collect(),
                "g" => self.sel = vec![0, 15],
                d if d.len() == 1 && d.chars().next().unwrap().is_ascii_digit() => {
                    let i = d.parse::<usize>().unwrap();
                    self.select(i, self.shift);
                }
                _ => return,
            },
        }
        self.draw();
    }

    fn handle_press(&mut self, x: f32, y: f32) {
        if let Some(i) = self.btn_at(x, y) {
            self.close(i == 0);
        } else if let Some(i) = self.dot_at(x, y) {
            self.select(i, self.shift);
            self.push_undo();
            self.drag = Some(Drag::Wheel { start: self.colors, press: wheel_ab(x, y) });
        } else if self.in_strip(x, y) {
            self.push_undo();
            self.drag = Some(Drag::Strip { start: self.colors, press: strip_l(y) });
        } else if let Some(i) = self.chip_at(x, y) {
            self.select(i, self.shift);
        } else {
            return;
        }
        self.draw();
    }

    fn handle_motion(&mut self, x: f32, y: f32) {
        match &self.drag {
            Some(Drag::Wheel { start, press }) => {
                let (a, b) = wheel_ab(x, y);
                let (da, db) = (a - press.0, b - press.1);
                for &i in &self.sel {
                    let s = start[i];
                    let (sa, sb) = (s.c * s.h.to_radians().cos() + da, s.c * s.h.to_radians().sin() + db);
                    self.colors[i].c = (sa * sa + sb * sb).sqrt().min(MAX_C);
                    self.colors[i].h = sb.atan2(sa).to_degrees().rem_euclid(360.0);
                }
            }
            Some(Drag::Strip { start, press }) => {
                let dl = strip_l(y) - press;
                for &i in &self.sel { self.colors[i].l = (start[i].l + dl).clamp(0.0, 1.0); }
            }
            None => {
                let hover = self.dot_at(x, y).or_else(|| self.chip_at(x, y));
                let hover_btn = self.btn_at(x, y);
                if hover == self.hover && hover_btn == self.hover_btn { return; }
                self.hover = hover;
                self.hover_btn = hover_btn;
            }
        }
        self.draw();
    }

    // --- rendering ---

    /// Slice of OKLab space at the given lightness: hue by angle, chroma by radius.
    /// Out-of-gamut regions are dimmed rather than cut off.
    fn ensure_disc(&mut self, l: f32) {
        let key = (l * 255.0) as i32;
        if self.disc.as_ref().is_some_and(|d| d.0 == key) { return; }
        let radius = WHEEL_R * S;
        let size = (radius * 2.0) as usize;
        let mut data = vec![0u8; size * size * 4];
        for py in 0..size {
            for px in 0..size {
                let dx = (px as f32 + 0.5 - radius) / radius;
                let dy = (py as f32 + 0.5 - radius) / radius;
                let d = dx.hypot(dy);
                if d > 1.0 { continue; }
                let ([r, g, b], inside) = oklab_to_srgb(l, dx * MAX_C, -dy * MAX_C);
                let k = if inside { 255.0 } else { 90.0 };
                let i = (py * size + px) * 4;
                data[i] = (r * k) as u8;
                data[i + 1] = (g * k) as u8;
                data[i + 2] = (b * k) as u8;
                data[i + 3] = (((1.0 - d) * radius).clamp(0.0, 1.0) * 255.0) as u8;
            }
        }
        self.disc = Some((key, data));
    }

    fn draw(&mut self) {
        let rgb = self.rgb();
        let bg = rgb[0];
        let fg = rgb[15];
        let dim = mix(bg, fg, 0.55);
        let dimmer = mix(bg, fg, 0.35);

        self.ensure_disc(self.colors[self.primary].l);
        let dot_order = self.dot_order();

        let (wl_buf, canvas) = self.pool
            .create_buffer(PW as i32, PH as i32, PW as i32 * 4, wl_shm::Format::Argb8888)
            .unwrap();
        let mut pixmap = Pixmap::new(PW, PH).unwrap();
        pixmap.fill(tiny_skia::Color::from_rgba8(bg[0], bg[1], bg[2], 0xff));

        // wheel
        let disc = self.disc.take().unwrap();
        let size = (WHEEL_R * 2.0 * S) as i32;
        blit_rgba(pixmap.data_mut(), ((WHEEL_CX - WHEEL_R) * S) as i32, ((WHEEL_CY - WHEEL_R) * S) as i32, size, size, &disc.1);
        self.disc = Some(disc);

        // lightness strip: neutral ramp, one pixel row at a time
        let strip_px_h = (STRIP_H * S) as u32;
        for y in 0..strip_px_h {
            let l = strip_l(STRIP_Y + y as f32 / S);
            let c = lch_to_rgb(Lch { l, c: 0.0, h: 0.0 });
            fill_rect_px(pixmap.data_mut(), (STRIP_X * S) as u32, (STRIP_Y * S) as u32 + y, (STRIP_W * S) as u32, 1, c);
        }

        // dots, selected drawn last so they sit on top
        for i in dot_order {
            let selected = self.sel.contains(&i);
            let ring = if i == self.primary { 4.0 } else if selected { 3.0 } else { 0.0 };
            let hovered = self.hover == Some(i);

            let (dx, dy) = wheel_pos(self.colors[i]);
            if ring > 0.0 { circle(&mut pixmap, dx, dy, DOT_R + ring, fg, 0xff); }
            else if hovered { circle(&mut pixmap, dx, dy, DOT_R + 2.0, dim, 0xff); }
            circle(&mut pixmap, dx, dy, DOT_R, rgb[i], 0xff);
            let label = i.to_string();
            let lw = measure_text(&mut self.font_system, &label, 11.0, &self.font_family);
            let ink = if self.colors[i].l > 0.6 { [0, 0, 0] } else { [255, 255, 255] };
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                &label, dx - lw / 2.0, dy + 4.0, 11.0, ink, &self.font_family);

            let sy = strip_y(self.colors[i].l);
            if ring > 0.0 { circle(&mut pixmap, STRIP_X + STRIP_W / 2.0, sy, STRIP_DOT_R + ring * 0.7, fg, 0xff); }
            circle(&mut pixmap, STRIP_X + STRIP_W / 2.0, sy, STRIP_DOT_R, rgb[i], 0xff);
        }

        // panel
        let c = self.colors[self.primary];
        let hex = { let [r, g, b] = rgb[self.primary]; format!("#{r:02x}{g:02x}{b:02x}") };
        let name = PathBuf::from(&self.image).file_name().unwrap().to_string_lossy().to_string();

        // close buttons, right-aligned along the top
        let mut right = BTN_RIGHT;
        for i in (0..2).rev() {
            let w = measure_text(&mut self.font_system, BUTTONS[i], BTN_SIZE, &self.font_family);
            self.btn_rects[i] = (right - w - 16.0, BTN_TOP, w + 16.0, BTN_H);
            right -= w + 32.0;
        }
        for i in 0..2 {
            if self.hover_btn == Some(i) {
                let (x, y, w, h) = self.btn_rects[i];
                fill_rect(pixmap.data_mut(), x, y, w, h, mix(bg, fg, 0.15));
            }
        }

        let mut text = |s: &str, x: f32, y: f32, size: f32, col: [u8; 3]| {
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                s, x, y, size, col, &self.font_family);
        };
        for i in 0..2 {
            let col = if self.hover_btn == Some(i) { fg } else { dim };
            text(BUTTONS[i], self.btn_rects[i].0 + 8.0, BTN_BASE, BTN_SIZE, col);
        }
        // the filename's cap-height top lines up with the top of the lightness strip
        text(&name, PANEL_X, STRIP_Y + 12.0 * 0.72, 12.0, dimmer);
        text(&format!("color{}", self.primary), PANEL_X, 85.0, 20.0, fg);
        text(&hex, PANEL_X + 44.0, 123.0, 24.0, fg);
        text(&format!("L {:.2}   C {:.3}   H {:.0}", c.l, c.c, c.h), PANEL_X, 157.0, 14.0, dim);
        text(&format!("{} selected", self.sel.len()), PANEL_X, 183.0, 13.0, dimmer);

        let hints = [
            ("drag dot", "hue / chroma"),
            ("drag strip", "lightness"),
            ("shift+click", "multi-select"),
            ("0-9 / tab", "pick color"),
            ("a n", "all / normal"),
            ("b g", "bright / bg+fg"),
            ("up down", "lightness"),
            ("left right", "hue"),
            ("- =", "chroma"),
            ("shift", "5x step"),
            ("ctrl+z", "undo"),
            ("ctrl+shift+z", "redo"),
            ("ctrl+r", "reset all"),
        ];
        for (n, (key, desc)) in hints.iter().enumerate() {
            let y = 223.0 + n as f32 * 17.0;
            text(key, PANEL_X, y, 12.0, dim);
            text(desc, PANEL_X + 96.0, y, 12.0, dimmer);
        }
        fill_rect(pixmap.data_mut(), PANEL_X, 103.0, 30.0, 26.0, rgb[self.primary]);

        // swatch rail
        let cw = chip_w();
        for i in 0..16 {
            let x = PAD + i as f32 * (cw + CHIP_GAP);
            let selected = self.sel.contains(&i);
            if selected || self.hover == Some(i) {
                let col = if i == self.primary { fg } else if selected { dim } else { dimmer };
                fill_rect(pixmap.data_mut(), x - 3.0, RAIL_Y - 3.0, cw + 6.0, RAIL_H + 6.0, col);
            }
            fill_rect(pixmap.data_mut(), x, RAIL_Y, cw, RAIL_H, rgb[i]);
            let label = i.to_string();
            let lw = measure_text(&mut self.font_system, &label, 11.0, &self.font_family);
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                &label, x + (cw - lw) / 2.0, RAIL_Y - 8.0, 11.0,
                if selected { fg } else { dimmer }, &self.font_family);
        }
        for (dst, src) in canvas.chunks_exact_mut(4).zip(pixmap.data().chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }
        wl_buf.attach_to(self.window.wl_surface()).unwrap();
        self.window.wl_surface().damage_buffer(0, 0, PW as i32, PH as i32);
        self.window.wl_surface().commit();
    }
}

// --- Rendering helpers ---

fn circle(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, c: [u8; 3], a: u8) {
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color_rgba8(c[0], c[1], c[2], a);
    let path = PathBuilder::from_circle(cx * S, cy * S, r * S).unwrap();
    pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
}

fn fill_rect(data: &mut [u8], x: f32, y: f32, w: f32, h: f32, c: [u8; 3]) {
    let (px, py) = ((x * S) as u32, (y * S) as u32);
    fill_rect_px(data, px, py, ((x + w) * S) as u32 - px, ((y + h) * S) as u32 - py, c);
}

fn fill_rect_px(data: &mut [u8], x: u32, y: u32, w: u32, h: u32, c: [u8; 3]) {
    for py in y..y.saturating_add(h).min(PH) {
        for px in x..x.saturating_add(w).min(PW) {
            let i = (py as usize * PW as usize + px as usize) * 4;
            data[i] = c[0];
            data[i + 1] = c[1];
            data[i + 2] = c[2];
            data[i + 3] = 0xff;
        }
    }
}

fn blit_rgba(data: &mut [u8], x0: i32, y0: i32, w: i32, h: i32, src: &[u8]) {
    for gy in 0..h {
        let py = y0 + gy;
        if py < 0 || py >= PH as i32 { continue; }
        for gx in 0..w {
            let px = x0 + gx;
            if px < 0 || px >= PW as i32 { continue; }
            let si = (gy * w + gx) as usize * 4;
            let a = src[si + 3] as u32;
            if a == 0 { continue; }
            let i = (py * PW as i32 + px) as usize * 4;
            let inv = 255 - a;
            data[i]     = (src[si] as u32 * a / 255 + data[i] as u32 * inv / 255) as u8;
            data[i + 1] = (src[si + 1] as u32 * a / 255 + data[i + 1] as u32 * inv / 255) as u8;
            data[i + 2] = (src[si + 2] as u32 * a / 255 + data[i + 2] as u32 * inv / 255) as u8;
            data[i + 3] = 0xff;
        }
    }
}

fn make_attrs(family: &str) -> Attrs<'_> {
    Attrs::new().family(cosmic_text::Family::Name(family))
}

fn measure_text(font_system: &mut FontSystem, text: &str, font_size: f32, family: &str) -> f32 {
    let size = font_size * S;
    let mut buf = Buffer::new(font_system, Metrics::new(size, size * 1.2));
    buf.set_size(font_system, None, None);
    buf.set_text(font_system, text, &make_attrs(family), Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);
    buf.layout_runs().next().map_or(0.0, |r| r.line_w) / S
}

fn render_text(
    pixmap: &mut Pixmap, font_system: &mut FontSystem, swash_cache: &mut SwashCache,
    text: &str, x: f32, y: f32, font_size: f32, color: [u8; 3], family: &str,
) {
    let size = font_size * S;
    let mut buf = Buffer::new(font_system, Metrics::new(size, size * 1.2));
    buf.set_size(font_system, None, None);
    buf.set_text(font_system, text, &make_attrs(family), Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);

    for run in buf.layout_runs() {
        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((x * S, y * S), 1.0);
            if let Some(image) = swash_cache.get_image_uncached(font_system, physical.cache_key) {
                let x0 = physical.x + image.placement.left;
                let y0 = physical.y - image.placement.top;
                let (w, h) = (image.placement.width as i32, image.placement.height as i32);
                if let SwashContent::Mask = image.content {
                    blit_mask(pixmap.data_mut(), x0, y0, w, h, &image.data, &color);
                }
            }
        }
    }
}

fn blit_mask(data: &mut [u8], x0: i32, y0: i32, w: i32, h: i32, mask: &[u8], color: &[u8; 3]) {
    for gy in 0..h {
        let py = y0 + gy;
        if py < 0 || py >= PH as i32 { continue; }
        for gx in 0..w {
            let px = x0 + gx;
            if px < 0 || px >= PW as i32 { continue; }
            let a = mask[(gy * w + gx) as usize] as u32;
            if a == 0 { continue; }
            let i = (py * PW as i32 + px) as usize * 4;
            let inv = 255 - a;
            data[i]     = ((color[0] as u32 * a + data[i] as u32 * inv) / 255) as u8;
            data[i + 1] = ((color[1] as u32 * a + data[i + 1] as u32 * inv) / 255) as u8;
            data[i + 2] = ((color[2] as u32 * a + data[i + 2] as u32 * inv) / 255) as u8;
            data[i + 3] = 0xff;
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
                Box::new(|state, _wl_kbd, event| { state.handle_key(&event); }),
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
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, mods: Modifiers, _: RawModifiers, _: u32) {
        self.shift = mods.shift;
        self.ctrl = mods.ctrl;
    }
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, pointer: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            let (x, y) = (event.position.0 as f32 / S, event.position.1 as f32 / S);
            match event.kind {
                PointerEventKind::Enter { serial, .. } => {
                    let device = self.cursor_shape_manager.get_shape_device(pointer, qh);
                    device.set_shape(serial, Shape::Default);
                    device.destroy();
                }
                PointerEventKind::Leave { .. } => {
                    if self.hover.take().is_some() | self.hover_btn.take().is_some() { self.draw(); }
                }
                PointerEventKind::Press { button: 0x110, .. } => self.handle_press(x, y),
                PointerEventKind::Release { button: 0x110, .. } => {
                    if self.drag.take().is_some() { self.apply(); }
                }
                PointerEventKind::Motion { .. } => self.handle_motion(x, y),
                PointerEventKind::Axis { ref vertical, .. } => {
                    let d = if vertical.absolute > 0.0 { -1.0 } else { 1.0 };
                    if self.in_strip(x, y) { self.nudge(0.01 * d, 0.0, 0.0); }
                    else { self.nudge(0.0, 0.005 * d, 0.0); }
                    self.draw();
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) { self.close(false); }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window, _: WindowConfigure, _: u32) {
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
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_registry!(App);

// --- Main ---

fn main() {
    let cfg = load_config();

    let arg = std::env::args().nth(1);
    let image = match arg {
        Some(p) => std::fs::canonicalize(&p)
            .unwrap_or_else(|e| { eprintln!("tincture: {p}: {e}"); std::process::exit(1) })
            .to_string_lossy().to_string(),
        None => std::fs::read_to_string(cache_dir().join("wal"))
            .unwrap_or_else(|e| { eprintln!("tincture: no image given and no current wallpaper: {e}"); std::process::exit(1) })
            .lines().next().unwrap().trim().to_string(),
    };
    let scheme_path = cache_dir().join("cache").join(sanitize_path(&image));
    let (colors, alpha) = read_scheme(&scheme_path);

    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<App> = EventLoop::try_new().unwrap();
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue).insert(loop_handle.clone()).unwrap();
    loop_handle.insert_source(Timer::from_duration(Duration::from_millis(150)), |_, _, app: &mut App| {
        app.poll_child();
        TimeoutAction::ToDuration(Duration::from_millis(150))
    }).unwrap();

    let compositor = CompositorState::bind(&globals, &qh).unwrap();
    let xdg_shell = XdgShell::bind(&globals, &qh).unwrap();
    let shm = Shm::bind(&globals, &qh).unwrap();
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).unwrap();

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("tincture");
    window.set_app_id("tincture");
    window.set_min_size(Some((PW, PH)));
    window.set_max_size(Some((PW, PH)));
    window.commit();

    let pool = SlotPool::new((PW * PH * 4) as usize, &shm).unwrap();

    let font_data = std::fs::read(expand_path(&cfg.font)).expect("failed to read font file");
    let mut db = cosmic_text::fontdb::Database::new();
    db.load_font_data(font_data);
    let font_family = db.faces().next().expect("font file contains no faces").families[0].0.clone();
    let font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        window,
        keyboard: None,
        pointer: None,
        cursor_shape_manager,
        pool,
        exit: false,
        font_system,
        swash_cache: SwashCache::new(),
        font_family,
        loop_handle: event_loop.handle(),
        image,
        scheme_path,
        colors,
        original: colors,
        alpha,
        undos: Vec::new(),
        redos: Vec::new(),
        last_undo: Instant::now() - Duration::from_secs(1),
        sel: vec![0],
        primary: 0,
        hover: None,
        hover_btn: None,
        btn_rects: [(0.0, 0.0, 0.0, 0.0); 2],
        drag: None,
        shift: false,
        ctrl: false,
        disc: None,
        child: None,
        pending: false,
    };

    loop {
        event_loop.dispatch(Duration::from_millis(16), &mut app).unwrap();
        if app.exit { break; }
    }
}
