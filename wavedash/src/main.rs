use std::path::PathBuf;
use libc;
use std::process::{Command, Child, Stdio};
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
    weather_lat: f64,
    weather_lon: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            color_file: Some("~/.cache/wal/colors-wavedash.toml".into()),
            font: "~/.local/share/fonts/GoogleSansCode-Bold.ttf".into(),
            icon_font: "/usr/share/fonts/OTF/Font Awesome 7 Free-Solid-900.otf".into(),
            font_size: 39.0,
            timer1_duration: 3600,
            timer2_duration: 900,
            bt_device_1: "AC:BF:71:08:A1:D6".into(),
            bt_device_2: "EC:81:93:AC:8B:60".into(),
            weather_lat: 0.0,
            weather_lon: 0.0,
        }
    }
}

fn load_config() -> Config {
    let path = config_dir().join("wavedash.toml");
    match std::fs::read_to_string(&path) {
        Ok(s) => match toml::from_str(&s) {
            Ok(cfg) => cfg,
            Err(e) => { eprintln!("wavedash: failed to parse {}: {e}", path.display()); Config::default() }
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
    sun: [u8; 3],
    clock: [u8; 3],
    accent: [u8; 3],
    weather: [u8; 3],
    audio: [u8; 3],
    volume: [u8; 3],
    notif: [u8; 3],
    timer: [u8; 3],
    dots: [[u8; 3]; 16],
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            background: [0x1e, 0x1e, 0x2e],
            background_alpha: 0xe6, // ~0.9
            border: [0xcd, 0xd6, 0xf4],
            divider: [0xcd, 0xd6, 0xf4],
            sun: [0xf9, 0xe2, 0xaf],
            clock: [0x89, 0xb4, 0xfa],
            accent: [0x89, 0xb4, 0xfa],
            weather: [0x94, 0xe2, 0xd5],
            audio: [0xcb, 0xa6, 0xf7],
            volume: [0xcb, 0xa6, 0xf7],
            notif: [0xcb, 0xa6, 0xf7],
            timer: [0xcb, 0xa6, 0xf7],
            dots: [
                [0xcd, 0xd6, 0xf4], // foreground
                [0xf3, 0x8b, 0xa8], [0xa6, 0xe3, 0xa1], [0xf9, 0xe2, 0xaf], [0x89, 0xb4, 0xfa],
                [0xcb, 0xa6, 0xf7], [0x94, 0xe2, 0xd5], [0xf2, 0xcd, 0xcd], [0xb4, 0xbe, 0xfe],
                [0xf3, 0x8b, 0xa8], [0xa6, 0xe3, 0xa1], [0xf9, 0xe2, 0xaf], [0x89, 0xb4, 0xfa],
                [0xcb, 0xa6, 0xf7], [0x94, 0xe2, 0xd5], [0xf2, 0xcd, 0xcd],
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
                            "sun" => colors.sun = c,
                            "clock" => colors.clock = c,
                            "accent" => colors.accent = c,
                            "weather" => colors.weather = c,
                            "audio" => colors.audio = c,
                            "volume" => colors.volume = c,
                            "notif" => colors.notif = c,
                            "timer" => colors.timer = c,
                            "foreground" => colors.dots[0] = c,
                            _ => {
                                if let Some(n) = key.strip_prefix("color") {
                                    if let Ok(i) = n.parse::<usize>() {
                                        if i >= 1 && i <= 15 { colors.dots[i] = c; }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    colors
}

// --- State ---

#[derive(Serialize, Deserialize, Default)]
struct State {
    #[serde(default)] timer1_duration: i64,
    #[serde(default)] timer1_started: u64,
    #[serde(default)] timer1_base: i64,
    #[serde(default)] timer2_duration: i64,
    #[serde(default)] timer2_started: u64,
    #[serde(default)] timer2_base: i64,
    #[serde(default)] weather_temp: f64,
    #[serde(default)] weather_feels: f64,
    #[serde(default)] weather_code: u32,
    #[serde(default)] weather_is_day: bool,
    #[serde(default)] weather_fetched: u64,
}

fn state_path() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/state"));
    base.join("widgets/wavedash.toml")
}

fn load_state(cfg: &Config) -> State {
    let mut st = match std::fs::read_to_string(state_path()) {
        Ok(s) => toml::from_str(&s).unwrap_or_default(),
        Err(_) => State::default(),
    };
    if st.timer1_base == 0 { st.timer1_base = cfg.timer1_duration as i64; }
    if st.timer2_base == 0 { st.timer2_base = cfg.timer2_duration as i64; }
    if st.timer1_duration == 0 { st.timer1_duration = cfg.timer1_duration as i64; }
    if st.timer2_duration == 0 { st.timer2_duration = cfg.timer2_duration as i64; }
    st
}

fn save_state(state: &State) {
    let path = state_path();
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    std::fs::write(path, toml::to_string(state).unwrap()).ok();
}

const WEATHER_MAX_AGE: u64 = 3600;

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn timer_remaining(duration: i64, started: u64) -> i64 {
    if started == 0 { return duration; }
    duration - (now_unix() as i64 - started as i64)
}

fn format_timer(secs: i64) -> String {
    let sign = if secs < 0 { "-" } else { "" };
    let abs = secs.unsigned_abs();
    let m = abs / 60;
    let s = abs % 60;
    format!("{sign}{m}:{s:02}")
}


fn weather_icon(code: u32, is_day: bool) -> &'static str {
    match code {
        0 | 1 => if is_day { "\u{f185}" } else { "\u{f186}" }, // fa-sun / fa-moon
        2 | 3 => "\u{f0c2}",           // fa-cloud
        45 | 48 => "\u{f75f}",         // fa-smog
        51..=67 | 80..=82 => "\u{f73d}", // fa-cloud-rain
        71..=77 | 85 | 86 => "\u{f2dc}", // fa-snowflake
        95 | 96 | 99 => "\u{f0e7}",    // fa-bolt
        _ => "\u{f0c2}",
    }
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

const WIDTH: u32 = 440;
const HEIGHT: u32 = 235;
const ACCENT_W: u32 = 4;
const LEFT_MARGIN: f32 = ACCENT_W as f32 + 10.0;

// Type scale
const CLOCK_HM_SIZE: f32 = 52.0;
const DATE_SIZE: f32 = 14.0;
const WEATHER_ICON_SIZE: f32 = 23.0;
const WEATHER_TEMP_SIZE: f32 = 36.0;
const WEATHER_FEELS_SIZE: f32 = 18.0;
const TIMER_SIZE: f32 = 32.0;
const UTIL_ICON_SIZE: f32 = 21.0;
const VOL_BAR_SIZE: f32 = 21.0;
const LINE_HEIGHT: f32 = 1.2;

// Hover
const HOVER_OPACITY_DEFAULT: f32 = 0.7;

// Volume
const VOL_SCROLL_STEP: f32 = 0.05;
const VOL_MAX: f32 = 2.0;

// Timers
const TIMER_SCROLL_STEP: i64 = 60;

// Timing
const TICK_MS: u64 = 100;
const AUDIO_REFRESH_COOLDOWN: u64 = 1;

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
    clock: Rect,
    notif: Rect,
    weather: Rect,
    timer1: Rect,
    timer2: Rect,
    volume: Rect,
    audio: Rect,
}

fn layout(w: u32, h: u32) -> Layout {
    let lm = LEFT_MARGIN as u32;
    let right = w - lm;
    // Top band: clock (left) + weather (right)
    let top_y: u32 = 8;
    let top_h: u32 = 78;
    // Bottom section: 3 rows — icons stacked left, timers stacked right
    let sec_y: u32 = 95;
    let sec_h = h - sec_y;
    let row_h = sec_h / 3;
    let r0 = sec_y;
    let r1 = sec_y + row_h;
    let r2 = sec_y + row_h * 2;
    Layout {
        clock: Rect { x: lm, y: top_y, w: 240, h: top_h },
        weather: Rect { x: right - 160, y: top_y, w: 160, h: top_h },
        toggle: Rect { x: lm, y: r0, w: 32, h: row_h },
        notif: Rect { x: lm, y: r1, w: 32, h: row_h },
        audio: Rect { x: lm, y: r2, w: 32, h: row_h },
        volume: Rect { x: lm + 36, y: r2, w: 200, h: row_h },
        timer2: Rect { x: right - 120, y: r1, w: 120, h: row_h },
        timer1: Rect { x: right - 120, y: r2, w: 120, h: row_h },
    }
}

// --- Hover ---

#[derive(PartialEq, Clone, Copy)]
enum HoverTile { None, Toggle, Notif, Timer1, Timer2, Volume, Audio }

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
    font_family: String,
    icon_family: String,
    // Timer state
    timer1_duration: i64,
    timer1_started: u64,
    timer2_duration: i64,
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
    // Base durations for reset (scroll-adjusted)
    timer1_base: i64,
    timer2_base: i64,
    volume_set_at: u64,
    // Weather
    weather_temp: f64,
    weather_feels: f64,
    weather_code: u32,
    weather_is_day: bool,
    weather_fetched: u64,
    weather_fetch: Option<Child>,
    // Notifications
    notif_paused: bool,
}

impl App {
    fn state(&self) -> State {
        State {
            timer1_duration: self.timer1_duration,
            timer1_started: self.timer1_started,
            timer1_base: self.timer1_base,
            timer2_duration: self.timer2_duration,
            timer2_started: self.timer2_started,
            timer2_base: self.timer2_base,
            weather_temp: self.weather_temp,
            weather_feels: self.weather_feels,
            weather_code: self.weather_code,
            weather_is_day: self.weather_is_day,
            weather_fetched: self.weather_fetched,
        }
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
        let lay = layout(self.width, self.height);

        let stride = self.width as i32 * 4;
        let (wl_buf, canvas) = self.pool
            .create_buffer(self.width as i32, self.height as i32, stride, wl_shm::Format::Argb8888)
            .unwrap();

        let mut pixmap = Pixmap::new(self.width, self.height).unwrap();
        pixmap.fill(tiny_skia::Color::TRANSPARENT);

        let pw = pixmap.width();
        let ph = pixmap.height();

        // Full-bleed background
        fill_rect_alpha(pixmap.data_mut(), pw, ph, 0, 0, self.width, self.height, bg, bg_a);

        // Accent bar (left edge, full height)
        fill_rect(pixmap.data_mut(), pw, ph, 0, 0, ACCENT_W, self.height, c.accent);

        let fa = &self.icon_family;

        // --- Clock (top-left, hero) ---
        let now = chrono_now();
        let hm_str = format!("{:02}:{:02}", now.0, now.1);
        let clock_y = lay.clock.y as f32 + 4.0;
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            &hm_str, LEFT_MARGIN, clock_y,
            CLOCK_HM_SIZE, lay.clock.w as f32, lay.clock.h as f32, c.clock,
            &self.font_family, Weight::BOLD);

        // Date below clock
        let date_str = format_date();
        let date_y = clock_y + CLOCK_HM_SIZE * LINE_HEIGHT + 2.0;
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            &date_str, LEFT_MARGIN, date_y,
            DATE_SIZE, lay.clock.w as f32, 30.0, alpha_color(c.clock, 0.75),
            &self.font_family, Weight::BOLD);

        // --- Weather (top-right) ---
        if self.weather_fetched > 0 {
            let icon = weather_icon(self.weather_code, self.weather_is_day);
            let temp_str = format!("{:.0}°", self.weather_temp);
            let feels_str = format!("{:.0}°", self.weather_feels);
            let icon_w = measure_text(&mut self.font_system, icon, WEATHER_ICON_SIZE, fa, Weight::NORMAL);
            let temp_w = measure_text(&mut self.font_system, &temp_str, WEATHER_TEMP_SIZE, &self.font_family, Weight::BOLD);
            let gap = 6.0;
            let block_w = icon_w + gap + temp_w;
            let weather_right = (lay.weather.x + lay.weather.w) as f32;
            let weather_x = weather_right - block_w;
            let weather_y = lay.weather.y as f32 + 4.0;
            let icon_y = weather_y + (WEATHER_TEMP_SIZE - WEATHER_ICON_SIZE) * 0.5;
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                icon, weather_x, icon_y,
                WEATHER_ICON_SIZE, 50.0, 50.0, c.weather,
                fa, Weight::NORMAL);
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                &temp_str, weather_x + icon_w + gap, weather_y,
                WEATHER_TEMP_SIZE, 100.0, 50.0, c.weather,
                &self.font_family, Weight::BOLD);
            // Feels-like below, right-aligned
            let feels_w = measure_text(&mut self.font_system, &feels_str, WEATHER_FEELS_SIZE, &self.font_family, Weight::BOLD);
            let feels_x = weather_right - feels_w;
            let feels_y = weather_y + WEATHER_TEMP_SIZE * LINE_HEIGHT + 2.0;
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                &feels_str, feels_x, feels_y,
                WEATHER_FEELS_SIZE, 100.0, 30.0, alpha_color(c.weather, 0.5),
                &self.font_family, Weight::BOLD);
        }

        // --- Left icon column (toggle, notif, audio — stacked vertically) ---
        let icon_x = lay.toggle.x as f32 + 2.0;

        // Toggle icon (sun/moon, top)
        let icon_char = if self.weather_is_day { "\u{f185}" } else { "\u{f186}" };
        let mut icon_color = c.sun;
        icon_color = alpha_color(icon_color, if self.hover == HoverTile::Toggle { 1.0 } else { HOVER_OPACITY_DEFAULT });
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            icon_char, icon_x + 1.0, lay.toggle.y as f32 + 6.0,
            UTIL_ICON_SIZE, 30.0, 30.0, icon_color,
            fa, Weight::BLACK);

        // Notif icon (middle)
        let notif_icon = if self.notif_paused { "\u{f1f6}" } else { "\u{f0f3}" };
        let notif_color = alpha_color(c.notif, if self.hover == HoverTile::Notif { 1.0 } else { HOVER_OPACITY_DEFAULT });
        let notif_w_on = measure_text(&mut self.font_system, "\u{f0f3}", UTIL_ICON_SIZE, fa, Weight::BLACK);
        let notif_w_off = measure_text(&mut self.font_system, "\u{f1f6}", UTIL_ICON_SIZE, fa, Weight::BLACK);
        let notif_w_cur = if self.notif_paused { notif_w_off } else { notif_w_on };
        let notif_x = icon_x + (notif_w_on.max(notif_w_off) - notif_w_cur) / 2.0;
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            notif_icon, notif_x, lay.notif.y as f32 + 6.0,
            UTIL_ICON_SIZE, 30.0, 30.0, notif_color,
            fa, Weight::BLACK);

        // Audio icon (bottom)
        let audio_icon = if self.headphones { "\u{f025}" } else { "\u{f028}" };
        let ai_alpha = if self.muted { 0.3 } else { 1.0 };
        let ai_hover = if self.hover == HoverTile::Audio { 1.0 } else { HOVER_OPACITY_DEFAULT };
        let ai_w = measure_text(&mut self.font_system, audio_icon, UTIL_ICON_SIZE, fa, Weight::BLACK);
        let ai_x = lay.audio.x as f32 + (lay.audio.w as f32 - ai_w) / 2.0 - 2.0;
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            audio_icon, ai_x, lay.audio.y as f32 + 6.0,
            UTIL_ICON_SIZE, 30.0, 30.0, alpha_color(c.audio, ai_alpha * ai_hover),
            fa, Weight::BLACK);

        // --- Volume bar (same row as audio) ---
        let vol_steps: usize = 16;
        let vol_pct = (self.volume / VOL_MAX * vol_steps as f32).round() as usize;
        let filled_count = vol_pct.min(vol_steps);
        let vol_hover = if self.hover == HoverTile::Volume { 1.0 } else { HOVER_OPACITY_DEFAULT };
        let vol_alpha = if self.muted { 0.3 } else { 1.0 };
        let vol_x = lay.volume.x as f32;
        let block_w = measure_text(&mut self.font_system, "\u{2588}", VOL_BAR_SIZE, &self.font_family, Weight::BOLD);
        let space_w = measure_text(&mut self.font_system, " ", VOL_BAR_SIZE, &self.font_family, Weight::BOLD);
        let step = block_w + space_w * 0.25 - 1.0;
        for i in 0..vol_steps {
            let (ch, alpha) = if i < filled_count { ("\u{2588}", vol_alpha * vol_hover) } else { ("\u{2591}", 0.55 * vol_hover) };
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                ch, vol_x + i as f32 * step, lay.audio.y as f32 + 6.0,
                VOL_BAR_SIZE, block_w + 1.0, 30.0, alpha_color(c.volume, alpha),
                &self.font_family, Weight::BOLD);
        }

        // --- Timers (bottom-right, stacked: short on top, long on bottom) ---
        let t2_rem = timer_remaining(self.timer2_duration, self.timer2_started);
        let t2_str = format_timer(t2_rem);
        let t2_alpha = if self.timer2_started > 0 { 1.0 } else { 0.7 };
        let t2_hover = if self.hover == HoverTile::Timer2 { 1.0 } else { HOVER_OPACITY_DEFAULT };
        let t2_w = measure_text(&mut self.font_system, &t2_str, TIMER_SIZE, &self.font_family, Weight::BOLD);
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            &t2_str, (lay.timer2.x + lay.timer2.w) as f32 - t2_w, lay.timer2.y as f32 + 6.0,
            TIMER_SIZE, lay.timer2.w as f32, lay.timer2.h as f32, alpha_color(c.timer, t2_alpha * t2_hover),
            &self.font_family, Weight::BOLD);

        let t1_rem = timer_remaining(self.timer1_duration, self.timer1_started);
        let t1_str = format_timer(t1_rem);
        let t1_alpha = if self.timer1_started > 0 { 1.0 } else { 0.7 };
        let t1_hover = if self.hover == HoverTile::Timer1 { 1.0 } else { HOVER_OPACITY_DEFAULT };
        let t1_w = measure_text(&mut self.font_system, &t1_str, TIMER_SIZE, &self.font_family, Weight::BOLD);
        render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
            &t1_str, (lay.timer1.x + lay.timer1.w) as f32 - t1_w, lay.timer1.y as f32 + 6.0,
            TIMER_SIZE, lay.timer1.w as f32, lay.timer1.h as f32, alpha_color(c.timer, t1_alpha * t1_hover),
            &self.font_family, Weight::BOLD);

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

        if lay.notif.contains(mx, my) {
            Command::new("dunstctl").arg("set-paused").arg("toggle").spawn().ok();
            self.notif_paused = !self.notif_paused;
            self.draw();
            return;
        }

        if lay.timer1.contains(mx, my) {
            if self.timer1_started > 0 {
                let rem = timer_remaining(self.timer1_duration, self.timer1_started);
                self.timer1_duration = rem;
                self.timer1_started = 0;
            } else {
                self.timer1_started = now_unix();
            }
            save_state(&self.state());
            self.draw();
            return;
        }

        if lay.timer2.contains(mx, my) {
            if self.timer2_started > 0 {
                let rem = timer_remaining(self.timer2_duration, self.timer2_started);
                self.timer2_duration = rem;
                self.timer2_started = 0;
            } else {
                self.timer2_started = now_unix();
            }
            save_state(&self.state());
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
            self.timer1_duration = (self.timer1_duration + delta).max(TIMER_SCROLL_STEP);
            self.timer1_base = self.timer1_duration;
            save_state(&self.state());
            self.draw();
            return;
        }

        if lay.timer2.contains(mx, my) {
            let delta: i64 = if dy > 0.0 { -TIMER_SCROLL_STEP } else { TIMER_SCROLL_STEP };
            self.timer2_duration = (self.timer2_duration + delta).max(TIMER_SCROLL_STEP);
            self.timer2_base = self.timer2_duration;
            save_state(&self.state());
            self.draw();
        }
    }

    fn handle_right_click(&mut self, x: f64, y: f64) {
        let (mx, my) = (x as u32, y as u32);
        let lay = layout(self.width, self.height);

        if lay.timer1.contains(mx, my) {
            self.timer1_duration = self.timer1_base;
            self.timer1_started = 0;
            save_state(&self.state());
            self.draw();
            return;
        }

        if lay.timer2.contains(mx, my) {
            self.timer2_duration = self.timer2_base;
            self.timer2_started = 0;
            save_state(&self.state());
            self.draw();
        }
    }

    fn hover_tile_at(&self, x: f64, y: f64) -> HoverTile {
        let (mx, my) = (x as u32, y as u32);
        let lay = layout(self.width, self.height);

        if lay.toggle.contains(mx, my) { return HoverTile::Toggle; }
        if lay.notif.contains(mx, my) { return HoverTile::Notif; }
        if lay.timer1.contains(mx, my) { return HoverTile::Timer1; }
        if lay.timer2.contains(mx, my) { return HoverTile::Timer2; }
        if lay.volume.contains(mx, my) { return HoverTile::Volume; }
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
                PointerEventKind::Release { .. } => {}
                PointerEventKind::Motion { .. } => {
                    let new_hover = self.hover_tile_at(event.position.0, event.position.1);
                    if new_hover != self.hover {
                        self.hover = new_hover;
                        self.draw();
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
    let st = load_state(&cfg);
    let weather_fetch = if cfg.weather_lat != 0.0 && now_unix() - st.weather_fetched > WEATHER_MAX_AGE {
        Command::new("curl")
            .args(["-s", "--max-time", "5", &format!(
                "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,apparent_temperature,weather_code,is_day&temperature_unit=fahrenheit",
                cfg.weather_lat, cfg.weather_lon)])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn().ok()
    } else {
        None
    };

    let notif_paused = Command::new("dunstctl").arg("is-paused").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true").unwrap_or(false);

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
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("wavedash"), None);
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
    // Load FA Regular for outline icons (same family, Weight::NORMAL)
    if let Ok(data) = std::fs::read("/usr/share/fonts/OTF/Font Awesome 7 Free-Regular-400.otf") {
        db.load_font_data(data);
    }
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
        font_family,
        icon_family,
        timer1_duration: st.timer1_duration,
        timer1_started: st.timer1_started,
        timer2_duration: st.timer2_duration,
        timer2_started: st.timer2_started,
        volume,
        muted,
        headphones,
        bt_device_1: cfg.bt_device_1,
        bt_device_2: cfg.bt_device_2,
        is_dim: false,
        hover: HoverTile::None,
        timer1_base: st.timer1_base,
        timer2_base: st.timer2_base,
        volume_set_at: 0,
        weather_temp: st.weather_temp,
        weather_feels: st.weather_feels,
        weather_code: st.weather_code,
        weather_is_day: st.weather_is_day,
        weather_fetched: st.weather_fetched,
        weather_fetch,
        notif_paused,
    };

    // 1-second timer for clock/timer redraws
    let timer = Timer::from_duration(std::time::Duration::from_millis(TICK_MS));
    event_loop.handle().insert_source(timer, |_, _, app| {
        if now_unix() - app.volume_set_at >= AUDIO_REFRESH_COOLDOWN {
            app.refresh_audio();
        }
        // Poll background weather fetch
        let done = match app.weather_fetch.as_mut() {
            Some(child) => child.try_wait().ok().flatten().is_some(),
            None => false,
        };
        if done {
            let child = app.weather_fetch.take().unwrap();
            if let Ok(output) = child.wait_with_output() {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    // Scope to "current":{ to skip "current_units"
                    if let Some(ci) = text.find("\"current\":{") {
                        let s = &text[ci..];
                        let num_at = |s: &str, needle: &str| -> Option<f64> {
                            let after = s[s.find(needle)? + needle.len()..].trim_start();
                            after[..after.find(|c: char| c == ',' || c == '}')?].trim().parse().ok()
                        };
                        let temp = num_at(s, "\"temperature_2m\":");
                        let feels = num_at(s, "\"apparent_temperature\":");
                        let code = num_at(s, "\"weather_code\":").map(|v| v as u32);
                        let is_day = num_at(s, "\"is_day\":").map(|v| v as u32 == 1);
                        if let (Some(temp), Some(feels), Some(code), Some(is_day)) = (temp, feels, code, is_day) {
                            app.weather_temp = temp;
                            app.weather_feels = feels;
                            app.weather_code = code;
                            app.weather_is_day = is_day;
                            app.weather_fetched = now_unix();
                            save_state(&app.state());
                        }
                    }
                }
            }
        }
        app.draw();
        TimeoutAction::ToDuration(std::time::Duration::from_millis(TICK_MS))
    }).unwrap();

    loop {
        event_loop.dispatch(std::time::Duration::from_millis(TICK_MS), &mut app).unwrap();
        if app.exit {
            save_state(&app.state());
            break;
        }
    }
}
