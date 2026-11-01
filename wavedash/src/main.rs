use std::path::PathBuf;
use libc;
use std::process::{Command, Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
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
use tiny_skia::{LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

// --- Config ---

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    color_file: Option<String>,
    font: String,
    font_bold: String,
    timer1_duration: u64,
    timer2_duration: u64,
    bt_device_1: String,
    audio_device_2: String,
    weather_lat: f64,
    weather_lon: f64,
    weather_max_age: u64,
    alert_sound: String,
    alert_volume: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            color_file: Some("~/.cache/wal/colors-wavedash.toml".into()),
            font: "~/.local/share/fonts/GoogleSansCode-Regular.ttf".into(),
            font_bold: "~/.local/share/fonts/GoogleSansCode-Bold.ttf".into(),
            timer1_duration: 3600,
            timer2_duration: 900,
            bt_device_1: "AC:BF:71:08:A1:D6".into(),
            audio_device_2: "EC:81:93:AC:8B:60".into(),
            weather_lat: 0.0,
            weather_lon: 0.0,
            weather_max_age: 3600,
            alert_sound: "~/.config/widgets/wavedash-alarm.wav".into(),
            alert_volume: 1.0,
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

#[derive(Clone, Copy)]
struct Colors {
    background: [u8; 3],
    background_alpha: u8,
    foreground: [u8; 3],
    border: [u8; 3],
    divider: [u8; 3],
    clock: [u8; 3],
    accentr: [u8; 3],
    weather: [u8; 3],
    volume: [u8; 3],
    brightness: [u8; 3],
    battery: [u8; 3],
    battery_charging: [u8; 3],
    battery_low: [u8; 3],
    timer: [u8; 3],
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            background: [0x1e, 0x1e, 0x2e],
            background_alpha: 0xe6, // ~0.9
            foreground: [0xcd, 0xd6, 0xf4],
            border: [0xcd, 0xd6, 0xf4],
            divider: [0xcd, 0xd6, 0xf4],
            clock: [0x89, 0xb4, 0xfa],
            accentr: [0x89, 0xb4, 0xfa],
            weather: [0x94, 0xe2, 0xd5],
            volume: [0xcb, 0xa6, 0xf7],
            brightness: [0xcb, 0xa6, 0xf7],
            battery: [0xcb, 0xa6, 0xf7],
            battery_charging: [0xa6, 0xe3, 0xa1],
            battery_low: [0xf3, 0x8b, 0xa8],
            timer: [0xcb, 0xa6, 0xf7],
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
                            "foreground" => colors.foreground = c,
                            "border" => colors.border = c,
                            "divider" => colors.divider = c,
                            "clock" => colors.clock = c,
                            "accentr" => colors.accentr = c,
                            "weather" => colors.weather = c,
                            "volume" => colors.volume = c,
                            "brightness" => colors.brightness = c,
                            "battery" => colors.battery = c,
                            "battery_charging" => colors.battery_charging = c,
                            "battery_low" => colors.battery_low = c,
                            "timer" => colors.timer = c,
                            _ => {}
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
    #[serde(default)] alert: bool,
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

// --- Alarm ---
// A detached `wavedash --alarm <n> <started>` process watches timer n in the state file and plays
// the alert sound when it reaches zero. Pausing or restarting the timer changes `started`, which
// makes the watcher exit, so nothing has to be cancelled explicitly.

fn play_alert(sound: &std::path::Path, volume: f32) -> Child {
    Command::new("pw-play").args(["--volume", &volume.to_string()]).arg(sound)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
}

fn spawn_alarm(n: u8, started: u64) {
    Command::new("pkill").args(["-f", &format!("wavedash --alarm {n}")]).status().unwrap();
    Command::new(std::env::current_exe().unwrap()).args(["--alarm", &n.to_string(), &started.to_string()])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
}

fn run_alarm(n: u8, started: u64, cfg: &Config) {
    // Renamed so the toggle script's `pkill -x wavedash` closes the panel without taking the watcher down
    unsafe { libc::prctl(libc::PR_SET_NAME, b"wavedash-alarm\0".as_ptr()); }
    let mut player: Option<Child> = None;
    loop {
        let st = load_state(cfg);
        let (t_started, t_dur) = if n == 1 { (st.timer1_started, st.timer1_duration) } else { (st.timer2_started, st.timer2_duration) };
        if t_started != started { break; }
        if let Some(p) = player.as_mut() {
            if p.try_wait().unwrap().is_some() { return; }
        } else if timer_remaining(t_dur, t_started) <= 0 {
            if !st.alert { return; }
            if !Command::new("pgrep").args(["-x", "wavedash"]).status().unwrap().success() {
                Command::new(std::env::current_exe().unwrap()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            }
            player = Some(play_alert(&expand_path(&cfg.alert_sound), cfg.alert_volume));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if let Some(mut p) = player { p.kill().unwrap(); }
}


// WMO weather code -> (index into WEATHER_ICONS, word)
fn weather_kind(code: u32, is_day: bool) -> (usize, &'static str) {
    match code {
        0 | 1 => (if is_day { 0 } else { 1 }, "clear"),
        45 | 48 => (3, "fog"),
        51..=67 | 80..=82 => (4, "rain"),
        71..=77 | 85 | 86 => (5, "snow"),
        95 | 96 | 99 => (6, "storm"),
        _ => (2, "cloudy"),
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
    assert!(Command::new("wpctl").args(["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{v:.2}")]).status().unwrap().success(), "wpctl set-volume failed");
}

fn is_headphones(bt_mac: &str) -> bool {
    let out = Command::new("wpctl").args(["inspect", "@DEFAULT_AUDIO_SINK@"]).output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).to_lowercase();
            let mac_lower = bt_mac.to_lowercase();
            let mac_underscored = mac_lower.replace(':', "_");
            s.contains(&mac_lower) || s.contains(&mac_underscored)
                || s.contains("headphone") || s.contains("headset")
        }
        Err(_) => false,
    }
}

fn toggle_mute() {
    assert!(Command::new("wpctl").args(["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]).status().unwrap().success(), "wpctl set-mute failed");
}

fn switch_audio(target: &str) {
    let out = Command::new("pactl").args(["list", "short", "sinks"]).output().unwrap();
    assert!(out.status.success(), "pactl list sinks: {}", String::from_utf8_lossy(&out.stderr));
    let sinks = String::from_utf8(out.stdout).unwrap();
    let target = target.replace(':', "_").to_lowercase();
    let sink = sinks.lines().filter_map(|line| line.split_whitespace().nth(1))
        .find(|sink| sink.to_lowercase().contains(&target)).expect("target audio sink is unavailable");
    assert!(Command::new("pactl").args(["set-default-sink", sink]).status().unwrap().success(), "pactl set-default-sink failed");
}

// --- Battery ---

// Returns (charge 0.0-1.0, plugged in). Plugged covers "Charging", "Full",
// and "Not charging" (held at a charge limit) — anything but "Discharging".
fn read_battery() -> Option<(f32, bool)> {
    for entry in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let path = entry.path();
        if std::fs::read_to_string(path.join("type")).map_or(true, |t| t.trim() != "Battery") { continue }
        let Ok(cap) = std::fs::read_to_string(path.join("capacity")) else { continue };
        let Ok(pct) = cap.trim().parse::<f32>() else { continue };
        let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
        let plugged = status.trim() != "Discharging";
        return Some(((pct / 100.0).clamp(0.0, 1.0), plugged));
    }
    None
}

// --- Brightness ---

fn read_brightness() -> Option<f32> {
    for entry in std::fs::read_dir("/sys/class/backlight").ok()?.flatten() {
        let path = entry.path();
        let Ok(cur) = std::fs::read_to_string(path.join("brightness")) else { continue };
        let Ok(max) = std::fs::read_to_string(path.join("max_brightness")) else { continue };
        let (Ok(cur), Ok(max)) = (cur.trim().parse::<f32>(), max.trim().parse::<f32>()) else { continue };
        if max > 0.0 { return Some((cur / max).clamp(0.0, 1.0)); }
    }
    None
}

fn set_brightness(frac: f32) {
    let pct = (frac.clamp(0.0, 1.0) * 100.0).round() as u32;
    assert!(Command::new("brightnessctl").args(["-q", "set", &format!("{pct}%")]).status().unwrap().success(), "brightnessctl set failed");
}

// --- Signal (long press detection) ---

static GOT_SIGUSR2: AtomicBool = AtomicBool::new(false);

extern "C" fn sigusr2_handler(_: libc::c_int) {
    GOT_SIGUSR2.store(true, Ordering::Release);
}

// --- Layout constants ---
// Sizes are in design px; S scales the whole panel.

const S: f32 = 1.0;
const WIDTH: f32 = 440.0 * S;
const PAD: f32 = 6.0 * S;          // panel edge to frame
const LINE: f32 = 1.0 * S;         // frame and divider thickness
const INSET_X: f32 = 10.0 * S;     // frame to content
const INSET_Y: f32 = 8.0 * S;
const ROW_H: f32 = 22.0 * S;
const DIV_H: f32 = 17.0 * S;       // divider row: space, rule, space
const LABEL_W: f32 = 48.0 * S;     // icon + label column of level and timer rows
const VALUE_W: f32 = 92.0 * S;     // value + state word column
const GAP: f32 = 10.0 * S;
const ICON: f32 = 14.0 * S;
const ICON_GAP: f32 = 7.0 * S;     // icon to its label
const TOGGLE_GAP: f32 = 18.0 * S;
const BAR_T: f32 = 8.0 * S;        // bar thickness
const TEXT: f32 = 12.0 * S;
const CLOCK: f32 = 14.0 * S;
const LINE_HEIGHT: f32 = 1.2;

// Opacities, blended against the background
const DIM: f32 = 0.6;     // labels and secondary text
const OFF: f32 = 0.32;    // off toggles, muted volume
const TRACK: f32 = 0.14;  // empty part of a bar
const RULE: f32 = 0.18;   // frame and dividers

// Volume
const VOL_SCROLL_STEP: f32 = 0.05;
const VOL_MAX: f32 = 2.0;

// Battery
const BATTERY_REFRESH: u64 = 5;
const BATTERY_LOW: f32 = 0.20;

// Brightness (min keeps scroll from blacking out the screen)
const BRIGHTNESS_SCROLL_STEP: f32 = 0.05;
const BRIGHTNESS_MIN: f32 = 0.05;

// Timers
const TIMER_SCROLL_STEP: i64 = 60;

// Timing
const TICK_MS: u64 = 33;
const AUDIO_REFRESH_MS: u128 = 500;
const AUDIO_REFRESH_COOLDOWN: u64 = 1;
const LONG_PRESS_GRACE_MS: u64 = 300;

// Header, level rows, two timers and the toggle row, with a divider between each group
fn panel_height(levels: u32) -> u32 {
    (2.0 * (PAD + LINE + INSET_Y) + ROW_H * (4 + levels) as f32 + 3.0 * DIV_H).round() as u32
}

// --- Geometry ---

#[derive(Clone, Copy, Default)]
struct Rect { x: f32, y: f32, w: f32, h: f32 }

impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    // The bar of a level or timer row, between the label and value columns
    fn bar(&self) -> Rect {
        Rect { x: self.x + LABEL_W + GAP, w: self.w - LABEL_W - VALUE_W - 2.0 * GAP, ..*self }
    }
}

// Rows and buttons; anything a machine lacks is a zero-size Rect
struct Layout {
    dividers: [f32; 3],
    time: Rect,
    date: Rect,
    weather: Rect,
    volume: Rect,
    brightness: Rect,
    battery: Rect,
    timer1: Rect,
    timer2: Rect,
    notif: Rect,
    alarm: Rect,
    output: Rect,
    dim: Rect,
}

#[derive(PartialEq, Clone, Copy)]
enum Hover { None, Date, Volume, Brightness, Timer1, Timer2, Notif, Alarm, Output, Dim }

#[derive(PartialEq, Clone, Copy)]
enum Drag { Volume, Brightness, Timer1, Timer2 }

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
    icons: Icons,
    // Advance of one character (the font is monospace): body text and clock
    cw: f32,
    clock_cw: f32,
    // Timer state
    timer1_duration: i64,
    timer1_started: u64,
    timer2_duration: i64,
    timer2_started: u64,
    alert: bool,
    alert_sound: PathBuf,
    alert_volume: f32,
    // Audio
    volume: f32,
    muted: bool,
    headphones: bool,
    bt_device_1: String,
    audio_device_2: String,
    // Dim toggle (hyprsunset via dim_toggle.sh); only shown when there is no backlight
    is_dim: bool,
    hover: Hover,
    drag: Option<Drag>,
    // Scroll and drag only update these; the tick applies them, at most once per frame
    volume_dirty: bool,
    brightness_dirty: bool,
    // Base durations for reset (scroll-adjusted)
    timer1_base: i64,
    timer2_base: i64,
    // Config-defined durations (middle-click reset target, and the scale of the timer bars)
    timer1_config: i64,
    timer2_config: i64,
    volume_set_at: u64,
    audio_refreshed: Instant,
    // Weather
    weather_temp: f64,
    weather_feels: f64,
    weather_code: u32,
    weather_is_day: bool,
    weather_fetched: u64,
    weather_fetch: Option<Child>,
    // Battery: (charge 0.0-1.0, plugged in); None if no battery present
    battery: Option<(f32, bool)>,
    battery_read_at: u64,
    // Brightness: backlight level 0.0-1.0; None if no backlight present
    brightness: Option<f32>,
    brightness_set_at: u64,
    // Notifications
    notif_paused: bool,
    // Long press: None = undecided (grace period), Some(true) = exit on key release, Some(false) = persistent
    long_press: Option<bool>,
    startup: Instant,
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
            alert: self.alert,
        }
    }

    fn refresh_audio(&mut self) {
        let (v, m) = get_volume();
        self.volume = v;
        self.muted = m;
        self.headphones = is_headphones(&self.bt_device_1);
    }

    // Left click: start a stopped timer, pause a running one, or reset a finished one to its base
    fn click_timer(&mut self, n: u8) {
        let (dur, started, base) = if n == 1 { (&mut self.timer1_duration, &mut self.timer1_started, self.timer1_base) } else { (&mut self.timer2_duration, &mut self.timer2_started, self.timer2_base) };
        if *started > 0 {
            let rem = timer_remaining(*dur, *started);
            *dur = if rem <= 0 { base } else { rem };
            *started = 0;
        } else {
            *started = now_unix();
        }
        let started = *started;
        save_state(&self.state());
        if started > 0 { spawn_alarm(n, started); }
    }

    // Stop a timer at its base (right click) or at the config duration, which also becomes the base (middle click)
    fn reset_timer(&mut self, n: u8, to_config: bool) {
        let (dur, started, base, config) = if n == 1 { (&mut self.timer1_duration, &mut self.timer1_started, &mut self.timer1_base, self.timer1_config) } else { (&mut self.timer2_duration, &mut self.timer2_started, &mut self.timer2_base, self.timer2_config) };
        if to_config { *base = config; }
        *dur = *base;
        *started = 0;
        save_state(&self.state());
    }

    fn output_label(&self) -> &'static str {
        if self.headphones { "headphones" } else { "speakers" }
    }

    // ("54° clear", "feels 51°"), or None before the first fetch
    fn weather_text(&self) -> Option<(String, String)> {
        if self.weather_fetched == 0 { return None; }
        let word = weather_kind(self.weather_code, self.weather_is_day).1;
        Some((format!("{:.0}° {word}", self.weather_temp), format!("feels {:.0}°", self.weather_feels)))
    }

    fn layout(&self) -> Layout {
        let x = PAD + LINE + INSET_X;
        let w = self.width as f32 - 2.0 * x;
        let row = |y: f32| Rect { x, y, w, h: ROW_H };
        let text_w = |s: &str| s.chars().count() as f32 * self.cw;
        let rule = |y: f32| y + (DIV_H - LINE) / 2.0;

        // Header: time, date and weather spread evenly across the row
        let mut y = PAD + LINE + INSET_Y;
        let time_w = clock_text().chars().count() as f32 * self.clock_cw;
        let date_w = ICON + ICON_GAP + text_w(&format_date());
        let weather_w = self.weather_text().map_or(0.0, |(a, b)| ICON + ICON_GAP + text_w(&a) + GAP + text_w(&b));
        let gap = (w - time_w - date_w - weather_w) / if weather_w > 0.0 { 2.0 } else { 1.0 };
        let time = Rect { x, y, w: time_w, h: ROW_H };
        let date = Rect { x: x + time_w + gap, y, w: date_w, h: ROW_H };
        let weather = Rect { x: x + w - weather_w, y, w: weather_w, h: ROW_H };
        y += ROW_H;
        let d0 = rule(y);
        y += DIV_H;

        let volume = row(y);
        y += ROW_H;
        let brightness = if self.brightness.is_some() { y += ROW_H; row(y - ROW_H) } else { Rect::default() };
        let battery = if self.battery.is_some() { y += ROW_H; row(y - ROW_H) } else { Rect::default() };
        let d1 = rule(y);
        y += DIV_H;

        let timer1 = row(y);
        let timer2 = row(y + ROW_H);
        y += 2.0 * ROW_H;
        let d2 = rule(y);
        y += DIV_H;

        // Toggles, left to right
        let mut tx = x;
        let mut toggle = |label: &str| {
            let r = Rect { x: tx, y, w: ICON + ICON_GAP + text_w(label), h: ROW_H };
            tx += r.w + TOGGLE_GAP;
            r
        };
        let notif = toggle("notif");
        let alarm = toggle("alarm");
        let output = toggle(self.output_label());
        let dim = if self.brightness.is_none() { toggle("dim") } else { Rect::default() };

        Layout { dividers: [d0, d1, d2], time, date, weather, volume, brightness, battery, timer1, timer2, notif, alarm, output, dim }
    }

    fn hover_at(&self, x: f32, y: f32) -> Hover {
        let l = self.layout();
        [(l.date, Hover::Date), (l.volume, Hover::Volume), (l.brightness, Hover::Brightness), (l.timer1, Hover::Timer1),
         (l.timer2, Hover::Timer2), (l.notif, Hover::Notif), (l.alarm, Hover::Alarm), (l.output, Hover::Output), (l.dim, Hover::Dim)]
            .into_iter().find(|(r, _)| r.contains(x, y)).map_or(Hover::None, |(_, h)| h)
    }

    fn draw(&mut self) {
        let lay = self.layout();
        let c = self.colors;
        let hv = self.hover;
        let (w, h) = (self.width, self.height);
        let clock = clock_text();
        let date = format_date();
        let weather = self.weather_text();
        let pulse_t = self.startup.elapsed().as_secs_f32();
        let output = self.output_label();

        let (wl_buf, canvas) = self.pool
            .create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888)
            .unwrap();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        fill_rect_alpha(pixmap.data_mut(), w, h, 0, 0, w, h, c.background, c.background_alpha);

        let fg = c.foreground;
        let (dim, off) = (fade(fg, c.background, DIM), fade(fg, c.background, OFF));
        let label = |which: Hover| if hv == which { fg } else { dim };
        let ic = &self.icons;
        let mut p = Painter {
            pm: &mut pixmap, fs: &mut self.font_system, sc: &mut self.swash_cache, family: &self.font_family, cw: self.cw,
            fg, dim, off, track: fade(fg, c.background, TRACK),
        };

        // Frame and dividers
        let (fw, fh) = (w as f32 - 2.0 * PAD, h as f32 - 2.0 * PAD);
        let border = fade(c.border, c.background, RULE);
        p.fill(PAD, PAD, fw, LINE, border);
        p.fill(PAD, PAD + fh - LINE, fw, LINE, border);
        p.fill(PAD, PAD, LINE, fh, border);
        p.fill(PAD + fw - LINE, PAD, LINE, fh, border);
        for y in lay.dividers { p.fill(PAD + LINE, y, fw - 2.0 * LINE, LINE, fade(c.divider, c.background, RULE)); }

        // Header
        p.text(&clock, lay.time.x, lay.time.y, CLOCK, Weight::BOLD, c.clock);
        let date_c = label(Hover::Date);
        p.icon(&ic.cal, lay.date.x, lay.date.y, date_c);
        p.text(&date, lay.date.x + ICON + ICON_GAP, lay.date.y, TEXT, Weight::NORMAL, date_c);
        if let Some((now, feels)) = &weather {
            let x = lay.weather.x + ICON + ICON_GAP;
            p.icon(&ic.weather[weather_kind(self.weather_code, self.weather_is_day).0], lay.weather.x, lay.weather.y, c.weather);
            p.text(now, x, lay.weather.y, TEXT, Weight::NORMAL, c.weather);
            p.text(feels, x + now.chars().count() as f32 * self.cw + GAP, lay.weather.y, TEXT, Weight::NORMAL, dim);
        }

        // Levels
        let vol = format!("{:.0}%", self.volume * 100.0);
        if self.muted {
            p.row(&lay.volume, &ic.vol_off, "vol", off, self.volume / VOL_MAX, fade(c.volume, c.background, OFF), &vol, off, "muted");
        } else {
            p.row(&lay.volume, &ic.vol, "vol", label(Hover::Volume), self.volume / VOL_MAX, c.volume, &vol, hover_color(fg, hv == Hover::Volume), "");
        }
        if let Some(b) = self.brightness {
            p.row(&lay.brightness, &ic.bri, "bri", label(Hover::Brightness), b, c.brightness, &format!("{:.0}%", b * 100.0), hover_color(fg, hv == Hover::Brightness), "");
        }
        if let Some((pct, plugged)) = self.battery {
            let low = !plugged && pct <= BATTERY_LOW;
            let bar_c = if plugged { c.battery_charging } else if low { c.battery_low } else { c.battery };
            p.row(&lay.battery, &ic.bat, "bat", dim, pct, bar_c, &format!("{:.0}%", pct * 100.0), fg, if plugged { "charging" } else if low { "low" } else { "" });
            p.icon(&battery_fill(ic.n, pct, plugged.then_some(&ic.bolt[..])), lay.battery.x, lay.battery.y, bar_c);
        }

        // Timers: the bar is the remaining time on the scale of the config duration
        for (r, dur, started, config, which, name) in [
            (lay.timer1, self.timer1_duration, self.timer1_started, self.timer1_config, Hover::Timer1, "t1"),
            (lay.timer2, self.timer2_duration, self.timer2_started, self.timer2_config, Hover::Timer2, "t2"),
        ] {
            let rem = timer_remaining(dur, started);
            let state = if started == 0 { "paused" } else if rem <= 0 { "done" } else { "" };
            let value_c = timer_color(&c, pulse_t, rem, started, hv == which);
            p.row(&r, &ic.timer, name, label(which), rem as f32 / config as f32, c.timer, &format_timer(rem), value_c, state);
        }

        // Toggles
        p.toggle(&lay.notif, if self.notif_paused { &ic.bell_off } else { &ic.bell }, "notif", !self.notif_paused, hv == Hover::Notif);
        p.toggle(&lay.alarm, if self.alert { &ic.alarm } else { &ic.alarm_off }, "alarm", self.alert, hv == Hover::Alarm);
        p.toggle(&lay.output, if self.headphones { &ic.phones } else { &ic.speaker }, output, true, hv == Hover::Output);
        if self.brightness.is_none() {
            p.toggle(&lay.dim, if self.weather_is_day { &ic.bri } else { &ic.moon }, "dim", true, hv == Hover::Dim);
        }

        // Copy RGBA premul -> BGRA (ARGB8888 on LE)
        for (dst, src) in canvas.chunks_exact_mut(4).zip(pixmap.data().chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }

        wl_buf.attach_to(self.layer.wl_surface()).unwrap();
        self.layer.wl_surface().damage_buffer(0, 0, w as i32, h as i32);
        self.layer.wl_surface().commit();
    }

    fn handle_click(&mut self, x: f32, y: f32) {
        let lay = self.layout();
        let target = self.hover_at(x, y);
        // A press on a bar starts a drag; the rest of the row is a button
        self.drag = match target {
            Hover::Volume if lay.volume.bar().contains(x, y) => Some(Drag::Volume),
            Hover::Brightness if lay.brightness.bar().contains(x, y) => Some(Drag::Brightness),
            Hover::Timer1 if lay.timer1.bar().contains(x, y) => Some(Drag::Timer1),
            Hover::Timer2 if lay.timer2.bar().contains(x, y) => Some(Drag::Timer2),
            _ => None,
        };
        if self.drag.is_some() {
            self.drag_to(x);
            return;
        }
        match target {
            Hover::Volume => {
                toggle_mute();
                self.refresh_audio();
            }
            Hover::Timer1 => self.click_timer(1),
            Hover::Timer2 => self.click_timer(2),
            Hover::Date => {
                Command::new("xdg-open").arg("https://calendar.google.com").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
            }
            Hover::Notif => {
                Command::new("dunstctl").arg("set-paused").arg("toggle").spawn().ok();
                self.notif_paused = !self.notif_paused;
            }
            Hover::Alarm => {
                self.alert = !self.alert;
                save_state(&self.state());
                if self.alert {
                    // Preview the sound so output device and volume can be checked
                    play_alert(&self.alert_sound, self.alert_volume);
                } else {
                    Command::new("pkill").args(["-f", &format!("pw-play .*{}", self.alert_sound.display())]).status().unwrap();
                }
            }
            Hover::Output => {
                self.refresh_audio();
                let target = if self.headphones { &self.audio_device_2 } else { &self.bt_device_1 };
                switch_audio(target);
                self.refresh_audio();
            }
            Hover::Dim => {
                let arg = if self.is_dim { "1" } else { "0" };
                Command::new("sh").arg("-c").arg(format!("{}/wgmn/scripts/dim_toggle.sh {arg}", home().display())).spawn().ok();
                self.is_dim = !self.is_dim;
            }
            Hover::Brightness | Hover::None => {}
        }
        self.draw();
    }

    // Set the dragged value from the pointer's x; timers snap to whole minutes
    fn drag_to(&mut self, x: f32) {
        let lay = self.layout();
        let Some(drag) = self.drag else { return };
        let bar = match drag { Drag::Volume => lay.volume, Drag::Brightness => lay.brightness, Drag::Timer1 => lay.timer1, Drag::Timer2 => lay.timer2 }.bar();
        let frac = ((x - bar.x - BAR_T / 2.0) / (bar.w - BAR_T)).clamp(0.0, 1.0);
        match drag {
            Drag::Volume => {
                self.volume = frac * VOL_MAX;
                self.volume_dirty = true;
            }
            Drag::Brightness => {
                self.brightness = Some(frac.max(BRIGHTNESS_MIN));
                self.brightness_dirty = true;
            }
            Drag::Timer1 | Drag::Timer2 => {
                let (dur, started, base, config) = if drag == Drag::Timer1 { (&mut self.timer1_duration, self.timer1_started, &mut self.timer1_base, self.timer1_config) } else { (&mut self.timer2_duration, self.timer2_started, &mut self.timer2_base, self.timer2_config) };
                let secs = ((frac * config as f32 / 60.0).round() as i64 * 60).max(TIMER_SCROLL_STEP);
                // A paused timer gets a new base; a running one keeps running from the new time
                if started == 0 {
                    *dur = secs;
                    *base = secs;
                } else {
                    *dur = secs + now_unix() as i64 - started as i64;
                }
            }
        }
        self.draw();
    }

    fn end_drag(&mut self) {
        if matches!(self.drag, Some(Drag::Timer1 | Drag::Timer2)) { save_state(&self.state()); }
        self.drag = None;
    }

    fn handle_scroll(&mut self, x: f32, y: f32, dy: f64) {
        let sign = if dy > 0.0 { -1.0 } else { 1.0 };
        match self.hover_at(x, y) {
            Hover::Volume => {
                self.volume = (self.volume + sign * VOL_SCROLL_STEP).clamp(0.0, VOL_MAX);
                self.volume_dirty = true;
            }
            Hover::Brightness => {
                let Some(b) = self.brightness else { return };
                self.brightness = Some((b + sign * BRIGHTNESS_SCROLL_STEP).clamp(BRIGHTNESS_MIN, 1.0));
                self.brightness_dirty = true;
            }
            which @ (Hover::Timer1 | Hover::Timer2) => {
                let (dur, base) = if which == Hover::Timer1 { (&mut self.timer1_duration, &mut self.timer1_base) } else { (&mut self.timer2_duration, &mut self.timer2_base) };
                *dur = (*dur + sign as i64 * TIMER_SCROLL_STEP).max(TIMER_SCROLL_STEP);
                *base = *dur;
                save_state(&self.state());
            }
            _ => return,
        }
        self.draw();
    }

    fn handle_middle_click(&mut self, x: f32, y: f32) {
        match self.hover_at(x, y) {
            Hover::Volume | Hover::Output => {
                Command::new("pavucontrol").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
            }
            Hover::Timer1 => self.reset_timer(1, true),
            Hover::Timer2 => self.reset_timer(2, true),
            _ => return,
        }
        self.draw();
    }

    fn handle_right_click(&mut self, x: f32, y: f32) {
        match self.hover_at(x, y) {
            Hover::Timer1 => self.reset_timer(1, false),
            Hover::Timer2 => self.reset_timer(2, false),
            _ => return,
        }
        self.draw();
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

// "2:57 pm"
fn clock_text() -> String {
    let (h, m, _) = chrono_now();
    format!("{}:{m:02} {}", if h % 12 == 0 { 12 } else { h % 12 }, if h < 12 { "am" } else { "pm" })
}

// "Sun, Nov 1"
fn format_date() -> String {
    let secs = now_unix();
    let t = secs as i64;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&t as *const i64, &mut tm) };
    let days = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{}, {} {}", days[tm.tm_wday as usize], months[tm.tm_mon as usize], tm.tm_mday)
}

// --- Rendering helpers ---

// `c` at opacity `a` over the background color
fn fade(c: [u8; 3], bg: [u8; 3], a: f32) -> [u8; 3] {
    let mix = |i: usize| (c[i] as f32 * a + bg[i] as f32 * (1.0 - a)) as u8;
    [mix(0), mix(1), mix(2)]
}

fn hover_color(c: [u8; 3], hovered: bool) -> [u8; 3] {
    if !hovered { return c; }
    let b = |v: f32| (v + (255.0 - v) * 0.25).min(255.0) as u8;
    [b(c[0] as f32), b(c[1] as f32), b(c[2] as f32)]
}

// `t` is seconds since launch; a finished timer breathes in the accent color until acknowledged
fn timer_color(c: &Colors, t: f32, rem: i64, started: u64, hovered: bool) -> [u8; 3] {
    if started > 0 && rem <= 0 {
        let pulse = 0.5 + 0.5 * (t * std::f32::consts::PI).sin();
        return fade(hover_color(c.accentr, hovered), c.background, 0.5 + 0.5 * pulse);
    }
    fade(hover_color(c.timer, hovered), c.background, if started > 0 { 1.0 } else { DIM })
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

// Draws into the panel pixmap. Text, icons and bars are positioned by the top of their row and centered in it.
struct Painter<'a> {
    pm: &'a mut Pixmap,
    fs: &'a mut FontSystem,
    sc: &'a mut SwashCache,
    family: &'a str,
    cw: f32,
    fg: [u8; 3],
    dim: [u8; 3],
    off: [u8; 3],
    track: [u8; 3],
}

impl Painter<'_> {
    fn fill(&mut self, x: f32, y: f32, w: f32, h: f32, c: [u8; 3]) {
        let (pw, ph) = (self.pm.width(), self.pm.height());
        fill_rect(self.pm.data_mut(), pw, ph, x as u32, y as u32, w as u32, h as u32, c);
    }

    fn text(&mut self, s: &str, x: f32, row_y: f32, size: f32, weight: Weight, c: [u8; 3]) {
        let y = row_y + (ROW_H - size * LINE_HEIGHT) / 2.0;
        render_text(self.pm, self.fs, self.sc, s, x, y, size, 1000.0, ROW_H, c, self.family, weight);
    }

    fn icon(&mut self, mask: &[u8], x: f32, row_y: f32, c: [u8; 3]) {
        let n = (mask.len() as f32).sqrt() as i32;
        let (pw, ph) = (self.pm.width() as i32, self.pm.height() as i32);
        let y = (row_y + (ROW_H - n as f32) / 2.0).round() as i32;
        blit_mask(self.pm.data_mut(), pw, ph, x.round() as i32, y, n, n, mask, &c);
    }

    // Round-capped track with the filled part on top
    fn bar(&mut self, r: &Rect, frac: f32, c: [u8; 3]) {
        let y = r.y + r.h / 2.0;
        let (x0, x1) = (r.x + BAR_T / 2.0, r.x + r.w - BAR_T / 2.0);
        self.stroke(x0, x1, y, self.track);
        if frac > 0.0 { self.stroke(x0, x0 + frac.min(1.0) * (x1 - x0), y, c); }
    }

    fn stroke(&mut self, x0: f32, x1: f32, y: f32, c: [u8; 3]) {
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y);
        pb.line_to(x1, y);
        let mut paint = Paint::default();
        paint.set_color_rgba8(c[0], c[1], c[2], 255);
        let stroke = Stroke { width: BAR_T, line_cap: LineCap::Round, ..Stroke::default() };
        self.pm.stroke_path(&pb.finish().unwrap(), &paint, &stroke, Transform::identity(), None);
    }

    // A level or timer row: icon and label, bar, then the value and state word flush right
    fn row(&mut self, r: &Rect, icon: &[u8], label: &str, label_c: [u8; 3], frac: f32, bar_c: [u8; 3], value: &str, value_c: [u8; 3], state: &str) {
        self.icon(icon, r.x, r.y, label_c);
        self.text(label, r.x + ICON + ICON_GAP, r.y, TEXT, Weight::NORMAL, label_c);
        self.bar(&r.bar(), frac, bar_c);
        let right = r.x + r.w;
        let state_w = state.chars().count() as f32 * self.cw;
        let value_x = right - state_w - if state.is_empty() { 0.0 } else { GAP } - value.chars().count() as f32 * self.cw;
        self.text(value, value_x, r.y, TEXT, Weight::NORMAL, value_c);
        self.text(state, right - state_w, r.y, TEXT, Weight::NORMAL, self.dim);
    }

    // Off toggles are dimmed (their icon is the slashed variant)
    fn toggle(&mut self, r: &Rect, icon: &[u8], label: &str, on: bool, hovered: bool) {
        let (icon_c, label_c) = match (on, hovered) {
            (true, false) => (self.fg, self.dim),
            (true, true) => (self.fg, self.fg),
            (false, h) => (hover_color(self.off, h), hover_color(self.off, h)),
        };
        self.icon(icon, r.x, r.y, icon_c);
        self.text(label, r.x + ICON + ICON_GAP, r.y, TEXT, Weight::NORMAL, label_c);
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
                if let SwashContent::Mask = image.content {
                    blit_mask(pixmap.data_mut(), pw, ph, x0, y0, w, h, &image.data, &color);
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

// --- Icons ---
// Drawn on a 24-unit grid in the style of Font Awesome Solid (weather in the outline style of FA Regular).
// Class "s" is a 2-unit round stroke. A `_HOLES` string is cut out of the icon above it.

const ICON_VOL: &str = r#"<path d="M3 9v6h4l5 4V5L7 9H3z"/><path class="s" d="M15.5 9.5a3.5 3.5 0 0 1 0 5M18 7a7 7 0 0 1 0 10"/>"#;
const ICON_BRI: &str = r#"<circle cx="12" cy="12" r="4.5"/><path class="s" d="M12 2.5V5M12 19v2.5M2.5 12H5M19 12h2.5M5.3 5.3l1.8 1.8M16.9 16.9l1.8 1.8M5.3 18.7l1.8-1.8M16.9 7.1l1.8-1.8"/>"#;
const ICON_BAT: &str = r#"<rect class="s" x="2" y="7" width="18" height="10" rx="2"/><rect x="21" y="10" width="2" height="4" rx=".5"/>"#;
const ICON_BOLT: &str = r#"<path d="M12.6 8.3 9.3 12.6h2.3l-.9 3.1 3.3-4.4h-2.3z"/>"#;
const ICON_TIMER: &str = r#"<circle cx="12" cy="13.5" r="7.5"/><rect x="9.5" y="2" width="5" height="2.5" rx="1"/><rect x="11" y="4" width="2" height="2.5"/>"#;
const ICON_TIMER_HOLES: &str = r#"<path class="s" d="M12 13.5V9.5"/>"#;
const ICON_CAL: &str = r#"<path d="M3 8a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2v2H3z"/><rect class="s" x="3" y="6" width="18" height="15" rx="2"/><path class="s" d="M8 3.5V7M16 3.5V7"/>"#;
const ICON_BELL: &str = r#"<path d="M12 2.5a1.5 1.5 0 0 1 1.5 1.5v.7a5.5 5.5 0 0 1 4 5.3V15l1.7 2a1 1 0 0 1-.8 1.6H5.6a1 1 0 0 1-.8-1.6L6.5 15v-5a5.5 5.5 0 0 1 4-5.3v-.7A1.5 1.5 0 0 1 12 2.5z"/><path d="M9.8 20a2.2 2.2 0 0 0 4.4 0z"/>"#;
const ICON_ALARM: &str = r#"<circle cx="12" cy="13" r="7.5"/><path class="s" d="M4 6.5l3-2.8M20 6.5l-3-2.8M5.5 21l2-2.2M18.5 21l-2-2.2"/>"#;
const ICON_ALARM_HOLES: &str = r#"<path class="s" d="M12 9.5V13l2.5 1.8"/>"#;
const ICON_PHONES: &str = r#"<path class="s" style="stroke-width:2.4" d="M4 15.5V12a8 8 0 0 1 16 0v3.5"/><rect x="2.5" y="14" width="4.5" height="7" rx="1.5"/><rect x="17" y="14" width="4.5" height="7" rx="1.5"/>"#;
const ICON_SPEAKER: &str = r#"<rect x="5" y="2" width="14" height="20" rx="2"/><circle cx="12" cy="15" r="1.4"/>"#;
const ICON_SPEAKER_HOLES: &str = r#"<circle class="s" style="stroke-width:2.2" cx="12" cy="15" r="2.5"/><circle cx="12" cy="7" r="1.5"/>"#;
const ICON_MOON: &str = r#"<path d="M12 3a6.5 6.5 0 0 0 9 9 9 9 0 1 1-9-9z"/>"#;
const SLASH_GAP: &str = r#"<path class="s" style="stroke-width:4.5" d="M3 3l18 18"/>"#;
const SLASH: &str = r#"<path class="s" d="M3 3l18 18"/>"#;
// Indexed by weather_kind: clear day, clear night, cloudy, fog, rain, snow, storm
const WEATHER_ICONS: [&str; 7] = [
    r#"<circle class="s" cx="12" cy="12" r="4"/><path class="s" d="M12 2.5v2M12 19.5v2M2.5 12h2M19.5 12h2M5.3 5.3l1.4 1.4M17.3 17.3l1.4 1.4M5.3 18.7l1.4-1.4M17.3 6.7l1.4-1.4"/>"#,
    r#"<path class="s" d="M12 3a6.5 6.5 0 0 0 9 9 9 9 0 1 1-9-9z"/>"#,
    r#"<path class="s" d="M6.5 19a4.5 4.5 0 0 1-.4-8.98 6 6 0 0 1 11.6-1.02A5 5 0 0 1 17.5 19z"/>"#,
    r#"<path class="s" d="M3 6h14M7 12h14M3 18h14"/>"#,
    r#"<g transform="translate(2.4 -1.5) scale(.8)"><path class="s" style="stroke-width:2.5" d="M6.5 19a4.5 4.5 0 0 1-.4-8.98 6 6 0 0 1 11.6-1.02A5 5 0 0 1 17.5 19z"/></g><path class="s" d="M8 17.5l-1.2 3.5M12 17.5l-1.2 3.5M16 17.5l-1.2 3.5"/>"#,
    r#"<path class="s" d="M12 3v18M4.2 7.5l15.6 9M4.2 16.5l15.6-9"/>"#,
    r#"<path class="s" d="M13 2.5 5 13.5h6.5l-1 8 8-11h-6.5z"/>"#,
];

// Alpha masks, `n` px square, rasterized once at startup
struct Icons {
    n: u32,
    vol: Vec<u8>,
    vol_off: Vec<u8>,
    bri: Vec<u8>,
    bat: Vec<u8>,
    bolt: Vec<u8>,
    timer: Vec<u8>,
    cal: Vec<u8>,
    bell: Vec<u8>,
    bell_off: Vec<u8>,
    alarm: Vec<u8>,
    alarm_off: Vec<u8>,
    phones: Vec<u8>,
    speaker: Vec<u8>,
    moon: Vec<u8>,
    weather: Vec<Vec<u8>>,
}

impl Icons {
    fn new(n: u32) -> Self {
        let r = |body: &str| raster(body, n);
        let (gap, line) = (r(SLASH_GAP), r(SLASH));
        let slashed = |m: &[u8]| -> Vec<u8> { cut(m, &gap).iter().zip(&line).map(|(&a, &b)| a.max(b)).collect() };
        let vol = r(ICON_VOL);
        let bell = r(ICON_BELL);
        let alarm = cut(&r(ICON_ALARM), &r(ICON_ALARM_HOLES));
        Icons {
            n,
            vol_off: slashed(&vol),
            vol,
            bri: r(ICON_BRI),
            bat: r(ICON_BAT),
            bolt: r(ICON_BOLT),
            timer: cut(&r(ICON_TIMER), &r(ICON_TIMER_HOLES)),
            cal: r(ICON_CAL),
            bell_off: slashed(&bell),
            bell,
            alarm_off: slashed(&alarm),
            alarm,
            phones: r(ICON_PHONES),
            speaker: cut(&r(ICON_SPEAKER), &r(ICON_SPEAKER_HOLES)),
            moon: r(ICON_MOON),
            weather: WEATHER_ICONS.iter().map(|b| r(b)).collect(),
        }
    }
}

fn raster(body: &str, n: u32) -> Vec<u8> {
    let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><style>.s{{fill:none;stroke:#000;stroke-width:2;stroke-linecap:round;stroke-linejoin:round}}</style>{body}</svg>"#);
    let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default()).unwrap();
    let mut pm = resvg::tiny_skia::Pixmap::new(n, n).unwrap();
    let k = n as f32 / 24.0;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(k, k), &mut pm.as_mut());
    pm.pixels().iter().map(|p| p.alpha()).collect()
}

// `a` with `b`'s coverage removed
fn cut(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b).map(|(&a, &b)| (a as u32 * (255 - b as u32) / 255) as u8).collect()
}

// The charge level inside the battery outline. While charging the bolt is inverted against it,
// so it shows as a hole in the fill and as solid where the battery is empty.
fn battery_fill(n: u32, frac: f32, bolt: Option<&[u8]>) -> Vec<u8> {
    let k = n as f32 / 24.0;
    let (x0, x1, y0, y1) = (4.5 * k, (4.5 + 13.0 * frac.clamp(0.0, 1.0)) * k, 9.5 * k, 14.5 * k);
    let cover = |a: f32, b: f32, p: f32| (b.min(p + 1.0) - a.max(p)).clamp(0.0, 1.0);
    (0..n * n).map(|i| {
        let (px, py) = ((i % n) as f32, (i / n) as f32);
        let fill = (cover(x0, x1, px) * cover(y0, y1, py) * 255.0) as i32;
        bolt.map_or(fill, |b| (fill - b[i as usize] as i32).abs()) as u8
    }).collect()
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
            let (x, y) = (event.position.0 as f32, event.position.1 as f32);
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    let device = self.cursor_shape_manager.get_shape_device(pointer, qh);
                    device.set_shape(serial, Shape::Default);
                    device.destroy();
                }
                PointerEventKind::Press { button: 0x110, .. } => self.handle_click(x, y),
                PointerEventKind::Press { button: 0x111, .. } => self.handle_right_click(x, y),
                PointerEventKind::Press { button: 0x112, .. } => self.handle_middle_click(x, y),
                PointerEventKind::Release { button: 0x110, .. } => self.end_drag(),
                PointerEventKind::Motion { .. } => {
                    if self.drag.is_some() {
                        self.drag_to(x);
                    } else {
                        let new_hover = self.hover_at(x, y);
                        if new_hover != self.hover {
                            self.hover = new_hover;
                            self.draw();
                        }
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.hover != Hover::None {
                        self.hover = Hover::None;
                        self.draw();
                    }
                }
                PointerEventKind::Axis { ref vertical, .. } => {
                    if vertical.absolute != 0.0 {
                        self.handle_scroll(x, y, vertical.absolute);
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
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--alarm") {
        run_alarm(args[2].parse().unwrap(), args[3].parse().unwrap(), &load_config());
        return;
    }

    // Install SIGUSR2 handler immediately — must be before any setup so we
    // catch the release signal even if the key is released during startup.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigusr2_handler as *const () as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGUSR2, &sa, std::ptr::null_mut());
    }

    let cfg = load_config();
    let colors = load_colors(cfg.color_file.as_deref());
    let st = load_state(&cfg);
    // Re-arm watchers for timers still running from a previous session (e.g. after a reboot)
    for (n, started, dur) in [(1, st.timer1_started, st.timer1_duration), (2, st.timer2_started, st.timer2_duration)] {
        if started > 0 && timer_remaining(dur, started) > 0 { spawn_alarm(n, started); }
    }
    let weather_fetch = if cfg.weather_lat != 0.0 && now_unix() - st.weather_fetched > cfg.weather_max_age {
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
    let headphones = is_headphones(&cfg.bt_device_1);

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

    // The panel's height depends on which level rows this machine has
    let battery = read_battery();
    let brightness = read_brightness();
    let (width, height) = (WIDTH.round() as u32, panel_height(1 + battery.is_some() as u32 + brightness.is_some() as u32));

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("wavedash"), None);
    layer.set_size(width, height);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.wl_surface().commit();

    let pool = SlotPool::new((width * height * 4) as usize, &shm).unwrap();

    // Regular and bold faces of one family, selected by weight
    let mut db = cosmic_text::fontdb::Database::new();
    db.load_font_data(std::fs::read(expand_path(&cfg.font)).expect("failed to read font file"));
    db.load_font_data(std::fs::read(expand_path(&cfg.font_bold)).expect("failed to read bold font file"));
    let font_family = db.faces().next().expect("font file contains no faces").families[0].0.clone();
    let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);
    let cw = measure_text(&mut font_system, "0000000000", TEXT, &font_family, Weight::NORMAL) / 10.0;
    let clock_cw = measure_text(&mut font_system, "0000000000", CLOCK, &font_family, Weight::BOLD) / 10.0;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        pointer: None,
        cursor_shape_manager,
        pool,
        width,
        height,
        exit: false,
        font_system,
        swash_cache: SwashCache::new(),
        colors,
        font_family,
        icons: Icons::new(ICON.round() as u32),
        cw,
        clock_cw,
        timer1_duration: st.timer1_duration,
        timer1_started: st.timer1_started,
        timer2_duration: st.timer2_duration,
        timer2_started: st.timer2_started,
        alert: st.alert,
        alert_sound: expand_path(&cfg.alert_sound),
        alert_volume: cfg.alert_volume,
        volume,
        muted,
        headphones,
        bt_device_1: cfg.bt_device_1,
        audio_device_2: cfg.audio_device_2,
        is_dim: false,
        hover: Hover::None,
        drag: None,
        volume_dirty: false,
        brightness_dirty: false,
        timer1_base: st.timer1_base,
        timer2_base: st.timer2_base,
        timer1_config: cfg.timer1_duration as i64,
        timer2_config: cfg.timer2_duration as i64,
        volume_set_at: 0,
        audio_refreshed: Instant::now(),
        weather_temp: st.weather_temp,
        weather_feels: st.weather_feels,
        weather_code: st.weather_code,
        weather_is_day: st.weather_is_day,
        weather_fetched: st.weather_fetched,
        weather_fetch,
        battery,
        battery_read_at: now_unix(),
        brightness,
        brightness_set_at: 0,
        notif_paused,
        long_press: None,
        startup: Instant::now(),
    };

    // Redraw tick (~30fps, so the finished-timer pulse is smooth); audio state is polled less often since it shells out
    let timer = Timer::from_duration(std::time::Duration::from_millis(TICK_MS));
    event_loop.handle().insert_source(timer, |_, _, app| {
        // Long press detection: after grace period, decide mode based on whether key was released
        if app.long_press.is_none() && app.startup.elapsed().as_millis() >= LONG_PRESS_GRACE_MS as u128 {
            app.long_press = Some(!GOT_SIGUSR2.load(Ordering::Acquire));
        }
        if app.long_press == Some(true) && GOT_SIGUSR2.load(Ordering::Acquire) {
            app.exit = true;
            return TimeoutAction::Drop;
        }
        if app.volume_dirty {
            set_volume(app.volume);
            app.volume_dirty = false;
            app.volume_set_at = now_unix();
        }
        if app.brightness_dirty {
            set_brightness(app.brightness.unwrap());
            app.brightness_dirty = false;
            app.brightness_set_at = now_unix();
        }
        if app.drag.is_none() && app.audio_refreshed.elapsed().as_millis() >= AUDIO_REFRESH_MS && now_unix() - app.volume_set_at >= AUDIO_REFRESH_COOLDOWN {
            app.refresh_audio();
            app.audio_refreshed = Instant::now();
        }
        if now_unix() - app.battery_read_at >= BATTERY_REFRESH {
            app.battery = read_battery();
            app.battery_read_at = now_unix();
        }
        // Re-read backlight (external hotkeys) unless we just set it
        if app.drag.is_none() && app.brightness.is_some() && now_unix() - app.brightness_set_at >= AUDIO_REFRESH_COOLDOWN {
            app.brightness = read_brightness();
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
