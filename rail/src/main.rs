use std::path::PathBuf;
use libc;
use std::process::{Command, Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping, SwashCache, SwashContent, Weight};
use serde::{Deserialize, Serialize};
use smithay_client_toolkit as sctk;
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
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
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
use tiny_skia::{FillRule, LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

// --- Config ---

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    color_file: Option<String>,
    height: u32,
    output: Option<String>,
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
            color_file: Some("~/.cache/wal/colors-rail.toml".into()),
            height: 32,
            output: None,
            font: "~/.local/share/fonts/GoogleSansCode-Regular.ttf".into(),
            font_bold: "~/.local/share/fonts/GoogleSansCode-Bold.ttf".into(),
            timer1_duration: 3600,
            timer2_duration: 900,
            bt_device_1: "AC:BF:71:08:A1:D6".into(),
            audio_device_2: "EC:81:93:AC:8B:60".into(),
            weather_lat: 0.0,
            weather_lon: 0.0,
            weather_max_age: 3600,
            alert_sound: "~/.config/widgets/rail-alarm.wav".into(),
            alert_volume: 1.0,
        }
    }
}

fn load_config() -> Config {
    let path = config_dir().join("rail.toml");
    match std::fs::read_to_string(&path) {
        Ok(s) => match toml::from_str(&s) {
            Ok(cfg) => cfg,
            Err(e) => { eprintln!("rail: failed to parse {}: {e}", path.display()); Config::default() }
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
    palette: [[u8; 3]; 6],  // color1-6, the dots that open tincture
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
            palette: [[0xf3, 0x8b, 0xa8], [0xa6, 0xe3, 0xa1], [0xf9, 0xe2, 0xaf], [0x89, 0xb4, 0xfa], [0xf5, 0xc2, 0xe7], [0x94, 0xe2, 0xd5]],
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
                            "color1" | "color2" | "color3" | "color4" | "color5" | "color6" => colors.palette[key[5..].parse::<usize>().unwrap() - 1] = c,
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
    base.join("widgets/rail.toml")
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
// A detached `rail --alarm <n> <started>` process watches timer n in the state file and plays
// the alert sound when it reaches zero. Pausing or restarting the timer changes `started`, which
// makes the watcher exit, so nothing has to be cancelled explicitly.

fn play_alert(sound: &std::path::Path, volume: f32) -> Child {
    Command::new("pw-play").args(["--volume", &volume.to_string()]).arg(sound)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
}

fn spawn_alarm(n: u8, started: u64) {
    Command::new("pkill").args(["-f", &format!("rail --alarm {n}")]).status().unwrap();
    Command::new(std::env::current_exe().unwrap()).args(["--alarm", &n.to_string(), &started.to_string()])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
}

fn run_alarm(n: u8, started: u64, cfg: &Config) {
    // Renamed so the toggle script's `pkill -x rail` closes the bar without taking the watcher down
    unsafe { libc::prctl(libc::PR_SET_NAME, b"rail-alarm\0".as_ptr()); }
    let mut player: Option<Child> = None;
    loop {
        let st = load_state(cfg);
        let (t_started, t_dur) = if n == 1 { (st.timer1_started, st.timer1_duration) } else { (st.timer2_started, st.timer2_duration) };
        if t_started != started { break; }
        if let Some(p) = player.as_mut() {
            if p.try_wait().unwrap().is_some() { return; }
        } else if timer_remaining(t_dur, t_started) <= 0 {
            if !st.alert { return; }
            if !Command::new("pgrep").args(["-x", "rail"]).status().unwrap().success() {
                Command::new(std::env::current_exe().unwrap()).arg("--pin").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
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
// Sizes are design px for a bar DESIGN_H tall; `Sz` scales them to the configured height.

const DESIGN_H: f32 = 28.0;
const LINE_HEIGHT: f32 = 1.2;

// Opacities, blended against the background
const DIM: f32 = 0.6;     // secondary text, stopped timers
const OFF: f32 = 0.32;    // off toggles, muted volume
const TRACK: f32 = 0.14;  // empty part of a meter
const RULE: f32 = 0.18;   // group separators

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
const FAST_TICK_MS: u64 = 33;
const AUDIO_REFRESH_MS: u128 = 2000;
const AUDIO_REFRESH_COOLDOWN: u64 = 1;
const LONG_PRESS_GRACE_MS: u128 = 300;

// Pixel sizes: the design sizes times height / DESIGN_H
#[derive(Clone, Copy)]
struct Sz { h: f32, text: f32, clock: f32, icon: f32, meter_w: f32, meter_t: f32, gap: f32, pad: f32, icon_gap: f32, rule_inset: f32, rule_gap: f32, rule_w: f32, dot: f32, dot_gap: f32 }

impl Sz {
    fn new(height: u32) -> Self {
        let s = height as f32 / DESIGN_H;
        Sz {
            h: height as f32, text: 14.0 * s, clock: 16.0 * s, icon: (17.0 * s).round(), meter_w: 90.0 * s, meter_t: 5.0 * s,
            gap: 20.0 * s, pad: 16.0 * s, icon_gap: 7.0 * s, rule_inset: 6.0 * s, rule_gap: 14.0 * s, rule_w: s.round().max(1.0),
            dot: 7.0 * s, dot_gap: 3.0 * s,
        }
    }
}

// --- Geometry ---

#[derive(Clone, Copy, Default)]
struct Rect { x: f32, y: f32, w: f32, h: f32 }

impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum Item { Time, Date, Weather, Output, Volume, Brightness, Battery, Alarm, Timer1, Timer2, Notif, Dim, Palette }

// One item of the bar. `meter` is its draggable meter, zero-size if it has none.
#[derive(Clone, Copy)]
struct Seg { item: Item, r: Rect, meter: Rect }

struct Layout { segs: Vec<Seg>, rules: Vec<f32>, end: f32 }

impl Layout {
    // Hit areas reach halfway into the gap on each side, so small icons are easy to hit
    fn at(&self, x: f32, y: f32, slop: f32) -> Option<Seg> {
        self.segs.iter().find(|s| Rect { x: s.r.x - slop, w: s.r.w + 2.0 * slop, ..s.r }.contains(x, y)).copied()
    }
    fn get(&self, item: Item) -> Seg {
        *self.segs.iter().find(|s| s.item == item).unwrap()
    }
}

// --- App ---

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: Option<LayerSurface>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_shape_manager: CursorShapeManager,
    pool: SlotPool,
    width: u32,
    height: u32,
    sz: Sz,
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
    hover: Option<Item>,
    drag: Option<Item>,
    // Scroll and drag only update these; the tick applies them, at most once per event batch
    volume_dirty: bool,
    brightness_dirty: bool,
    // Base durations for reset (scroll-adjusted)
    timer1_base: i64,
    timer2_base: i64,
    // Config-defined durations (middle-click reset target, and the scale of the timer meters)
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
    // Long press: None = undecided (grace period), Some(true) = peek (exit on key release), Some(false) = persistent
    long_press: Option<bool>,
    startup: Instant,
    // frame_key() of the last drawn frame
    shown: String,
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

    fn layer(&self) -> &LayerSurface {
        self.layer.as_ref().unwrap()
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

    // (remaining, started, config duration) of a timer item
    fn timer(&self, item: Item) -> (i64, u64, i64) {
        if item == Item::Timer1 { (timer_remaining(self.timer1_duration, self.timer1_started), self.timer1_started, self.timer1_config) }
        else { (timer_remaining(self.timer2_duration, self.timer2_started), self.timer2_started, self.timer2_config) }
    }

    // A timer that ran out and hasn't been clicked yet; it pulses, so the bar redraws at the fast tick
    fn finished(&self) -> bool {
        [Item::Timer1, Item::Timer2].into_iter().any(|t| { let (rem, started, _) = self.timer(t); started > 0 && rem <= 0 })
    }

    // ("54° clear", "feels 51°"), or None before the first fetch
    fn weather_text(&self) -> Option<(String, String)> {
        if self.weather_fetched == 0 { return None; }
        let word = weather_kind(self.weather_code, self.weather_is_day).1;
        Some((format!("{:.0}° {word}", self.weather_temp), format!("feels {:.0}°", self.weather_feels)))
    }

    // Everything the bar shows apart from hover and the pulse; the tick redraws when it changes
    fn frame_key(&self) -> String {
        format!("{} {} {:?} {:?} {} {} {} {:?} {:?} {:?} {} {} {}", clock_text(), format_date(), self.timer(Item::Timer1), self.timer(Item::Timer2),
            self.volume, self.muted, self.headphones, self.brightness, self.battery, self.weather_text(), self.alert, self.notif_paused, self.width)
    }

    // Left block: time | date | weather | audio and levels. Right block: | alarm and timers | toggles | palette
    fn groups(&self) -> (Vec<Vec<Item>>, Vec<Vec<Item>>) {
        let mut left = vec![vec![Item::Time], vec![Item::Date]];
        if self.weather_fetched > 0 { left.push(vec![Item::Weather]); }
        let mut audio = vec![Item::Output, Item::Volume];
        if self.brightness.is_some() { audio.push(Item::Brightness); }
        if self.battery.is_some() { audio.push(Item::Battery); }
        left.push(audio);
        let toggles = if self.brightness.is_none() { vec![Item::Notif, Item::Dim] } else { vec![Item::Notif] };
        (left, vec![vec![Item::Alarm, Item::Timer1, Item::Timer2], toggles, vec![Item::Palette]])
    }

    fn item_w(&self, item: Item) -> f32 {
        let z = self.sz;
        let text = |n: usize| n as f32 * self.cw;
        let metered = |n: usize| z.icon + 2.0 * z.icon_gap + z.meter_w + text(n);
        match item {
            Item::Time => clock_text().chars().count() as f32 * self.clock_cw,
            Item::Date => z.icon + z.icon_gap + text(format_date().chars().count()),
            Item::Weather => { let (a, b) = self.weather_text().unwrap(); z.icon + z.icon_gap + text(a.chars().count() + 1 + b.chars().count()) }
            Item::Volume | Item::Brightness => metered(4),
            Item::Timer1 | Item::Timer2 => metered(5),
            Item::Battery => z.icon + z.icon_gap + text(4),
            Item::Output | Item::Alarm | Item::Notif | Item::Dim => z.icon,
            Item::Palette => 6.0 * z.dot + 5.0 * z.dot_gap,
        }
    }

    // Places groups left to right from x with a rule between groups (and before the first if `lead_rule`); returns the end x
    fn place(&self, lay: &mut Layout, groups: &[Vec<Item>], mut x: f32, lead_rule: bool) -> f32 {
        let z = self.sz;
        for (i, group) in groups.iter().enumerate() {
            if i > 0 || lead_rule {
                if i > 0 { x += z.gap + z.rule_gap; }
                lay.rules.push(x);
                x += z.rule_w + z.rule_gap + z.gap;
            }
            for (j, &item) in group.iter().enumerate() {
                if j > 0 { x += z.gap; }
                let r = Rect { x, y: 0.0, w: self.item_w(item), h: z.h };
                let metered = matches!(item, Item::Volume | Item::Brightness | Item::Timer1 | Item::Timer2);
                let meter = if metered { Rect { x: x + z.icon + z.icon_gap, w: z.meter_w, ..r } } else { Rect::default() };
                lay.segs.push(Seg { item, r, meter });
                x += r.w;
            }
        }
        x
    }

    // The left block from the left edge, the right block flush against the right edge
    fn layout(&self) -> Layout {
        let (left, right) = self.groups();
        let mut lay = Layout { segs: vec![], rules: vec![], end: 0.0 };
        lay.end = self.place(&mut lay, &left, self.sz.pad, false);
        let mut tail = Layout { segs: vec![], rules: vec![], end: 0.0 };
        let dx = self.width as f32 - self.sz.pad - self.place(&mut tail, &right, 0.0, true);
        let shift = |r: Rect| if r.w > 0.0 { Rect { x: r.x + dx, ..r } } else { r };
        lay.segs.extend(tail.segs.iter().map(|s| Seg { r: shift(s.r), meter: shift(s.meter), ..*s }));
        lay.rules.extend(tail.rules.iter().map(|x| x + dx));
        lay.end += self.width as f32 - dx; // total content width, for the overflow check
        lay
    }

    fn item_at(&self, x: f32, y: f32) -> Option<Item> {
        self.layout().at(x, y, self.sz.gap / 2.0).map(|s| s.item)
    }

    fn draw(&mut self) {
        let lay = self.layout();
        self.shown = self.frame_key();
        let c = self.colors;
        let z = self.sz;
        let hv = self.hover;
        let (w, h) = (self.width, self.height);
        let pulse_t = self.startup.elapsed().as_secs_f32();
        let weather = self.weather_text();
        let timers = [self.timer(Item::Timer1), self.timer(Item::Timer2)];

        let (wl_buf, canvas) = self.pool
            .create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888)
            .unwrap();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        fill_rect_alpha(pixmap.data_mut(), w, h, 0, 0, w, h, c.background, c.background_alpha);

        let fg = c.foreground;
        let (dim, off) = (fade(fg, c.background, DIM), fade(fg, c.background, OFF));
        let ic = &self.icons;
        let mut p = Painter {
            pm: &mut pixmap, fs: &mut self.font_system, sc: &mut self.swash_cache, family: &self.font_family,
            h: z.h, meter_t: z.meter_t, track: fade(fg, c.background, TRACK),
        };

        for x in &lay.rules { p.fill(x.round(), z.rule_inset, z.rule_w, z.h - 2.0 * z.rule_inset, fade(c.divider, c.background, RULE)); }

        for s in &lay.segs {
            let lit = |col: [u8; 3]| hover_color(col, hv == Some(s.item));
            let toggle = |on: bool| lit(if on { fg } else { off });
            let x = s.r.x;
            let after_icon = x + z.icon + z.icon_gap;
            let after_meter = s.meter.x + s.meter.w + z.icon_gap;
            match s.item {
                Item::Time => p.text(&clock_text(), x, z.clock, Weight::BOLD, c.clock),
                Item::Date => {
                    p.icon(&ic.cal, x, lit(fg));
                    p.text(&format_date(), after_icon, z.text, Weight::NORMAL, lit(fg));
                }
                Item::Weather => {
                    let (now, feels) = weather.as_ref().unwrap();
                    p.icon(&ic.weather[weather_kind(self.weather_code, self.weather_is_day).0], x, c.weather);
                    p.text(now, after_icon, z.text, Weight::NORMAL, c.weather);
                    p.text(feels, after_icon + (now.chars().count() + 1) as f32 * self.cw, z.text, Weight::NORMAL, dim);
                }
                Item::Output => p.icon(if self.headphones { &ic.phones } else { &ic.speaker }, x, lit(fg)),
                Item::Volume => {
                    let (icon, col, bar) = if self.muted { (&ic.vol_off, off, fade(c.volume, c.background, OFF)) } else { (&ic.vol, fg, c.volume) };
                    p.icon(icon, x, lit(col));
                    p.meter(&s.meter, self.volume / VOL_MAX, bar);
                    p.text(&format!("{:.0}%", self.volume * 100.0), after_meter, z.text, Weight::NORMAL, lit(col));
                }
                Item::Brightness => {
                    let b = self.brightness.unwrap();
                    p.icon(&ic.bri, x, lit(fg));
                    p.meter(&s.meter, b, c.brightness);
                    p.text(&format!("{:.0}%", b * 100.0), after_meter, z.text, Weight::NORMAL, lit(fg));
                }
                Item::Battery => {
                    let (pct, plugged) = self.battery.unwrap();
                    let low = !plugged && pct <= BATTERY_LOW;
                    let fill_c = if plugged { c.battery_charging } else if low { c.battery_low } else { c.battery };
                    p.icon(&ic.bat, x, fg);
                    p.icon(&battery_fill(ic.n, pct, plugged.then_some(&ic.bolt[..])), x, fill_c);
                    p.text(&format!("{:.0}%", pct * 100.0), after_icon, z.text, Weight::NORMAL, fg);
                }
                Item::Alarm => p.icon(if self.alert { &ic.alarm } else { &ic.alarm_off }, x, toggle(self.alert)),
                // Running: full color. Stopped: dimmed. Finished: the time pulses until clicked.
                Item::Timer1 | Item::Timer2 => {
                    let (rem, started, config) = timers[(s.item == Item::Timer2) as usize];
                    let col = lit(if started > 0 { fg } else { dim });
                    let text_c = if started > 0 && rem <= 0 { pulse(&c, pulse_t) } else { col };
                    p.icon(&ic.timer, x, col);
                    p.meter(&s.meter, rem as f32 / config as f32, if started > 0 { c.timer } else { fade(c.timer, c.background, DIM) });
                    p.text(&format_timer(rem), after_meter, z.text, Weight::NORMAL, lit(text_c));
                }
                Item::Notif => p.icon(if self.notif_paused { &ic.bell_off } else { &ic.bell }, x, toggle(!self.notif_paused)),
                Item::Dim => p.icon(if self.weather_is_day { &ic.bri } else { &ic.moon }, x, lit(fg)),
                Item::Palette => for (i, &col) in c.palette.iter().enumerate() {
                    p.dot(x + i as f32 * (z.dot + z.dot_gap) + z.dot / 2.0, z.dot / 2.0, lit(col));
                }
            }
        }

        // Copy RGBA premul -> BGRA (ARGB8888 on LE)
        for (dst, src) in canvas.chunks_exact_mut(4).zip(pixmap.data().chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }

        let surface = self.layer().wl_surface();
        wl_buf.attach_to(surface).unwrap();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
    }

    fn handle_click(&mut self, x: f32, y: f32) {
        let Some(seg) = self.layout().at(x, y, self.sz.gap / 2.0) else { return };
        // A press on a meter starts a drag; the rest of the item is a button
        if seg.meter.contains(x, y) {
            self.drag = Some(seg.item);
            self.drag_to(x);
            return;
        }
        match seg.item {
            Item::Volume => {
                toggle_mute();
                self.refresh_audio();
            }
            Item::Timer1 => self.click_timer(1),
            Item::Timer2 => self.click_timer(2),
            Item::Date => {
                Command::new("xdg-open").arg("https://calendar.google.com").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
            }
            Item::Notif => {
                Command::new("dunstctl").arg("set-paused").arg("toggle").spawn().ok();
                self.notif_paused = !self.notif_paused;
            }
            Item::Alarm => {
                self.alert = !self.alert;
                save_state(&self.state());
                if self.alert {
                    // Preview the sound so output device and volume can be checked
                    play_alert(&self.alert_sound, self.alert_volume);
                } else {
                    Command::new("pkill").args(["-f", &format!("pw-play .*{}", self.alert_sound.display())]).status().unwrap();
                }
            }
            Item::Output => {
                self.refresh_audio();
                let target = if self.headphones { &self.audio_device_2 } else { &self.bt_device_1 };
                switch_audio(target);
                self.refresh_audio();
            }
            Item::Dim => {
                let arg = if self.is_dim { "1" } else { "0" };
                Command::new("sh").arg("-c").arg(format!("{}/wgmn/scripts/dim_toggle.sh {arg}", home().display())).spawn().ok();
                self.is_dim = !self.is_dim;
            }
            // Tincture with no argument edits the current wallpaper's scheme
            Item::Palette => {
                Command::new("tincture").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            }
            Item::Time | Item::Weather | Item::Brightness | Item::Battery => {}
        }
        self.draw();
    }

    // Set the dragged value from the pointer's x; timers snap to whole minutes
    fn drag_to(&mut self, x: f32) {
        let Some(drag) = self.drag else { return };
        let bar = self.layout().get(drag).meter;
        let t = self.sz.meter_t;
        let frac = ((x - bar.x - t / 2.0) / (bar.w - t)).clamp(0.0, 1.0);
        match drag {
            Item::Volume => {
                self.volume = frac * VOL_MAX;
                self.volume_dirty = true;
            }
            Item::Brightness => {
                self.brightness = Some(frac.max(BRIGHTNESS_MIN));
                self.brightness_dirty = true;
            }
            _ => {
                let (dur, started, base, config) = if drag == Item::Timer1 { (&mut self.timer1_duration, self.timer1_started, &mut self.timer1_base, self.timer1_config) } else { (&mut self.timer2_duration, self.timer2_started, &mut self.timer2_base, self.timer2_config) };
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
        if matches!(self.drag, Some(Item::Timer1 | Item::Timer2)) { save_state(&self.state()); }
        self.drag = None;
    }

    fn handle_scroll(&mut self, x: f32, y: f32, dy: f64) {
        let sign = if dy > 0.0 { -1.0 } else { 1.0 };
        match self.item_at(x, y) {
            Some(Item::Volume) => {
                self.volume = (self.volume + sign * VOL_SCROLL_STEP).clamp(0.0, VOL_MAX);
                self.volume_dirty = true;
            }
            Some(Item::Brightness) => {
                let Some(b) = self.brightness else { return };
                self.brightness = Some((b + sign * BRIGHTNESS_SCROLL_STEP).clamp(BRIGHTNESS_MIN, 1.0));
                self.brightness_dirty = true;
            }
            Some(which @ (Item::Timer1 | Item::Timer2)) => {
                let (dur, base) = if which == Item::Timer1 { (&mut self.timer1_duration, &mut self.timer1_base) } else { (&mut self.timer2_duration, &mut self.timer2_base) };
                *dur = (*dur + sign as i64 * TIMER_SCROLL_STEP).max(TIMER_SCROLL_STEP);
                *base = *dur;
                save_state(&self.state());
            }
            _ => return,
        }
        self.draw();
    }

    fn handle_middle_click(&mut self, x: f32, y: f32) {
        match self.item_at(x, y) {
            Some(Item::Volume | Item::Output) => {
                Command::new("pavucontrol").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
            }
            Some(Item::Timer1) => self.reset_timer(1, true),
            Some(Item::Timer2) => self.reset_timer(2, true),
            _ => return,
        }
        self.draw();
    }

    fn handle_right_click(&mut self, x: f32, y: f32) {
        match self.item_at(x, y) {
            Some(Item::Timer1) => self.reset_timer(1, false),
            Some(Item::Timer2) => self.reset_timer(2, false),
            _ => return,
        }
        self.draw();
    }

    // Runs after every event batch and on the timeout from next_timeout()
    fn tick(&mut self) {
        // Long press: after the grace period, a key still held means peek (exit on release); a quick tap pins the bar
        if self.long_press.is_none() && self.startup.elapsed().as_millis() >= LONG_PRESS_GRACE_MS {
            self.long_press = Some(!GOT_SIGUSR2.load(Ordering::Acquire));
            if self.long_press == Some(false) { self.reserve(); }
        }
        if self.long_press == Some(true) && GOT_SIGUSR2.load(Ordering::Acquire) {
            self.exit = true;
            return;
        }
        if self.volume_dirty {
            set_volume(self.volume);
            self.volume_dirty = false;
            self.volume_set_at = now_unix();
        }
        if self.brightness_dirty {
            set_brightness(self.brightness.unwrap());
            self.brightness_dirty = false;
            self.brightness_set_at = now_unix();
        }
        // Volume and backlight can change from outside (hotkeys); poll them unless we just set them
        if self.drag.is_none() && self.audio_refreshed.elapsed().as_millis() >= AUDIO_REFRESH_MS {
            if now_unix() - self.volume_set_at >= AUDIO_REFRESH_COOLDOWN { self.refresh_audio(); }
            if self.brightness.is_some() && now_unix() - self.brightness_set_at >= AUDIO_REFRESH_COOLDOWN { self.brightness = read_brightness(); }
            self.audio_refreshed = Instant::now();
        }
        if now_unix() - self.battery_read_at >= BATTERY_REFRESH {
            self.battery = read_battery();
            self.battery_read_at = now_unix();
        }
        if self.weather_fetch.as_mut().is_some_and(|child| child.try_wait().unwrap().is_some()) {
            self.read_weather();
        }
        if self.width > 0 && (self.finished() || self.frame_key() != self.shown) { self.draw(); }
    }

    // Fast while something animates or waits on input; otherwise at the next whole second, when the timers tick
    fn next_timeout(&self) -> std::time::Duration {
        if self.drag.is_some() || self.long_press != Some(false) || self.finished() || self.weather_fetch.is_some() {
            return std::time::Duration::from_millis(FAST_TICK_MS);
        }
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_millis() as u64;
        std::time::Duration::from_millis(1000 - ms + 5)
    }

    // Pinned: push windows down by the bar's height
    fn reserve(&self) {
        self.layer().set_exclusive_zone(self.height as i32);
        self.layer().commit();
    }

    fn read_weather(&mut self) {
        let output = self.weather_fetch.take().unwrap().wait_with_output().unwrap();
        if !output.status.success() { return; }
        let text = String::from_utf8_lossy(&output.stdout);
        // Scope to "current":{ to skip "current_units"
        let Some(ci) = text.find("\"current\":{") else { return };
        let s = &text[ci..];
        let num_at = |needle: &str| -> Option<f64> {
            let after = s[s.find(needle)? + needle.len()..].trim_start();
            after[..after.find(|c: char| c == ',' || c == '}')?].trim().parse().ok()
        };
        let (Some(temp), Some(feels), Some(code), Some(is_day)) = (num_at("\"temperature_2m\":"), num_at("\"apparent_temperature\":"), num_at("\"weather_code\":"), num_at("\"is_day\":")) else { return };
        self.weather_temp = temp;
        self.weather_feels = feels;
        self.weather_code = code as u32;
        self.weather_is_day = is_day as u32 == 1;
        self.weather_fetched = now_unix();
        save_state(&self.state());
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

// "Sunday, November 1"
fn format_date() -> String {
    let secs = now_unix();
    let t = secs as i64;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&t as *const i64, &mut tm) };
    let days = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let months = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
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

// `t` is seconds since launch; a finished timer's time breathes in the accent color until clicked
fn pulse(c: &Colors, t: f32) -> [u8; 3] {
    fade(c.accentr, c.background, 0.75 + 0.25 * (t * std::f32::consts::PI).sin())
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

// Draws into the bar pixmap. Text, icons, meters and dots are centered vertically in the bar.
struct Painter<'a> {
    pm: &'a mut Pixmap,
    fs: &'a mut FontSystem,
    sc: &'a mut SwashCache,
    family: &'a str,
    h: f32,
    meter_t: f32,
    track: [u8; 3],
}

impl Painter<'_> {
    fn fill(&mut self, x: f32, y: f32, w: f32, h: f32, c: [u8; 3]) {
        let (pw, ph) = (self.pm.width(), self.pm.height());
        fill_rect(self.pm.data_mut(), pw, ph, x as u32, y as u32, w as u32, h as u32, c);
    }

    fn text(&mut self, s: &str, x: f32, size: f32, weight: Weight, c: [u8; 3]) {
        let y = (self.h - size * LINE_HEIGHT) / 2.0;
        render_text(self.pm, self.fs, self.sc, s, x, y, size, 10000.0, self.h, c, self.family, weight);
    }

    fn icon(&mut self, mask: &[u8], x: f32, c: [u8; 3]) {
        let n = (mask.len() as f32).sqrt() as i32;
        let (pw, ph) = (self.pm.width() as i32, self.pm.height() as i32);
        let y = ((self.h - n as f32) / 2.0).round() as i32;
        blit_mask(self.pm.data_mut(), pw, ph, x.round() as i32, y, n, n, mask, &c);
    }

    // Round-capped track with the filled part on top
    fn meter(&mut self, r: &Rect, frac: f32, c: [u8; 3]) {
        let y = self.h / 2.0;
        let (x0, x1) = (r.x + self.meter_t / 2.0, r.x + r.w - self.meter_t / 2.0);
        self.stroke(x0, x1, y, self.track);
        if frac > 0.0 { self.stroke(x0, x0 + frac.min(1.0) * (x1 - x0), y, c); }
    }

    fn stroke(&mut self, x0: f32, x1: f32, y: f32, c: [u8; 3]) {
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y);
        pb.line_to(x1, y);
        let stroke = Stroke { width: self.meter_t, line_cap: LineCap::Round, ..Stroke::default() };
        self.pm.stroke_path(&pb.finish().unwrap(), &paint(c), &stroke, Transform::identity(), None);
    }

    fn dot(&mut self, cx: f32, r: f32, c: [u8; 3]) {
        let path = PathBuilder::from_circle(cx, self.h / 2.0, r).unwrap();
        self.pm.fill_path(&path, &paint(c), FillRule::Winding, Transform::identity(), None);
    }
}

fn paint(c: [u8; 3]) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(c[0], c[1], c[2], 255);
    paint
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
                        let new_hover = self.item_at(x, y);
                        if new_hover != self.hover {
                            self.hover = new_hover;
                            self.draw();
                        }
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.hover.is_some() {
                        self.hover = None;
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
        self.width = configure.new_size.0;
        let lay = self.layout();
        if lay.end > self.width as f32 { eprintln!("rail: content is {:.0}px wide but the bar is {}px; lower `height`", lay.end, self.width); }
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
    // --pin: reserve space right away instead of deciding between peek and pin from the toggle key
    let pin = args.get(1).map(String::as_str) == Some("--pin");

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
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).unwrap();
    let layer_shell = LayerShell::bind(&globals, &qh).unwrap();
    let shm = Shm::bind(&globals, &qh).unwrap();
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).unwrap();

    let height = cfg.height;
    let sz = Sz::new(height);
    let pool = SlotPool::new((1920 * height * 4) as usize, &shm).unwrap();

    // Regular and bold faces of one family, selected by weight
    let mut db = cosmic_text::fontdb::Database::new();
    db.load_font_data(std::fs::read(expand_path(&cfg.font)).expect("failed to read font file"));
    db.load_font_data(std::fs::read(expand_path(&cfg.font_bold)).expect("failed to read bold font file"));
    let font_family = db.faces().next().expect("font file contains no faces").families[0].0.clone();
    let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);
    let cw = measure_text(&mut font_system, "0000000000", sz.text, &font_family, Weight::NORMAL) / 10.0;
    let clock_cw = measure_text(&mut font_system, "0000000000", sz.clock, &font_family, Weight::BOLD) / 10.0;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer: None,
        pointer: None,
        cursor_shape_manager,
        pool,
        width: 0,
        height,
        sz,
        exit: false,
        font_system,
        swash_cache: SwashCache::new(),
        colors,
        font_family,
        icons: Icons::new(sz.icon as u32),
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
        hover: None,
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
        battery: read_battery(),
        battery_read_at: now_unix(),
        brightness: read_brightness(),
        brightness_set_at: 0,
        notif_paused,
        long_press: pin.then_some(false),
        startup: Instant::now(),
        shown: String::new(),
    };

    // The configured output by name (needs a roundtrip to learn output names); otherwise the compositor's pick, the focused monitor
    let output = cfg.output.as_ref().map(|name| {
        event_queue.roundtrip(&mut app).unwrap();
        app.output_state.outputs().find(|o| app.output_state.info(o).and_then(|i| i.name).as_ref() == Some(name))
            .unwrap_or_else(|| panic!("no output named {name}"))
    });

    // Across the top edge. Pinned bars push windows down; a peek floats over them.
    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Top, Some("rail"), output.as_ref());
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, height);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    app.layer = Some(layer);
    if pin { app.reserve(); } else { app.layer().commit(); }

    let mut event_loop: EventLoop<App> = EventLoop::try_new().unwrap();
    WaylandSource::new(conn.clone(), event_queue).insert(event_loop.handle()).unwrap();

    loop {
        event_loop.dispatch(app.next_timeout(), &mut app).unwrap();
        app.tick();
        if app.exit {
            save_state(&app.state());
            break;
        }
    }
}
