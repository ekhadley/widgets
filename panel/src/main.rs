use std::path::PathBuf;
use libc;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping, SwashCache, SwashContent, Weight};
use serde::{Deserialize, Serialize};
use smithay_client_toolkit as sctk;
use sctk::reexports::calloop::timer::{TimeoutAction, Timer};
use sctk::reexports::calloop::EventLoop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::registry_handlers;
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::pointer::cursor_shape::CursorShapeManager;
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape;
use sctk::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use sctk::shell::WaylandSurface;
use sctk::shm::slot::SlotPool;
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};
use tiny_skia::Pixmap;

// --- Config ---

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    color_file: Option<String>,
    font: String,
    icon_font: String,
    font_size: f32,
    timer1_duration: u64,
    timer2_duration: u64,
    bt_device_1: String,
    bt_device_2: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            color_file: Some("~/.cache/wal/colors-panel.toml".into()),
            font: "~/.local/share/fonts/GoogleSansCode-Bold.ttf".into(),
            icon_font: "/usr/share/fonts/OTF/Font Awesome 7 Free-Solid-900.otf".into(),
            font_size: 30.0,
            timer1_duration: 3600,
            timer2_duration: 900,
            bt_device_1: "AC:BF:71:08:A1:D6".into(),
            bt_device_2: "EC:81:93:AC:8B:60".into(),
        }
    }
}

fn load_config() -> Config {
    let path = config_dir().join("panel.toml");
    match std::fs::read_to_string(&path) {
        Ok(s) => match toml::from_str(&s) {
            Ok(cfg) => cfg,
            Err(e) => { eprintln!("panel: failed to parse {}: {e}", path.display()); Config::default() }
        }
        Err(_) => Config::default(),
    }
}

fn config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"));
    base.join("widgets")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap())
}

fn expand_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

// --- Colors ---

struct Colors {
    background: [u8; 3],
    background_alpha: u8,
    border: [u8; 3],
    divider: [u8; 3],
    accent: [[u8; 3]; 6],
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            background: [0x1e, 0x1e, 0x2e],
            background_alpha: 0xe6, // ~0.9
            border: [0xcd, 0xd6, 0xf4],
            divider: [0xcd, 0xd6, 0xf4],
            accent: [
                [0xf3, 0x8b, 0xa8], // dot1
                [0xa6, 0xe3, 0xa1], // dot2
                [0xf9, 0xe2, 0xaf], // sun
                [0x89, 0xb4, 0xfa], // clock
                [0xcb, 0xa6, 0xf7], // ui
                [0x94, 0xe2, 0xd5], // dot6
            ],
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
                "background_opacity" => {
                    if let Ok(f) = val.parse::<f32>() {
                        colors.background_alpha = (f.clamp(0.0, 1.0) * 255.0) as u8;
                    }
                }
                _ => {
                    if let Some(c) = parse_hex(val) {
                        match key {
                            "background" => colors.background = c,
                            "border" => colors.border = c,
                            "divider" => colors.divider = c,
                            "dot1" => colors.accent[0] = c,
                            "dot2" => colors.accent[1] = c,
                            "sun" => colors.accent[2] = c,
                            "clock" => colors.accent[3] = c,
                            "ui" => colors.accent[4] = c,
                            "dot6" => colors.accent[5] = c,
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    colors
}

// --- Timer State ---

#[derive(Serialize, Deserialize, Default)]
struct TimerState {
    timer1_duration: u64,
    timer1_started: u64,
    timer2_duration: u64,
    timer2_started: u64,
}

fn state_dir() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/state"));
    base.join("widgets/panel")
}

fn load_timer_state(cfg: &Config) -> TimerState {
    let path = state_dir().join("timers.toml");
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|_| TimerState {
            timer1_duration: cfg.timer1_duration,
            timer2_duration: cfg.timer2_duration,
            ..Default::default()
        }),
        Err(_) => TimerState {
            timer1_duration: cfg.timer1_duration,
            timer2_duration: cfg.timer2_duration,
            ..Default::default()
        },
    }
}

fn save_timer_state(state: &TimerState) {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).ok();
    let s = toml::to_string(state).unwrap();
    std::fs::write(dir.join("timers.toml"), s).ok();
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn timer_remaining(duration: u64, started: u64) -> i64 {
    if started == 0 { return duration as i64; }
    duration as i64 - (now_unix() as i64 - started as i64)
}

fn format_timer(secs: i64) -> String {
    let sign = if secs < 0 { "-" } else { "" };
    let abs = secs.unsigned_abs();
    let m = abs / 60;
    let s = abs % 60;
    format!("{sign}{m}:{s:02}")
}

// --- Audio ---

fn get_volume() -> (f32, bool) {
    let out = Command::new("wpctl").args(["get-volume", "@DEFAULT_AUDIO_SINK@"]).output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let muted = s.contains("[MUTED]");
            let vol = s.split_whitespace().nth(1)
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.5);
            (vol, muted)
        }
        Err(_) => (0.5, false),
    }
}

fn set_volume(vol: f32) {
    let v = vol.clamp(0.0, VOL_MAX);
    Command::new("wpctl")
        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{v:.2}")])
        .spawn().ok();
}

fn is_headphones() -> bool {
    let out = Command::new("wpctl").args(["inspect", "@DEFAULT_AUDIO_SINK@"]).output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).to_lowercase();
            s.contains("ac:bf:71:08:a1:d6") || s.contains("ac_bf_71_08_a1_d6")
                || s.contains("headphone") || s.contains("headset")
        }
        Err(_) => false,
    }
}

fn switch_audio(target_mac: &str) {
    Command::new("sh").arg("-c")
        .arg(format!("{}/scripts/audio_switch.sh {target_mac}",
            home().join(".config/quickshell").display()))
        .spawn().ok();
}

// --- Layout constants ---

const WIDTH: u32 = 320;
const HEIGHT: u32 = 202;
const OUTER: u32 = 3;   // outer border thickness
const INNER: u32 = 1;   // inner divider thickness
const LEFT_W: u32 = 42;
const RIGHT_W: u32 = 42;
const TOGGLE_H: u32 = 38;  // left column split
const CLOCK_H: u32 = 145;  // center column split
const AUDIO_H: u32 = 34;   // right column split (from bottom)

// Font/text sizes
const ICON_SIZE: f32 = 25.0;
const DOT_SIZE: f32 = 22.0;
const DATE_SIZE: f32 = 18.0;
const TIMER_SIZE: f32 = 26.0;
const LINE_HEIGHT: f32 = 1.2;
const CLOCK_DATE_GAP: f32 = 2.0;

// Hover
const HOVER_OPACITY_DEFAULT: f32 = 0.7;

// Volume
const VOL_BAR_PAD: u32 = 12;
const VOL_BAR_W: u32 = 12;
const VOL_BG_ALPHA: f32 = 0.3;
const VOL_SCROLL_STEP: f32 = 0.05;
const VOL_MAX: f32 = 2.0;

// Timers
const TIMER_SCROLL_STEP: i64 = 60;
// Timers
const INACTIVE_ALPHA: f32 = 0.8;

// Timing
const TICK_MS: u64 = 100;
const AUDIO_REFRESH_COOLDOWN: u64 = 1;

// Audio icon
const AUDIO_ICON_NUDGE: f32 = -0.1;

// --- Tile geometry ---

#[derive(Clone, Copy)]
struct Rect { x: u32, y: u32, w: u32, h: u32 }

impl Rect {
    fn contains(&self, mx: u32, my: u32) -> bool {
        mx >= self.x && mx < self.x + self.w && my >= self.y && my < self.y + self.h
    }
}

struct Layout {
    toggle: Rect,
    dots: Rect,
    clock: Rect,
    timer1: Rect,
    timer2: Rect,
    volume: Rect,
    audio: Rect,
}

fn layout(w: u32, h: u32) -> Layout {
    let interior_w = w - 2 * OUTER;
    let interior_h = h - 2 * OUTER;
    let center_x = OUTER + LEFT_W + INNER;
    let center_w = interior_w - LEFT_W - RIGHT_W - 2 * INNER;
    let right_x = w - OUTER - RIGHT_W;
    let timer_y = OUTER + CLOCK_H + INNER;
    let timer_h = interior_h - CLOCK_H - INNER;
    let timer_w = (center_w - INNER) / 2;
    let audio_y = h - OUTER - AUDIO_H;
    Layout {
        toggle: Rect { x: OUTER, y: OUTER, w: LEFT_W, h: TOGGLE_H },
        dots: Rect { x: OUTER, y: OUTER + TOGGLE_H + INNER, w: LEFT_W, h: interior_h - TOGGLE_H - INNER },
        clock: Rect { x: center_x, y: OUTER, w: center_w, h: CLOCK_H },
        timer1: Rect { x: center_x, y: timer_y, w: timer_w, h: timer_h },
        timer2: Rect { x: center_x + timer_w + INNER, y: timer_y, w: center_w - timer_w - INNER, h: timer_h },
        volume: Rect { x: right_x, y: OUTER, w: RIGHT_W, h: audio_y - OUTER },
        audio: Rect { x: right_x, y: audio_y, w: RIGHT_W, h: AUDIO_H },
    }
}

fn center_x(area_x: f32, area_w: f32, text_w: f32) -> f32 {
    area_x + (area_w - text_w) / 2.0
}

// nudge: usually 0.0; positive pushes down
fn center_y(area_y: f32, area_h: f32, font_size: f32, nudge: f32) -> f32 {
    area_y + (area_h - font_size * LINE_HEIGHT) / 2.0 + font_size * nudge
}

// --- Hover ---

#[derive(PartialEq, Clone, Copy)]
enum HoverTile { None, Toggle, Timer1, Timer2, Audio }

// --- App ---

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_shape_manager: CursorShapeManager,
    pool: SlotPool,
    width: u32,
    height: u32,
    exit: bool,
    font_system: FontSystem,
    swash_cache: SwashCache,
    colors: Colors,
    font_size: f32,
    font_family: String,
    icon_family: String,
    // Timer state
    timer1_duration: u64,
    timer1_started: u64,
    timer2_duration: u64,
    timer2_started: u64,
    // Audio
    volume: f32,
    muted: bool,
    headphones: bool,
    bt_device_1: String,
    bt_device_2: String,
    // Theme
    is_dim: bool,
    // Hover
    hover: HoverTile,
    // Default durations for reset
    timer1_default: u64,
    timer2_default: u64,
    // Volume drag
    dragging_volume: bool,
    volume_set_at: u64,
}

impl App {
    fn timer_state(&self) -> TimerState {
        TimerState {
            timer1_duration: self.timer1_duration,
            timer1_started: self.timer1_started,
            timer2_duration: self.timer2_duration,
            timer2_started: self.timer2_started,
        }
    }

    fn volume_from_y(&self, y: f64) -> f32 {
        let lay = layout(self.width, self.height);
        let vol_bar_top = lay.volume.y + VOL_BAR_PAD;
        let vol_bar_h = lay.volume.h - 2 * VOL_BAR_PAD;
        let frac = 1.0 - (y as f32 - vol_bar_top as f32) / vol_bar_h as f32;
        (frac * VOL_MAX).clamp(0.0, VOL_MAX)
    }

    fn refresh_audio(&mut self) {
        let (v, m) = get_volume();
        self.volume = v;
        self.muted = m;
        self.headphones = is_headphones();
    }

    fn draw(&mut self) {
        let c = &self.colors;
        let bg = c.background;
        let bg_a = c.background_alpha;
        let border = c.border;
        let divider = c.divider;
        let lay = layout(self.width, self.height);

        let stride = self.width as i32 * 4;
        let (wl_buf, canvas) = self.pool
            .create_buffer(self.width as i32, self.height as i32, stride, wl_shm::Format::Argb8888)
            .unwrap();

        let mut pixmap = Pixmap::new(self.width, self.height).unwrap();
        pixmap.fill(tiny_skia::Color::TRANSPARENT);

        let pw = pixmap.width();
        let ph = pixmap.height();

        let interior_w = self.width - 2 * OUTER;
        let interior_h = self.height - 2 * OUTER;

        // Uniform background
        fill_rect_alpha(pixmap.data_mut(), pw, ph,
            OUTER, OUTER, interior_w, interior_h, bg, bg_a);

        // Outer border (heavy)
        fill_rect(pixmap.data_mut(), pw, ph, 0, 0, self.width, OUTER, border);
        fill_rect(pixmap.data_mut(), pw, ph, 0, self.height - OUTER, self.width, OUTER, border);
        fill_rect(pixmap.data_mut(), pw, ph, 0, 0, OUTER, self.height, border);
        fill_rect(pixmap.data_mut(), pw, ph, self.width - OUTER, 0, OUTER, self.height, border);

        // Column dividers (full height)
        fill_rect(pixmap.data_mut(), pw, ph, OUTER + LEFT_W, OUTER, INNER, interior_h, divider);
        fill_rect(pixmap.data_mut(), pw, ph, lay.volume.x - INNER, OUTER, INNER, interior_h, divider);

        // Per-column horizontal dividers (each only spans its column)
        fill_rect(pixmap.data_mut(), pw, ph, OUTER, lay.toggle.y + lay.toggle.h, LEFT_W, INNER, divider);
        fill_rect(pixmap.data_mut(), pw, ph, lay.clock.x, lay.clock.y + lay.clock.h, lay.clock.w, INNER, divider);

        // Timer split (vertical, within timer area of center column)
        fill_rect(pixmap.data_mut(), pw, ph, lay.timer1.x + lay.timer1.w, lay.timer1.y, INNER, lay.timer1.h, divider);

        let fa = &self.icon_family;

        // --- Toggle tile (top-left) ---
        let icon_char = if self.is_dim { "\u{f186}" } else { "\u{f185}" };
        let mut icon_color = if self.is_dim { c.accent[3] } else { c.accent[2] };
        icon_color = alpha_color(icon_color, if self.hover == HoverTile::Toggle { 1.0 } else { HOVER_OPACITY_DEFAULT });
        let icon_w = measure_text(&mut self.font_system, icon_char, ICON_SIZE, fa, Weight::BLACK);
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            icon_char,
            center_x(lay.toggle.x as f32, lay.toggle.w as f32, icon_w),
            center_y(lay.toggle.y as f32, lay.toggle.h as f32, ICON_SIZE, 0.0),
            ICON_SIZE, lay.toggle.w as f32, lay.toggle.h as f32, icon_color,
            fa, Weight::BLACK);

        // --- Dots tile (bottom-left, vertical) ---
        let dot_char = "\u{25cf}";
        let dot_step = lay.dots.h as f32 / 6.0;
        for (i, &color) in c.accent.iter().enumerate() {
            let dw = measure_text(&mut self.font_system, dot_char, DOT_SIZE, &self.font_family, Weight::BOLD);
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                dot_char,
                center_x(lay.dots.x as f32, lay.dots.w as f32, dw),
                center_y(lay.dots.y as f32 + i as f32 * dot_step, dot_step, DOT_SIZE, 0.0),
                DOT_SIZE, lay.dots.w as f32, dot_step, color,
                &self.font_family, Weight::BOLD);
        }

        // --- Clock tile (top-center) ---
        let now = chrono_now();
        let time_str = format!("{:02}:{:02}:{:02}", now.0, now.1, now.2);
        let time_size = self.font_size;
        let date_str = format_date();
        let time_line_h = time_size * LINE_HEIGHT;
        let date_line_h = DATE_SIZE * LINE_HEIGHT;
        let block_h = time_line_h + CLOCK_DATE_GAP + date_line_h;
        let block_y = lay.clock.y as f32 + (lay.clock.h as f32 - block_h) / 2.0;

        let time_w = measure_text(&mut self.font_system, &time_str, time_size, &self.font_family, Weight::BOLD);
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            &time_str,
            center_x(lay.clock.x as f32, lay.clock.w as f32, time_w),
            block_y,
            time_size, lay.clock.w as f32, lay.clock.h as f32, c.accent[3],
            &self.font_family, Weight::BOLD);

        let date_w = measure_text(&mut self.font_system, &date_str, DATE_SIZE, &self.font_family, Weight::BOLD);
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            &date_str,
            center_x(lay.clock.x as f32, lay.clock.w as f32, date_w),
            block_y + time_line_h + CLOCK_DATE_GAP,
            DATE_SIZE, lay.clock.w as f32, lay.clock.h as f32, c.accent[3],
            &self.font_family, Weight::BOLD);

        // --- Timer 1 tile (bottom-center-left) ---
        let t1_rem = timer_remaining(self.timer1_duration, self.timer1_started);
        let t1_str = format_timer(t1_rem);
        let mut t1_color = if self.timer1_started > 0 { c.accent[4] }
                          else { alpha_color(c.accent[4], INACTIVE_ALPHA) };
        t1_color = alpha_color(t1_color, if self.hover == HoverTile::Timer1 { 1.0 } else { HOVER_OPACITY_DEFAULT });
        let t1_w = measure_text(&mut self.font_system, &t1_str, TIMER_SIZE, &self.font_family, Weight::BOLD);
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            &t1_str,
            center_x(lay.timer1.x as f32, lay.timer1.w as f32, t1_w),
            center_y(lay.timer1.y as f32, lay.timer1.h as f32, TIMER_SIZE, 0.0),
            TIMER_SIZE, lay.timer1.w as f32, lay.timer1.h as f32, t1_color,
            &self.font_family, Weight::BOLD);

        // --- Timer 2 tile (bottom-center-right) ---
        let t2_rem = timer_remaining(self.timer2_duration, self.timer2_started);
        let t2_str = format_timer(t2_rem);
        let mut t2_color = if self.timer2_started > 0 { c.accent[4] }
                          else { alpha_color(c.accent[4], INACTIVE_ALPHA) };
        t2_color = alpha_color(t2_color, if self.hover == HoverTile::Timer2 { 1.0 } else { HOVER_OPACITY_DEFAULT });
        let t2_w = measure_text(&mut self.font_system, &t2_str, TIMER_SIZE, &self.font_family, Weight::BOLD);
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            &t2_str,
            center_x(lay.timer2.x as f32, lay.timer2.w as f32, t2_w),
            center_y(lay.timer2.y as f32, lay.timer2.h as f32, TIMER_SIZE, 0.0),
            TIMER_SIZE, lay.timer2.w as f32, lay.timer2.h as f32, t2_color,
            &self.font_family, Weight::BOLD);

        // --- Volume tile (right column, unified with audio) ---
        let vol_bar_top = lay.volume.y + VOL_BAR_PAD;
        let vol_bar_h = lay.volume.h - 2 * VOL_BAR_PAD;
        let bar_x = lay.volume.x + (lay.volume.w - VOL_BAR_W) / 2;

        let vol_bg_color = alpha_color(c.accent[4], VOL_BG_ALPHA);
        fill_rect(pixmap.data_mut(), pw, ph, bar_x, vol_bar_top, VOL_BAR_W, vol_bar_h, vol_bg_color);

        let fill_frac = (self.volume / VOL_MAX).clamp(0.0, 1.0);
        let fill_h = (vol_bar_h as f32 * fill_frac) as u32;
        if fill_h > 0 {
            let opacity = if self.muted { 0x4d } else { 0xff };
            fill_rect_alpha(pixmap.data_mut(), pw, ph,
                bar_x, vol_bar_top + vol_bar_h - fill_h, VOL_BAR_W, fill_h, c.accent[4], opacity);
        }

        // --- Audio tile (bottom-right) ---
        let audio_icon = if self.headphones { "\u{f025}" } else { "\u{f028}" };
        let mut ai_color = if self.muted { alpha_color(c.accent[4], VOL_BG_ALPHA) } else { c.accent[4] };
        ai_color = alpha_color(ai_color, if self.hover == HoverTile::Audio { 1.0 } else { HOVER_OPACITY_DEFAULT });
        let ai_w = measure_text(&mut self.font_system, audio_icon, ICON_SIZE, fa, Weight::BLACK);
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            audio_icon,
            center_x(lay.audio.x as f32, lay.audio.w as f32, ai_w),
            center_y(lay.audio.y as f32, lay.audio.h as f32, ICON_SIZE, AUDIO_ICON_NUDGE),
            ICON_SIZE, lay.audio.w as f32, lay.audio.h as f32, ai_color,
            fa, Weight::BLACK);

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

    fn handle_click(&mut self, x: f64, y: f64) {
        let (mx, my) = (x as u32, y as u32);
        let lay = layout(self.width, self.height);

        if lay.volume.contains(mx, my) {
            self.dragging_volume = true;
            self.volume = self.volume_from_y(y);
            set_volume(self.volume);
            self.volume_set_at = now_unix();
            self.draw();
            return;
        }

        if lay.toggle.contains(mx, my) {
            let arg = if self.is_dim { "1" } else { "0" };
            Command::new("sh").arg("-c")
                .arg(format!("{}/scripts/dim_toggle.sh {arg}",
                    home().join(".config/quickshell").display()))
                .spawn().ok();
            self.is_dim = !self.is_dim;
            self.draw();
            return;
        }

        if lay.timer1.contains(mx, my) {
            if self.timer1_started > 0 {
                let rem = timer_remaining(self.timer1_duration, self.timer1_started);
                self.timer1_duration = rem.max(0) as u64;
                self.timer1_started = 0;
            } else {
                self.timer1_started = now_unix();
            }
            save_timer_state(&self.timer_state());
            self.draw();
            return;
        }

        if lay.timer2.contains(mx, my) {
            if self.timer2_started > 0 {
                let rem = timer_remaining(self.timer2_duration, self.timer2_started);
                self.timer2_duration = rem.max(0) as u64;
                self.timer2_started = 0;
            } else {
                self.timer2_started = now_unix();
            }
            save_timer_state(&self.timer_state());
            self.draw();
            return;
        }

        if lay.audio.contains(mx, my) {
            let target = if self.headphones { &self.bt_device_2 } else { &self.bt_device_1 };
            let target = target.clone();
            switch_audio(&target);
            self.headphones = !self.headphones;
            self.draw();
        }
    }

    fn handle_scroll(&mut self, x: f64, y: f64, dy: f64) {
        let (mx, my) = (x as u32, y as u32);
        let lay = layout(self.width, self.height);

        if lay.volume.contains(mx, my) {
            let delta: f32 = if dy > 0.0 { -VOL_SCROLL_STEP } else { VOL_SCROLL_STEP };
            self.volume = (self.volume + delta).clamp(0.0, VOL_MAX);
            set_volume(self.volume);
            self.draw();
            return;
        }

        if lay.timer1.contains(mx, my) {
            let delta: i64 = if dy > 0.0 { -TIMER_SCROLL_STEP } else { TIMER_SCROLL_STEP };
            self.timer1_duration = (self.timer1_duration as i64 + delta).max(TIMER_SCROLL_STEP) as u64;
            save_timer_state(&self.timer_state());
            self.draw();
            return;
        }

        if lay.timer2.contains(mx, my) {
            let delta: i64 = if dy > 0.0 { -TIMER_SCROLL_STEP } else { TIMER_SCROLL_STEP };
            self.timer2_duration = (self.timer2_duration as i64 + delta).max(TIMER_SCROLL_STEP) as u64;
            save_timer_state(&self.timer_state());
            self.draw();
        }
    }

    fn handle_right_click(&mut self, x: f64, y: f64) {
        let (mx, my) = (x as u32, y as u32);
        let lay = layout(self.width, self.height);

        if lay.timer1.contains(mx, my) {
            self.timer1_duration = self.timer1_default;
            self.timer1_started = 0;
            save_timer_state(&self.timer_state());
            self.draw();
            return;
        }

        if lay.timer2.contains(mx, my) {
            self.timer2_duration = self.timer2_default;
            self.timer2_started = 0;
            save_timer_state(&self.timer_state());
            self.draw();
        }
    }

    fn hover_tile_at(&self, x: f64, y: f64) -> HoverTile {
        let (mx, my) = (x as u32, y as u32);
        let lay = layout(self.width, self.height);

        if lay.toggle.contains(mx, my) { return HoverTile::Toggle; }
        if lay.timer1.contains(mx, my) { return HoverTile::Timer1; }
        if lay.timer2.contains(mx, my) { return HoverTile::Timer2; }
        if lay.audio.contains(mx, my) { return HoverTile::Audio; }
        HoverTile::None
    }
}

// --- Time helpers (no chrono dependency, use libc) ---

fn chrono_now() -> (u32, u32, u32) {
    let secs = now_unix();
    // Use libc localtime for proper timezone handling
    let t = secs as i64;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&t as *const i64, &mut tm) };
    (tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32)
}

fn format_date() -> String {
    let secs = now_unix();
    let t = secs as i64;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&t as *const i64, &mut tm) };
    let months = ["January", "February", "March", "April", "May", "June",
                  "July", "August", "September", "October", "November", "December"];
    let month = months[tm.tm_mon as usize];
    format!("{} {}", month, tm.tm_mday)
}

// --- Rendering helpers ---

fn alpha_color(c: [u8; 3], a: f32) -> [u8; 3] {
    [(c[0] as f32 * a) as u8, (c[1] as f32 * a) as u8, (c[2] as f32 * a) as u8]
}

fn fill_rect(data: &mut [u8], pw: u32, ph: u32, x: u32, y: u32, w: u32, h: u32, c: [u8; 3]) {
    for py in y..y.saturating_add(h).min(ph) {
        for px in x..x.saturating_add(w).min(pw) {
            let i = (py as usize * pw as usize + px as usize) * 4;
            data[i] = c[0]; data[i + 1] = c[1]; data[i + 2] = c[2]; data[i + 3] = 0xff;
        }
    }
}

fn fill_rect_alpha(data: &mut [u8], pw: u32, ph: u32, x: u32, y: u32, w: u32, h: u32, c: [u8; 3], a: u8) {
    if a == 0xff { return fill_rect(data, pw, ph, x, y, w, h, c); }
    if a == 0 { return; }
    let a32 = a as u32;
    let inv = 255 - a32;
    for py in y..y.saturating_add(h).min(ph) {
        for px in x..x.saturating_add(w).min(pw) {
            let i = (py as usize * pw as usize + px as usize) * 4;
            data[i]     = ((c[0] as u32 * a32 + data[i] as u32 * inv) / 255) as u8;
            data[i + 1] = ((c[1] as u32 * a32 + data[i + 1] as u32 * inv) / 255) as u8;
            data[i + 2] = ((c[2] as u32 * a32 + data[i + 2] as u32 * inv) / 255) as u8;
            data[i + 3] = ((a32 + data[i + 3] as u32 * inv / 255)) as u8;
        }
    }
}

fn make_attrs(family: &str, weight: Weight) -> Attrs<'_> {
    Attrs::new().weight(weight).family(cosmic_text::Family::Name(family))
}

fn measure_text(font_system: &mut FontSystem, text: &str, font_size: f32, family: &str, weight: Weight) -> f32 {
    let mut buf = Buffer::new(font_system, Metrics::new(font_size, font_size * LINE_HEIGHT));
    buf.set_size(font_system, None, None);
    buf.set_text(font_system, text, &make_attrs(family, weight), Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);
    buf.layout_runs().next().map_or(0.0, |r| r.line_w)
}

fn render_text(
    pixmap: &mut Pixmap, font_system: &mut FontSystem, swash_cache: &mut SwashCache,
    text: &str, x: f32, y: f32, font_size: f32, max_w: f32, max_h: f32, color: [u8; 3],
    family: &str, weight: Weight,
) {
    let line_h = font_size * LINE_HEIGHT;
    let mut buf = Buffer::new(font_system, Metrics::new(font_size, line_h));
    buf.set_size(font_system, Some(max_w), Some(max_h));
    buf.set_text(font_system, text, &make_attrs(family, weight), Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);

    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    for run in buf.layout_runs() {
        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((x, y + run.line_y), 1.0);
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
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = Some(self.seat_state.get_pointer(qh, &seat).unwrap());
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, pointer: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    let device = self.cursor_shape_manager.get_shape_device(pointer, qh);
                    device.set_shape(serial, Shape::Default);
                    device.destroy();
                }
                PointerEventKind::Press { button: 0x110, .. } => {
                    self.handle_click(event.position.0, event.position.1);
                }
                PointerEventKind::Press { button: 0x111, .. } => {
                    self.handle_right_click(event.position.0, event.position.1);
                }
                PointerEventKind::Release { button: 0x110, .. } => {
                    self.dragging_volume = false;
                }
                PointerEventKind::Motion { .. } => {
                    if self.dragging_volume {
                        self.volume = self.volume_from_y(event.position.1);
                        set_volume(self.volume);
                        self.volume_set_at = now_unix();
                        self.draw();
                    } else {
                        let new_hover = self.hover_tile_at(event.position.0, event.position.1);
                        if new_hover != self.hover {
                            self.hover = new_hover;
                            self.draw();
                        }
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.hover != HoverTile::None {
                        self.hover = HoverTile::None;
                        self.draw();
                    }
                }
                PointerEventKind::Axis { ref vertical, .. } => {
                    if vertical.absolute != 0.0 {
                        self.handle_scroll(event.position.0, event.position.1, vertical.absolute);
                    }
                }
                _ => {}
            }
        }
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
delegate_pointer!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);

// --- Main ---

fn main() {
    let cfg = load_config();
    let colors = load_colors(cfg.color_file.as_deref());
    let ts = load_timer_state(&cfg);
    let (volume, muted) = get_volume();
    let headphones = is_headphones();

    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<App> = EventLoop::try_new().unwrap();
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue).insert(loop_handle).unwrap();

    let compositor = CompositorState::bind(&globals, &qh).unwrap();
    let layer_shell = LayerShell::bind(&globals, &qh).unwrap();
    let shm = Shm::bind(&globals, &qh).unwrap();
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).unwrap();

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("panel"), None);
    layer.set_size(WIDTH, HEIGHT);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.wl_surface().commit();

    let pool = SlotPool::new((WIDTH * HEIGHT * 4) as usize, &shm).unwrap();

    let font_data = std::fs::read(expand_path(&cfg.font)).expect("failed to read font file");
    let icon_data = std::fs::read(expand_path(&cfg.icon_font)).expect("failed to read icon font file");
    let mut db = cosmic_text::fontdb::Database::new();
    db.load_font_data(font_data);
    let font_family = db.faces().next().expect("font file contains no faces").families[0].0.clone();
    db.load_font_data(icon_data);
    let icon_family = db.faces().last().expect("icon font file contains no faces").families[0].0.clone();
    let font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        pointer: None,
        cursor_shape_manager,
        pool,
        width: WIDTH,
        height: HEIGHT,
        exit: false,
        font_system,
        swash_cache: SwashCache::new(),
        colors,
        font_size: cfg.font_size,
        font_family,
        icon_family,
        timer1_duration: ts.timer1_duration,
        timer1_started: ts.timer1_started,
        timer2_duration: ts.timer2_duration,
        timer2_started: ts.timer2_started,
        volume,
        muted,
        headphones,
        bt_device_1: cfg.bt_device_1,
        bt_device_2: cfg.bt_device_2,
        is_dim: false,
        hover: HoverTile::None,
        timer1_default: cfg.timer1_duration,
        timer2_default: cfg.timer2_duration,
        dragging_volume: false,
        volume_set_at: 0,
    };

    // 1-second timer for clock/timer redraws
    let timer = Timer::from_duration(std::time::Duration::from_millis(TICK_MS));
    event_loop.handle().insert_source(timer, |_, _, app| {
        if now_unix() - app.volume_set_at >= AUDIO_REFRESH_COOLDOWN {
            app.refresh_audio();
        }
        app.draw();
        TimeoutAction::ToDuration(std::time::Duration::from_millis(TICK_MS))
    }).unwrap();

    loop {
        event_loop.dispatch(std::time::Duration::from_millis(TICK_MS), &mut app).unwrap();
        if app.exit {
            save_timer_state(&app.timer_state());
            break;
        }
    }
}
