use std::path::PathBuf;
use std::process::{Command, Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;
use libc;
use serde::Deserialize;
use smithay_client_toolkit as sctk;
use sctk::reexports::calloop::generic::Generic;
use sctk::reexports::calloop::timer::{TimeoutAction, Timer};
use sctk::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::registry_handlers;
use sctk::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym};
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
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};
use tiny_skia::Pixmap;

// --- Config ---

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    color_file: Option<String>,
    model: String,
    models_dir: String,
    sounds: bool,
    width: u32,
    height: u32,
    bar_count: usize,
    bar_width: u32,
    bar_gap: u32,
    margin: f32,
    scale: f32,
    border_width: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            color_file: Some("~/.cache/wal/colors-evoke.toml".into()),
            model: "medium.en".into(),
            models_dir: "~/.local/share/pywhispercpp/models".into(),
            sounds: false,
            width: 300,
            height: 60,
            bar_count: 48,
            bar_width: 4,
            bar_gap: 2,
            margin: 0.25,
            scale: 4.0,
            border_width: 1,
        }
    }
}

fn load_config() -> Config {
    let path = config_dir().join("evoke.toml");
    match std::fs::read_to_string(&path) {
        Ok(s) => match toml::from_str(&s) {
            Ok(cfg) => cfg,
            Err(e) => { eprintln!("evoke: failed to parse {}: {e}", path.display()); Config::default() }
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
    waveform: [u8; 3],
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            background: [0x1e, 0x1e, 0x2e],
            background_alpha: 0xd9, // ~0.85
            border: [0xcd, 0xd6, 0xf4],
            waveform: [0x89, 0xb4, 0xfa],
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
                            "waveform" => colors.waveform = c,
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    colors
}

// --- Constants ---

const CHUNK_SAMPLES: usize = 320; // 20ms at 16kHz
const TICK_MS: u64 = 33; // ~30fps

// --- Phase ---

#[derive(PartialEq)]
enum Phase { Recording, Transcribing }

// --- Signal ---

static GOT_SIGNAL: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_: libc::c_int) {
    GOT_SIGNAL.store(true, Ordering::Release);
}

// --- App ---

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    cursor_shape_manager: CursorShapeManager,
    loop_handle: sctk::reexports::calloop::LoopHandle<'static, App>,
    pool: SlotPool,
    width: u32,
    height: u32,
    exit: bool,
    configured: bool,
    colors: Colors,
    config: Config,
    // Recording state
    phase: Phase,
    started_at: Instant,
    audio_samples: Vec<i16>,
    ring_buf: Vec<f32>,
    ring_pos: usize,
    // Child process
    recorder: Option<Child>,
    // Pending audio bytes (for when we get an odd number of bytes)
    pending_byte: Option<u8>,
    // Transcription result from background thread
    transcription_rx: Option<mpsc::Receiver<String>>,
    // Screen height for margin calculation
    margin_set: bool,
}

impl App {
    fn process_audio_chunk(&mut self, buf: &[u8]) {
        let mut start = 0;
        // Handle leftover byte from previous chunk
        if let Some(prev) = self.pending_byte.take() {
            if !buf.is_empty() {
                let sample = i16::from_le_bytes([prev, buf[0]]);
                self.audio_samples.push(sample);
                start = 1;
            } else {
                self.pending_byte = Some(prev);
                return;
            }
        }

        let remaining = &buf[start..];
        let pairs = remaining.len() / 2;
        for chunk in remaining[..pairs * 2].chunks_exact(2) {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            self.audio_samples.push(sample);
        }
        if remaining.len() % 2 != 0 {
            self.pending_byte = Some(remaining[remaining.len() - 1]);
        }

        // Update ring buffer with RMS of latest chunk
        let len = self.audio_samples.len();
        if len >= CHUNK_SAMPLES {
            let start_idx = len - CHUNK_SAMPLES;
            let rms: f32 = (self.audio_samples[start_idx..].iter()
                .map(|&s| (s as f32).powi(2))
                .sum::<f32>() / CHUNK_SAMPLES as f32)
                .sqrt() / 32768.0;
            let bar_count = self.ring_buf.len();
            self.ring_buf[self.ring_pos % bar_count] = rms;
            self.ring_pos += 1;
        }
    }

    fn stop_and_transcribe(&mut self) {
        // Kill recorder
        if let Some(mut child) = self.recorder.take() {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM); }
            child.wait().ok();
        }

        self.phase = Phase::Transcribing;
        self.started_at = Instant::now();

        // Convert s16 -> f32
        let samples_f32: Vec<f32> = self.audio_samples.iter()
            .map(|&s| s as f32 / 32768.0)
            .collect();

        if samples_f32.is_empty() {
            eprintln!("evoke: no audio captured");
            self.exit = true;
            return;
        }

        eprintln!("evoke: transcribing {} samples ({:.1}s)...",
            samples_f32.len(), samples_f32.len() as f64 / 16000.0);

        let model_path = expand_path(&self.config.models_dir)
            .join(format!("ggml-{}.bin", self.config.model));

        let (tx, rx) = mpsc::channel();
        self.transcription_rx = Some(rx);

        std::thread::spawn(move || {
            let text = transcribe(&model_path, &samples_f32);
            tx.send(text).ok();
        });
    }

    fn draw(&mut self) {
        if !self.configured { return; }
        let c = &self.colors;

        let stride = self.width as i32 * 4;
        let (wl_buf, canvas) = self.pool
            .create_buffer(self.width as i32, self.height as i32, stride, wl_shm::Format::Argb8888)
            .unwrap();

        let mut pixmap = Pixmap::new(self.width, self.height).unwrap();
        pixmap.fill(tiny_skia::Color::TRANSPARENT);

        let pw = pixmap.width();
        let ph = pixmap.height();

        // Background
        fill_rect_alpha(pixmap.data_mut(), pw, ph, 0, 0, self.width, self.height, c.background, c.background_alpha);

        // Border
        let bw = self.config.border_width;
        if bw > 0 {
            fill_rect(pixmap.data_mut(), pw, ph, 0, 0, self.width, bw, c.border);
            fill_rect(pixmap.data_mut(), pw, ph, 0, self.height - bw, self.width, bw, c.border);
            fill_rect(pixmap.data_mut(), pw, ph, 0, 0, bw, self.height, c.border);
            fill_rect(pixmap.data_mut(), pw, ph, self.width - bw, 0, bw, self.height, c.border);
        }

        match self.phase {
            Phase::Recording => {
                let bar_count = self.config.bar_count;
                let bar_w = self.config.bar_width;
                let gap = self.config.bar_gap;
                let scale = self.config.scale;
                let total_w = bar_count as u32 * (bar_w + gap) - gap;
                let x_start = (self.width - total_w) / 2;
                let max_bar_h = self.height - 20;
                let padding_y = 10;

                for i in 0..bar_count {
                    let idx = (self.ring_pos + i) % bar_count;
                    let amplitude = (self.ring_buf[idx] * scale).min(1.0);
                    let bar_h = ((amplitude * max_bar_h as f32) as u32).max(2);
                    let x = x_start + i as u32 * (bar_w + gap);
                    let y = padding_y + (max_bar_h - bar_h) / 2;
                    fill_rect(pixmap.data_mut(), pw, ph, x, y, bar_w, bar_h, c.waveform);
                }
            }
            Phase::Transcribing => {
                let bar_count = self.config.bar_count;
                let bar_w = self.config.bar_width;
                let gap = self.config.bar_gap;
                let total_w = bar_count as u32 * (bar_w + gap) - gap;
                let x_start = (self.width - total_w) / 2;
                let max_bar_h = self.height - 20;
                let padding_y = 10;

                let t = self.started_at.elapsed().as_secs_f32();
                for i in 0..bar_count {
                    let frac = i as f32 / bar_count as f32;
                    let amplitude = ((frac * std::f32::consts::TAU * 2.0 + t * 12.0).sin() * 0.5 + 0.5) * 0.25;
                    let bar_h = ((amplitude * max_bar_h as f32) as u32).max(2);
                    let x = x_start + i as u32 * (bar_w + gap);
                    let y = padding_y + (max_bar_h - bar_h) / 2;
                    fill_rect(pixmap.data_mut(), pw, ph, x, y, bar_w, bar_h, c.waveform);
                }
            }
        }

        // Copy RGBA premul -> BGRA
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

// --- Transcription ---

fn transcribe(model_path: &std::path::Path, samples: &[f32]) -> String {
    use whisper_rs::{WhisperContext, WhisperContextParameters, FullParams, SamplingStrategy};

    let mut ctx_params = WhisperContextParameters::default();
    ctx_params.use_gpu(true);
    let ctx = match WhisperContext::new_with_params(model_path.to_str().unwrap(), ctx_params) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("evoke: failed to load whisper model {}: {e}", model_path.display());
            return String::new();
        }
    };

    let mut state = match ctx.create_state() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("evoke: failed to create whisper state: {e}");
            return String::new();
        }
    };

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(4);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    params.set_single_segment(true);
    params.set_no_timestamps(true);

    if let Err(e) = state.full(params, samples) {
        eprintln!("evoke: transcription failed: {e}");
        return String::new();
    }

    let n = state.full_n_segments();
    let mut text = String::new();
    for i in 0..n {
        if let Some(seg) = state.get_segment(i) {
            if let Ok(s) = seg.to_str() {
                text.push_str(s);
            }
        }
    }
    text
}

// --- Output ---

fn output_text(text: &str) {
    // Copy to clipboard
    match Command::new("wl-copy").arg("--").stdin(Stdio::piped()).spawn() {
        Ok(mut proc) => {
            if let Some(mut stdin) = proc.stdin.take() {
                use std::io::Write;
                stdin.write_all(text.as_bytes()).ok();
            }
            proc.wait().ok();
        }
        Err(e) => eprintln!("evoke: wl-copy failed: {e}"),
    }

    // Paste via Ctrl+V
    Command::new("ydotool").args(["key", "29:1", "47:1", "47:0", "29:0"]).status().ok();
}

// --- Rendering helpers ---

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


// --- Wayland handler boilerplate ---

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, output: &wl_output::WlOutput) {
        if !self.margin_set {
            // Set bottom margin to 25% of screen height
            if let Some(info) = self.output_state.info(output) {
                if let Some(size) = info.logical_size {
                    let margin = (size.1 as f32 * self.config.margin) as u32;
                    self.layer.set_margin(0, 0, margin as i32, 0);
                    self.layer.wl_surface().commit();
                    self.margin_set = true;
                }
            }
        }
    }
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
                Box::new(|_state, _wl_kbd, _event| {}),
            ).unwrap());
        }
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
            if let PointerEventKind::Enter { serial } = event.kind {
                let device = self.cursor_shape_manager.get_shape_device(pointer, qh);
                device.set_shape(serial, Shape::Default);
                device.destroy();
            }
        }
    }
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if event.keysym == Keysym::Escape { self.exit = true; }
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: sctk::seat::keyboard::Modifiers, _: sctk::seat::keyboard::RawModifiers, _: u32) {}
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
        self.configured = true;
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

// --- Main ---

fn main() {
    let cfg = load_config();
    let colors = load_colors(cfg.color_file.as_deref());

    // Set up SIGUSR1 handler
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as *const () as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
    }

    // Start recording: pw-record to stdout with raw PCM
    let recorder = Command::new("pw-record")
        .args(["--format=s16", "--rate=16000", "--channels=1", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start pw-record");

    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<App> = EventLoop::try_new().unwrap();
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue).insert(loop_handle.clone()).unwrap();

    let compositor = CompositorState::bind(&globals, &qh).unwrap();
    let layer_shell = LayerShell::bind(&globals, &qh).unwrap();
    let shm = Shm::bind(&globals, &qh).unwrap();
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).unwrap();

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("evoke"), None);
    layer.set_size(cfg.width, cfg.height);
    layer.set_anchor(Anchor::BOTTOM);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.wl_surface().commit();

    let pool = SlotPool::new((cfg.width * cfg.height * 4) as usize, &shm).unwrap();

    let ring_buf = vec![0.0; cfg.bar_count];
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        pointer: None,
        keyboard: None,
        cursor_shape_manager,
        loop_handle: loop_handle.clone(),
        pool,
        width: cfg.width,
        height: cfg.height,
        exit: false,
        configured: false,
        colors,
        phase: Phase::Recording,
        started_at: Instant::now(),
        audio_samples: Vec::with_capacity(16000 * 60),
        ring_buf,
        ring_pos: 0,
        recorder: None,
        pending_byte: None,
        transcription_rx: None,
        margin_set: false,
        config: cfg,
    };

    // Set up pw-record stdout as calloop source
    let mut recorder = recorder;
    let stdout = recorder.stdout.take().unwrap();
    app.recorder = Some(recorder);

    // Set non-blocking
    use std::os::unix::io::AsRawFd;
    let stdout_fd = stdout.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(stdout_fd, libc::F_GETFL);
        libc::fcntl(stdout_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let generic_source = Generic::new(stdout, Interest::READ, Mode::Level);
    loop_handle.insert_source(generic_source, |_, stdout_wrapper, app: &mut App| {
        let fd = stdout_wrapper.as_ref().as_raw_fd();
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 { break; }
            app.process_audio_chunk(&buf[..n as usize]);
        }
        Ok(PostAction::Continue)
    }).unwrap();

    // Redraw timer (~30fps)
    let timer = Timer::from_duration(std::time::Duration::from_millis(TICK_MS));
    loop_handle.insert_source(timer, |_, _, app| {
        // Check for SIGUSR1
        if GOT_SIGNAL.load(Ordering::Acquire) && app.phase == Phase::Recording {
            GOT_SIGNAL.store(false, Ordering::Release);
            app.stop_and_transcribe();
        }
        // Check for transcription result
        if let Some(rx) = &app.transcription_rx {
            if let Ok(text) = rx.try_recv() {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    eprintln!("evoke: transcribed: {text}");
                    output_text(&text);
                } else {
                    eprintln!("evoke: no speech detected");
                }
                app.exit = true;
            }
        }
        if !app.exit {
            app.draw();
        }
        TimeoutAction::ToDuration(std::time::Duration::from_millis(TICK_MS))
    }).unwrap();

    loop {
        event_loop.dispatch(std::time::Duration::from_millis(TICK_MS), &mut app).unwrap();
        if app.exit { break; }
    }
}
