use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping, SwashCache, SwashContent};
use serde::{Deserialize, Serialize};
use smithay_client_toolkit as sctk;
use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::calloop::{EventLoop, LoopHandle};
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::registry_handlers;
use sctk::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use sctk::seat::pointer::cursor_shape::CursorShapeManager;
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
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
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
};
use tiny_skia::Pixmap;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

// --- Config ---

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    color_file: Option<String>,
    font: String,
    font_size: f32,
    comment_font_size: f32,
    icon_size: u32,
    window_width: u32,
    window_height: u32,
    terminal: String,
    columns: usize,
    show_comments: bool,
    search_comments: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            color_file: None, font: "~/.local/share/fonts/GoogleSansCode-Regular.ttf".into(),
            font_size: 18.0, comment_font_size: 14.0, icon_size: 32,
            window_width: 600, window_height: 400,
            terminal: "ghostty -e".into(),
            columns: 1, show_comments: true, search_comments: false,
        }
    }
}

fn load_config() -> Config {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".config"));
    let path = base.join("widgets/grimoire.toml");
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Config::default(),
    };
    match toml::from_str(&content) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("grimoire: failed to parse {}: {e}", path.display());
            Config::default()
        }
    }
}

// --- Colors ---

struct Colors {
    background: [u8; 3],
    background_alpha: u8,
    border: [u8; 3],
    bar_bg: [u8; 3],
    bar_border: [u8; 3],
    text: [u8; 3],
    text_comment: [u8; 3],
    text_placeholder: [u8; 3],
    selection: [u8; 3],
    selection_alpha: u8,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            background: [0x1a, 0x1a, 0x2e], background_alpha: 0xff,
            border: [0x4a, 0x4a, 0x6e],
            bar_bg: [0x2a, 0x2a, 0x4e], bar_border: [0x4a, 0x4a, 0x6e],
            text: [0xe0, 0xe0, 0xe0], text_comment: [0x80, 0x80, 0x90],
            text_placeholder: [0x60, 0x60, 0x70],
            selection: [0x40, 0x40, 0x90], selection_alpha: 0xcc,
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
                            "border" => colors.border = c,
                            "bar_bg" => colors.bar_bg = c,
                            "bar_border" => colors.bar_border = c,
                            "text" => colors.text = c,
                            "text_comment" => colors.text_comment = c,
                            "text_placeholder" => colors.text_placeholder = c,
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

// --- Desktop entry parsing ---

fn desktop_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap();
    vec![
        PathBuf::from(&home).join(".local/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/usr/share/applications"),
    ]
}

fn strip_field_codes(exec: &str) -> String {
    let mut result = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&next) = chars.peek() {
                if "fFuUdDnNickvm".contains(next) {
                    chars.next();
                    continue;
                }
            }
        }
        result.push(c);
    }
    result.trim().to_string()
}

fn parse_desktop_file(path: &Path) -> Option<(String, String, String, String, bool)> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut comment = String::new();
    let mut icon = String::new();
    let mut terminal = false;
    let mut no_display = false;
    let mut hidden = false;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            if in_entry { break; }
            if line == "[Desktop Entry]" { in_entry = true; }
            continue;
        }
        if !in_entry { continue; }
        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim();
            let val = val.trim();
            match key {
                "Name" => name = Some(val.to_string()),
                "Exec" => exec = Some(strip_field_codes(val)),
                "Comment" => comment = val.to_string(),
                "Icon" => icon = val.to_string(),
                "Terminal" => terminal = val.eq_ignore_ascii_case("true"),
                "NoDisplay" => no_display = val.eq_ignore_ascii_case("true"),
                "Hidden" => hidden = val.eq_ignore_ascii_case("true"),
                "Type" => { if val != "Application" { return None; } }
                _ => {}
            }
        }
    }
    if no_display || hidden { return None; }
    Some((name?, exec?, comment, icon, terminal))
}

// --- Icon resolution ---

fn icon_cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".cache"));
    base.join("thumbnails/grimoire")
}

fn find_icon_path(name: &str) -> Option<PathBuf> {
    if name.starts_with('/') {
        let p = PathBuf::from(name);
        if p.exists() { return Some(p); }
        return None;
    }
    let sizes = ["48x48", "64x64", "32x32", "128x128", "256x256"];
    for size in &sizes {
        let p = PathBuf::from(format!("/usr/share/icons/hicolor/{size}/apps/{name}.png"));
        if p.exists() { return Some(p); }
    }
    let svg = PathBuf::from(format!("/usr/share/icons/hicolor/scalable/apps/{name}.svg"));
    if svg.exists() { return Some(svg); }
    for ext in ["png", "svg"] {
        let p = PathBuf::from(format!("/usr/share/pixmaps/{name}.{ext}"));
        if p.exists() { return Some(p); }
    }
    None
}

fn load_svg(path: &Path, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    let data = std::fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
    let ts = tree.size();
    let sx = size as f32 / ts.width();
    let sy = size as f32 / ts.height();
    let scale = sx.min(sy);
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some((pixmap.take(), size, size))
}

fn load_icon(path: &Path, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    if path.extension().is_some_and(|e| e == "svg") {
        return load_svg(path, size);
    }
    let img = image::open(path).ok()?;
    let resized = img.resize(size, size, image::imageops::FilterType::Triangle);
    let rgba = resized.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

fn icon_cache_key(name: &str, size: u32) -> String {
    let mut h = DefaultHasher::new();
    name.hash(&mut h);
    size.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn resolve_icon(name: &str, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    if name.is_empty() { return None; }
    let cd = icon_cache_dir();
    let key = icon_cache_key(name, size);
    let cached = cd.join(format!("{key}.png"));

    if cached.exists() {
        if let Ok(img) = image::open(&cached) {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            return Some((rgba.into_raw(), w, h));
        }
    }

    let path = find_icon_path(name)?;
    let (data, w, h) = load_icon(&path, size)?;

    // Cache as PNG (only for non-SVG sources or any resolved icon)
    std::fs::create_dir_all(&cd).ok();
    if let Some(img_buf) = image::RgbaImage::from_raw(w, h, data.clone()) {
        img_buf.save(&cached).ok();
    }

    Some((data, w, h))
}

// --- Items ---

struct Item {
    name: String,
    exec: String,
    comment: String,
    icon_data: Option<Vec<u8>>,
    icon_w: u32,
    icon_h: u32,
    terminal: bool,
    desktop_id: String,
}

// --- Frecency ---

#[derive(Serialize, Deserialize, Clone, Default)]
struct FrecencyEntry {
    count: u32,
    last: u64,
}

fn frecency_state_path() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".local/state"));
    base.join("widgets/grimoire.toml")
}

fn load_frecency() -> HashMap<String, FrecencyEntry> {
    let path = frecency_state_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => match toml::from_str(&s) {
            Ok(state) => state,
            Err(e) => {
                eprintln!("grimoire: failed to parse {}: {e}", path.display());
                HashMap::new()
            }
        },
        Err(_) => HashMap::new(),
    }
}

fn save_frecency(state: &HashMap<String, FrecencyEntry>) {
    let path = frecency_state_path();
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    let content = toml::to_string(state).unwrap();
    if let Err(e) = std::fs::write(&path, content) {
        eprintln!("grimoire: failed to save frecency: {e}");
    }
}

fn frecency_score(entry: &FrecencyEntry, now: u64) -> f64 {
    let hours = now.saturating_sub(entry.last) as f64 / 3600.0;
    entry.count as f64 / (1.0 + hours / 72.0)
}

fn load_desktop_entries(icon_size: u32, frecency: &HashMap<String, FrecencyEntry>) -> Vec<Item> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut items: Vec<Item> = Vec::new();

    for dir in desktop_dirs() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "desktop") { continue; }
            let filename = path.file_name().unwrap().to_string_lossy().to_string();
            let desktop_id = path.file_stem().unwrap().to_string_lossy().to_string();

            if let Some((name, exec, comment, icon_name, terminal)) = parse_desktop_file(&path) {
                let (icon_data, icon_w, icon_h) = match resolve_icon(&icon_name, icon_size) {
                    Some((d, w, h)) => (Some(d), w, h),
                    None => (None, 0, 0),
                };
                let item = Item { name, exec, comment, icon_data, icon_w, icon_h, terminal, desktop_id };

                if let Some(&idx) = seen.get(&filename) {
                    items[idx] = item; // local overrides system
                } else {
                    seen.insert(filename, items.len());
                    items.push(item);
                }
            }
        }
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    items.sort_by(|a, b| {
        let sa = frecency.get(&a.desktop_id).map_or(0.0, |e| frecency_score(e, now));
        let sb = frecency.get(&b.desktop_id).map_or(0.0, |e| frecency_score(e, now));
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    items
}

fn load_stdin_items() -> Vec<Item> {
    let stdin = std::io::stdin();
    stdin.lock().lines().flatten().map(|line| {
        Item {
            name: line.clone(), exec: line, comment: String::new(),
            icon_data: None, icon_w: 0, icon_h: 0, terminal: false, desktop_id: String::new(),
        }
    }).collect()
}

// --- Mode ---

#[derive(PartialEq)]
enum Mode { Drun, Dmenu }

// --- App ---

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_shape_manager: CursorShapeManager,
    pool: SlotPool,
    width: u32,
    height: u32,
    exit: bool,
    font_system: FontSystem,
    swash_cache: SwashCache,
    loop_handle: LoopHandle<'static, App>,
    mode: Mode,
    items: Vec<Item>,
    filtered: Vec<usize>,
    selected: usize,
    scroll_offset: usize,
    input: String,
    colors: Colors,
    font_size: f32,
    comment_font_size: f32,
    icon_size: u32,
    terminal_cmd: String,
    font_family: String,
    hover_index: Option<usize>,
    cols: usize,
    show_comments: bool,
    search_comments: bool,
    frecency: HashMap<String, FrecencyEntry>,
}

const BAR_H: f32 = 50.0;
const PAD: f32 = 8.0;
const ROW_PAD: f32 = 8.0;

impl App {
    fn row_height(&self) -> f32 { self.icon_size as f32 + ROW_PAD }
    fn visible_rows(&self) -> usize { ((self.height as f32 - BAR_H) / self.row_height()).max(0.0) as usize }

    fn effective_cols(&self) -> usize {
        let n = self.filtered.len();
        if n == 0 { return self.cols; }
        n.min(self.cols).max(1)
    }

    fn col_width(&self) -> f32 { self.width as f32 / self.cols as f32 }

    fn grid_x_offset(&self) -> f32 {
        let ecols = self.effective_cols();
        let col_w = self.col_width();
        (self.width as f32 - ecols as f32 * col_w) / 2.0
    }

    fn item_at_pos(&self, x: f32, y: f32) -> Option<usize> {
        if y < BAR_H { return None; }
        let ecols = self.effective_cols();
        let col_w = self.col_width();
        let x_off = self.grid_x_offset();
        if x < x_off { return None; }
        let col = ((x - x_off) / col_w) as usize;
        if col >= ecols { return None; }
        let row = ((y - BAR_H) / self.row_height()) as usize;
        let idx = self.scroll_offset + row * ecols + col;
        if idx < self.filtered.len() { Some(idx) } else { None }
    }

    fn ensure_visible(&mut self) {
        let ecols = self.effective_cols();
        let visible = self.visible_rows() * ecols;
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
                .filter(|&i| fuzzy_match(&self.items[i].name, &self.input)
                    || (self.search_comments && fuzzy_match(&self.items[i].comment, &self.input)))
                .collect()
        };
        self.selected = 0;
        self.scroll_offset = 0;
    }

    fn select_item(&mut self) {
        if self.filtered.is_empty() { return; }
        let item = &self.items[self.filtered[self.selected]];

        if self.mode == Mode::Dmenu {
            println!("{}", item.exec);
            self.exit = true;
            return;
        }

        // Update frecency
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let entry = self.frecency.entry(item.desktop_id.clone()).or_default();
        entry.count += 1;
        entry.last = now;
        save_frecency(&self.frecency);

        // drun: fork+exec
        let exec_cmd = if item.terminal {
            format!("{} {}", self.terminal_cmd, item.exec)
        } else {
            item.exec.clone()
        };
        Command::new("sh")
            .arg("-c")
            .arg(&exec_cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        self.exit = true;
    }

    fn handle_key(&mut self, event: &KeyEvent) {
        if event.keysym == Keysym::Escape {
            self.exit = true;
            return;
        }
        if event.keysym == Keysym::Return {
            self.select_item();
            return;
        }
        let n = self.filtered.len();
        let ecols = self.effective_cols();
        let changed = match event.keysym {
            Keysym::BackSpace => {
                if self.input.pop().is_some() { self.refilter(); true } else { false }
            }
            Keysym::Left if self.selected > 0 => { self.selected -= 1; true }
            Keysym::Right if self.selected + 1 < n => { self.selected += 1; true }
            Keysym::Up if self.selected >= ecols => { self.selected -= ecols; true }
            Keysym::Down if self.selected + ecols < n => { self.selected += ecols; true }
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
        let bg = self.colors.background;
        let bg_alpha = self.colors.background_alpha;
        let bar_bg = self.colors.bar_bg;
        let bar_border = self.colors.bar_border;
        let border = self.colors.border;
        let text_color = self.colors.text;
        let comment_color = self.colors.text_comment;
        let placeholder_color = self.colors.text_placeholder;
        let sel_color = self.colors.selection;
        let sel_alpha = self.colors.selection_alpha;
        let row_h = self.row_height();
        let ecols = self.effective_cols();
        let col_w = self.col_width();
        let x_off = self.grid_x_offset();
        let visible = self.visible_rows() * ecols;
        let icon_sz = self.icon_size;
        let has_icons = self.mode == Mode::Drun;
        let icon_pad = if has_icons { PAD + icon_sz as f32 + PAD } else { PAD };
        let font_size = self.font_size;
        let comment_font_size = self.comment_font_size;
        let show_comments = self.show_comments;
        let width = self.width;
        let height = self.height;
        let start = self.scroll_offset;
        let end = (start + visible).min(self.filtered.len());
        let selected = self.selected;
        let hover = self.hover_index;
        let filtered: Vec<usize> = self.filtered[start..end].to_vec();

        let stride = width as i32 * 4;
        let (wl_buf, canvas) = self.pool
            .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
            .unwrap();

        let mut pixmap = Pixmap::new(width, height).unwrap();
        pixmap.fill(tiny_skia::Color::from_rgba8(bg[0], bg[1], bg[2], bg_alpha));

        let pw = pixmap.width();
        let ph = pixmap.height();

        // Search bar background
        fill_rect_alpha(pixmap.data_mut(), pw, ph, 0, 0, width, BAR_H as u32, bar_bg, bg_alpha);

        // Search bar bottom border
        fill_rect_alpha(pixmap.data_mut(), pw, ph, 0, BAR_H as u32 - 2, width, 2, bar_border, bg_alpha);

        // Window border
        fill_rect_alpha(pixmap.data_mut(), pw, ph, 0, 0, width, 2, border, bg_alpha);
        fill_rect_alpha(pixmap.data_mut(), pw, ph, 0, height - 2, width, 2, border, bg_alpha);
        fill_rect_alpha(pixmap.data_mut(), pw, ph, 0, 0, 2, height, border, bg_alpha);
        fill_rect_alpha(pixmap.data_mut(), pw, ph, width - 2, 0, 2, height, border, bg_alpha);

        // Search text or placeholder
        if self.input.is_empty() {
            let placeholder = "Search...";
            let tw = measure_text(&mut self.font_system, placeholder, font_size, &self.font_family);
            let tx = (width as f32 - tw) / 2.0;
            let ty = (BAR_H + font_size) / 2.0;
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                placeholder, tx, ty, font_size, width as f32, BAR_H, placeholder_color,
                &self.font_family);
        } else {
            let tw = measure_text(&mut self.font_system, &self.input, font_size, &self.font_family);
            let tx = (width as f32 - tw) / 2.0;
            let ty = (BAR_H + font_size) / 2.0;
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                &self.input, tx, ty, font_size, width as f32, BAR_H, text_color,
                &self.font_family);
        }

        // Grid items
        for (vi, &item_idx) in filtered.iter().enumerate() {
            let i = start + vi;
            let col = vi % ecols;
            let row = vi / ecols;
            let cell_x = x_off + col as f32 * col_w;
            let cell_y = BAR_H + row as f32 * row_h;
            let text_x = cell_x + icon_pad;

            // Selection highlight
            if i == selected {
                fill_rect_alpha(pixmap.data_mut(), pw, ph,
                    cell_x as u32, cell_y as u32, col_w as u32, row_h as u32, sel_color, sel_alpha);
            } else if hover == Some(i) {
                fill_rect_alpha(pixmap.data_mut(), pw, ph,
                    cell_x as u32, cell_y as u32, col_w as u32, row_h as u32, sel_color, sel_alpha / 2);
            }

            // Icon
            if has_icons {
                if let Some(ref data) = self.items[item_idx].icon_data {
                    let iw = self.items[item_idx].icon_w;
                    let ih = self.items[item_idx].icon_h;
                    let ix = cell_x as i32 + PAD as i32;
                    let iy = cell_y as i32 + (row_h as i32 - ih as i32) / 2;
                    blit_rgba(pixmap.data_mut(), pw as i32, ph as i32,
                        ix, iy, iw as i32, ih as i32, data);
                }
            }

            // Name
            let name_y = cell_y + (row_h + font_size) / 2.0;
            let max_name_w = if !show_comments || self.items[item_idx].comment.is_empty() {
                (col_w - icon_pad).max(0.0)
            } else {
                (col_w - icon_pad) * 0.5
            };
            render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                &self.items[item_idx].name, text_x, name_y, font_size,
                max_name_w, row_h, text_color, &self.font_family);

            // Comment
            if show_comments && !self.items[item_idx].comment.is_empty() {
                let name_w = measure_text(&mut self.font_system, &self.items[item_idx].name,
                    font_size, &self.font_family);
                let comment_x = text_x + name_w.min(max_name_w) + 12.0;
                let comment_y = cell_y + (row_h + comment_font_size) / 2.0;
                let comment_max_w = (cell_x + col_w - comment_x - PAD).max(0.0);
                if comment_max_w > 20.0 {
                    render_text(&mut pixmap, &mut self.font_system, &mut self.swash_cache,
                        &self.items[item_idx].comment, comment_x, comment_y,
                        comment_font_size, comment_max_w, row_h, comment_color,
                        &self.font_family);
                }
            }
        }

        // Copy RGBA -> BGRA
        for (dst, src) in canvas.chunks_exact_mut(4).zip(pixmap.data().chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }

        wl_buf.attach_to(self.layer.wl_surface()).unwrap();
        self.layer.wl_surface().damage_buffer(0, 0, width as i32, height as i32);
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

fn make_attrs(family: &str) -> Attrs<'_> {
    Attrs::new().family(cosmic_text::Family::Name(family))
}

fn measure_text(font_system: &mut FontSystem, text: &str, font_size: f32, family: &str) -> f32 {
    let mut buf = Buffer::new(font_system, Metrics::new(font_size, font_size * 1.2));
    buf.set_size(font_system, None, None);
    buf.set_text(font_system, text, &make_attrs(family), Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);
    buf.layout_runs().next().map_or(0.0, |r| r.line_w)
}

fn render_text(
    pixmap: &mut Pixmap, font_system: &mut FontSystem, swash_cache: &mut SwashCache,
    text: &str, x: f32, y: f32, font_size: f32, max_w: f32, max_h: f32, color: [u8; 3],
    family: &str,
) {
    let line_h = font_size * 1.2;
    let mut buf = Buffer::new(font_system, Metrics::new(font_size, line_h));
    buf.set_size(font_system, Some(max_w), Some(max_h));
    buf.set_text(font_system, text, &make_attrs(family), Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);

    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    if let Some(run) = buf.layout_runs().next() {
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
            let a = src[si + 3] as u32;
            if a == 0 { continue; }
            if a == 255 {
                data[di] = src[si];
                data[di + 1] = src[si + 1];
                data[di + 2] = src[si + 2];
                data[di + 3] = 255;
            } else {
                let inv = 255 - a;
                data[di]     = ((src[si] as u32 * a + data[di] as u32 * inv) / 255) as u8;
                data[di + 1] = ((src[si + 1] as u32 * a + data[di + 1] as u32 * inv) / 255) as u8;
                data[di + 2] = ((src[si + 2] as u32 * a + data[di + 2] as u32 * inv) / 255) as u8;
                data[di + 3] = ((a + data[di + 3] as u32 * inv / 255)) as u8;
            }
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
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, pointer: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        let mut redraw = false;
        for event in events {
            match event.kind {
                PointerEventKind::Enter { serial, .. } => {
                    let device = self.cursor_shape_manager.get_shape_device(pointer, qh);
                    device.set_shape(serial, Shape::Default);
                    device.destroy();
                }
                PointerEventKind::Press { button: 0x110, .. } => {
                    if let Some(idx) = self.item_at_pos(event.position.0 as f32, event.position.1 as f32) {
                        self.selected = idx;
                        self.select_item();
                        return;
                    }
                }
                PointerEventKind::Motion { .. } => {
                    let new_hover = self.item_at_pos(event.position.0 as f32, event.position.1 as f32);
                    if new_hover != self.hover_index {
                        self.hover_index = new_hover;
                        redraw = true;
                    }
                }
                PointerEventKind::Axis { ref vertical, .. } => {
                    let ecols = self.effective_cols();
                    let visible = self.visible_rows() * ecols;
                    if vertical.absolute > 0.0 && self.scroll_offset + visible < self.filtered.len() {
                        self.scroll_offset = (self.scroll_offset + ecols)
                            .min(self.filtered.len().saturating_sub(visible));
                        redraw = true;
                    } else if vertical.absolute < 0.0 && self.scroll_offset > 0 {
                        self.scroll_offset = self.scroll_offset.saturating_sub(ecols);
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

// --- Main ---

fn main() {
    let cfg = load_config();
    let colors = load_colors(cfg.color_file.as_deref());

    let args: Vec<String> = std::env::args().collect();
    let mut mode = Mode::Drun;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--dmenu" => { mode = Mode::Dmenu; i += 1; }
            "--drun" => { mode = Mode::Drun; i += 1; }
            _ => { eprintln!("grimoire: unknown arg: {}", args[i]); i += 1; }
        }
    }

    let frecency = load_frecency();
    let items = match mode {
        Mode::Drun => load_desktop_entries(cfg.icon_size, &frecency),
        Mode::Dmenu => load_stdin_items(),
    };

    let width = cfg.window_width;
    let height = cfg.window_height;

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
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("grimoire"), None);
    layer.set_size(width, height);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.wl_surface().commit();

    let pool = SlotPool::new((width * height * 4) as usize, &shm).unwrap();

    let filtered: Vec<usize> = (0..items.len()).collect();

    let font_data = std::fs::read(expand_path(&cfg.font)).expect("failed to read font file");
    let mut db = fontdb::Database::new();
    db.load_font_data(font_data);
    let font_family = db.faces().next().expect("font file contains no faces").families[0].0.clone();
    let font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        keyboard: None,
        pointer: None,
        cursor_shape_manager,
        pool,
        width,
        height,
        exit: false,
        font_system,
        swash_cache: SwashCache::new(),
        loop_handle: event_loop.handle(),
        mode,
        filtered,
        items,
        selected: 0,
        scroll_offset: 0,
        input: String::new(),
        colors,
        font_size: cfg.font_size,
        comment_font_size: cfg.comment_font_size,
        icon_size: cfg.icon_size,
        terminal_cmd: cfg.terminal,
        font_family,
        hover_index: None,
        cols: cfg.columns.max(1),
        show_comments: cfg.show_comments,
        search_comments: cfg.search_comments,
        frecency,
    };

    loop {
        event_loop.dispatch(Duration::from_millis(16), &mut app).unwrap();
        if app.exit { break; }
    }
}
