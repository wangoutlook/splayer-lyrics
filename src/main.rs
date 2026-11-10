mod api;
mod config;
mod logging;
mod model;
mod render;
mod tray;

use crate::api::{Command, ControlAction, UiMsg};
use crate::config::Config;
use crate::model::{pick_primary_index, LyricLine, LyricWord, NowPlaying};
use gtk4::prelude::*;
use gtk4::glib;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type TickCell = Rc<RefCell<Option<glib::SourceId>>>;

struct Args {
    config: PathBuf,
    debug: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        config: PathBuf::from("config.json"),
        debug: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                if let Some(value) = iter.next() {
                    args.config = PathBuf::from(value);
                }
            }
            "--debug" => args.debug = true,
            _ => {
                if let Some(value) = arg.strip_prefix("--config=") {
                    args.config = PathBuf::from(value);
                }
            }
        }
    }
    args
}

fn epoch_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
struct SavedPosition {
    x: i32,
    y: i32,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct UiSettings {
    visible: Option<bool>,
    font_size: Option<i32>,
    font_family: Option<String>,
    played_color: Option<String>,
    follow_system_color: Option<bool>,
}

fn load_ui_settings(path: &Path) -> UiSettings {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn system_accent_color() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".config/niri/colors.kdl");
    if let Ok(text) = fs::read_to_string(&path) {
        for (needle, quoted) in [("active-gradient", "from=\""), ("active-color", "\"")] {
            if let Some(line) = text.lines().find(|line| line.contains(needle)) {
                if let Some(pos) = line.find(quoted) {
                    let rest = &line[pos + quoted.len()..];
                    let value = rest.split('\"').next().unwrap_or("").trim();
                    let value = value.trim_start_matches('#');
                    if value.len() == 8 {
                        return Some(format!("#{}", &value[..6]));
                    }
                    if value.len() == 6 && value.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Some(format!("#{value}"));
                    }
                }
            }
        }
    }
    let waybar = PathBuf::from(std::env::var_os("HOME")?).join(".config/waybar/colors.css");
    if let Ok(text) = fs::read_to_string(waybar) {
        for line in text.lines() {
            if line.contains("@define-color") && line.contains("accent") {
                if let Some(pos) = line.find('#') {
                    let value = line[pos + 1..]
                        .chars()
                        .take_while(|c| c.is_ascii_hexdigit())
                        .collect::<String>();
                    if value.len() == 6 {
                        return Some(format!("#{value}"));
                    }
                }
            }
        }
    }
    None
}

fn save_ui_settings(path: &Path, settings: &UiSettings) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(settings) {
        let _ = fs::write(path, text);
    }
}

#[derive(Debug, Clone, Copy)]
struct DragState {
    start_x: i32,
    start_y: i32,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn set_config_output(path: &Path, output: &str) -> anyhow::Result<()> {
    let text = fs::read_to_string(path)?;
    // Prefer a text edit so the user's key order/formatting is preserved.
    if let Some(marker) = text.find("\"output\"") {
        let after = marker + "\"output\"".len();
        if let Some(colon) = text[after..].find(':').map(|offset| after + offset) {
            if let Some(quote_start) = text[colon..].find('"').map(|offset| colon + offset) {
                if let Some(quote_end) = text[quote_start + 1..]
                    .find('"')
                    .map(|offset| quote_start + 1 + offset)
                {
                    let mut updated = String::with_capacity(text.len());
                    updated.push_str(&text[..quote_start + 1]);
                    updated.push_str(output);
                    updated.push_str(&text[quote_end..]);
                    fs::write(path, updated)?;
                    return Ok(());
                }
            }
        }
    }

    // Fallback for configs without an output key: rewrite as JSON.
    let mut value: serde_json::Value = serde_json::from_str(&text)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("config root is not a JSON object"))?;
    object.insert(
        "output".to_string(),
        serde_json::Value::String(output.to_string()),
    );
    fs::write(path, serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

/// Writes the new output and restarts this process via start.sh once the old
/// process has exited. A full restart is much simpler and safer than tearing
/// down and rebuilding the layer surface in place.
fn restart_with_output(config_path: &Path, output: &str) {
    if let Err(err) = set_config_output(config_path, output) {
        logging::error(&format!(
            "cannot update output in {}: {err:#}",
            config_path.display()
        ));
        return;
    }

    let pid = std::process::id();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = config_path
        .canonicalize()
        .unwrap_or_else(|_| cwd.join(config_path));
    let script = format!(
        "while kill -0 {pid} 2>/dev/null; do sleep 0.1; done; cd {} && ./start.sh --config {}",
        shell_quote(&cwd.display().to_string()),
        shell_quote(&config.display().to_string())
    );

    match ProcessCommand::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => {
            logging::info(&format!("switching output to {output}, restarting overlay"));
            std::process::exit(0);
        }
        Err(err) => logging::error(&format!("cannot restart overlay: {err}")),
    }
}

struct UiState {
    cfg: Config,
    now: Option<NowPlaying>,
    track_id: Option<String>,
    playing: bool,
    speed: f64,
    duration_ms: f64,
    anchor_position: f64,
    anchor_at: Instant,
    api_ok: bool,
    api_error_logged: bool,
    api_offset_ms: f64,
    lyric_available: bool,
    lyrics: Vec<LyricLine>,
    fallback_line: Option<LyricLine>,
    lyrics_requested: Option<String>,
    lyrics_retry_at: Option<Instant>,
    lyrics_error_logged: bool,
    last_line: Option<usize>,
    hover: Option<usize>,
    hover_handle: bool,
    win_x: i32,
    win_y: i32,
    card_width: i32,
    drag: Option<DragState>,
    monitor_w: i32,
    monitor_h: i32,
    position_path: PathBuf,
    ui_settings_path: PathBuf,
    visible: bool,
    follow_system_color: bool,
    area: Option<gtk4::DrawingArea>,
    window: Option<gtk4::Window>,
}

impl UiState {
    fn new(
        cfg: Config,
        win_x: i32,
        win_y: i32,
        position_path: PathBuf,
        monitor_w: i32,
        monitor_h: i32,
    ) -> Self {
        let card_width = cfg.width;
        Self {
            cfg,
            now: None,
            track_id: None,
            playing: false,
            speed: 1.0,
            duration_ms: 0.0,
            anchor_position: 0.0,
            anchor_at: Instant::now(),
            api_ok: false,
            api_error_logged: false,
            api_offset_ms: 0.0,
            lyric_available: false,
            lyrics: Vec::new(),
            fallback_line: None,
            lyrics_requested: None,
            lyrics_retry_at: None,
            lyrics_error_logged: false,
            last_line: None,
            hover: None,
            hover_handle: false,
            win_x,
            win_y,
            card_width,
            drag: None,
            monitor_w,
            monitor_h,
            position_path,
            ui_settings_path: PathBuf::from("runtime/ui.json"),
            visible: true,
            follow_system_color: false,
            area: None,
            window: None,
        }
    }

    fn current_time_ms(&self) -> f64 {
        let raw = if self.playing {
            self.anchor_position
                + self.anchor_at.elapsed().as_secs_f64() * 1000.0 * self.speed
        } else {
            self.anchor_position
        };
        let clamped = raw.max(0.0);
        if self.duration_ms > 0.0 {
            clamped.min(self.duration_ms)
        } else {
            clamped
        }
    }

    fn lyric_time_ms(&self) -> f64 {
        self.current_time_ms() + self.api_offset_ms + self.cfg.lyric_offset_ms
    }

    fn redraw(&self) {
        if let Some(area) = &self.area {
            area.queue_draw();
        }
    }

    fn clamped_position(&self, x: i32, y: i32) -> (i32, i32) {
        let total = render::controls_total_width(&self.cfg.controls);
        let min_x = (total / 2).min(self.monitor_w / 2);
        // Keep the right-side drag handle on-screen even when the card is wider
        // than the visible area; the card/shadow may extend off-screen.
        let max_x = (self.monitor_w - self.card_width / 2 + self.cfg.controls.right_margin)
            .max(min_x);
        let max_y = (self.monitor_h - self.cfg.height).max(0);
        (x.clamp(min_x, max_x), y.clamp(0, max_y))
    }

    fn card_x(&self) -> i32 {
        self.win_x - self.card_width / 2
    }

    fn card_y(&self) -> i32 {
        (self.monitor_h - self.win_y - self.cfg.height)
            .clamp(0, (self.monitor_h - self.cfg.height).max(0))
    }

    fn update_card_width(&mut self) {
        let placeholder = self.lyrics.is_empty();
        let line = if placeholder {
            self.fallback_line.as_ref()
        } else {
            pick_primary_index(&self.lyrics, self.lyric_time_ms())
                .and_then(|index| self.lyrics.get(index))
        };
        let width = render::measure_card_width(&self.cfg, line, placeholder)
            .clamp(320, self.monitor_w.max(320));
        if width != self.card_width {
            self.card_width = width;
            logging::debug(&format!("card width {width}"));
            let (x, y) = self.clamped_position(self.win_x, self.win_y);
            self.win_x = x;
            self.win_y = y;
            self.redraw();
            if let Some(window) = &self.window {
                apply_input_region(
                    window,
                    &self.cfg,
                    self.card_width,
                    self.card_x(),
                    self.card_y(),
                    self.card_visible(),
                    "width",
                );
            }
        }
    }

    fn card_visible(&self) -> bool {
        self.visible && self.api_ok
    }

    fn save_ui_settings(&self) {
        let settings = UiSettings {
            visible: Some(self.visible),
            font_size: Some(self.cfg.font_size),
            font_family: Some(self.cfg.font_family.clone()),
            played_color: Some(self.cfg.played_color.clone()),
            follow_system_color: Some(self.follow_system_color),
        };
        save_ui_settings(&self.ui_settings_path, &settings);
    }

    fn save_position(&self) {
        let parent = self
            .position_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        if !parent.as_os_str().is_empty() && !parent.exists() {
            let _ = fs::create_dir_all(&parent);
        }
        let saved = SavedPosition {
            x: self.win_x,
            y: self.win_y,
        };
        match serde_json::to_string_pretty(&saved) {
            Ok(text) => {
                if let Err(err) = fs::write(&self.position_path, text) {
                    logging::warn(&format!(
                        "failed to save position to {}: {err}",
                        self.position_path.display()
                    ));
                }
            }
            Err(err) => logging::warn(&format!("failed to serialize position: {err}")),
        }
    }

    fn apply_now_playing(&mut self, np: NowPlaying, cmd_tx: &Sender<Command>) {
        let was_api_ok = self.api_ok;
        self.api_ok = true;
        self.api_error_logged = false;
        if !was_api_ok {
            logging::info("API reachable");
        }

        let old_track = self.track_id.clone();
        let old_playing = self.playing;

        self.fallback_line = if np.title.trim().is_empty() {
            None
        } else {
            Some(LyricLine {
                words: vec![LyricWord {
                    word: np.title.clone(),
                    start_time: 0.0,
                    end_time: 0.0,
                }],
                translated: np.artists.join(" / "),
                roman: String::new(),
                start_time: 0.0,
                end_time: 0.0,
                is_bg: false,
            })
        };

        let speed = np.speed;
        let delta = if np.send_timestamp_ms > 1.0e12 {
            (epoch_ms() - np.send_timestamp_ms).clamp(0.0, 5000.0)
        } else {
            0.0
        };
        let anchor = if np.playing {
            np.position_ms + delta * speed
        } else {
            np.position_ms
        };
        self.speed = speed;
        self.duration_ms = np.duration_ms;
        self.anchor_position = if np.duration_ms > 0.0 {
            anchor.clamp(0.0, np.duration_ms)
        } else {
            anchor.max(0.0)
        };
        self.anchor_at = Instant::now();
        self.playing = np.playing;
        self.api_offset_ms = np.lyric_offset_ms;
        self.lyric_available = np.lyric_available;
        self.track_id = np.track_id.clone();

        if old_track != np.track_id {
            logging::debug(&format!(
                "track changed: {:?} -> {:?} ({}) lyric_available={} lines={}",
                old_track, np.track_id, np.title, np.lyric_available, np.lyric_line_count
            ));
            self.lyrics.clear();
            self.lyrics_requested = None;
            self.lyrics_retry_at = None;
            self.lyrics_error_logged = false;
            self.last_line = None;
            if let Some(id) = &np.track_id {
                let _ = cmd_tx.send(Command::FetchLyrics(id.clone()));
                self.lyrics_requested = Some(id.clone());
            }
        } else if np.lyric_available && self.lyrics.is_empty() {
            let due = self
                .lyrics_retry_at
                .map(|t| Instant::now() >= t)
                .unwrap_or(true);
            if self.lyrics_requested.is_none() && due {
                if let Some(id) = &np.track_id {
                    let _ = cmd_tx.send(Command::FetchLyrics(id.clone()));
                    self.lyrics_requested = Some(id.clone());
                }
            }
        }

        let now = self.now.replace(np);
        if old_playing != self.playing {
            logging::debug(if self.playing {
                "playback resumed"
            } else {
                "playback paused"
            });
        }
        drop(now);
        self.redraw();
        self.update_card_width();
    }

    fn apply_lyrics(
        &mut self,
        requested_track: String,
        result: Result<api::LyricsData, String>,
    ) {
        let current = self.track_id.clone();
        if current.as_deref() != Some(requested_track.as_str()) {
            logging::debug(&format!(
                "dropping lyrics response for {requested_track} (current {current:?})"
            ));
            return;
        }
        match result {
            Ok(data) => {
                if let Some(resp_track) = &data.track_id {
                    if resp_track != &requested_track {
                        logging::debug(&format!(
                            "ignoring lyrics for {resp_track}, current {requested_track}"
                        ));
                        self.lyrics_requested = None;
                        self.lyrics_retry_at = Some(Instant::now() + Duration::from_secs(3));
                        return;
                    }
                }
                if data.lines.is_empty() {
                    logging::debug(&format!(
                        "lyrics empty for {requested_track}, will retry"
                    ));
                    self.lyrics_requested = None;
                    self.lyrics_retry_at = Some(Instant::now() + Duration::from_secs(3));
                } else {
                    logging::info(&format!(
                        "loaded {} lyric lines for track {}",
                        data.lines.len(),
                        requested_track
                    ));
                    self.lyrics = data.lines;
                    self.lyrics_retry_at = None;
                    self.lyrics_error_logged = false;
                }
                if data.lyric_offset_ms != 0.0 {
                    logging::debug(&format!(
                        "lyrics api offset {} ms",
                        data.lyric_offset_ms
                    ));
                }
                self.redraw();
                self.update_card_width();
            }
            Err(err) => {
                if !self.lyrics_error_logged {
                    logging::warn(&format!("lyrics fetch failed: {err}"));
                    self.lyrics_error_logged = true;
                } else {
                    logging::debug(&format!("lyrics fetch retry failed: {err}"));
                }
                self.lyrics_requested = None;
                self.lyrics_retry_at = Some(Instant::now() + Duration::from_secs(3));
            }
        }
    }
}

fn build_scene<'a>(state: &'a UiState) -> render::LyricScene<'a> {
    let visible = state.api_ok || !state.cfg.hide_when_disconnected;
    let lyric_time = state.lyric_time_ms();
    let placeholder = visible && state.lyrics.is_empty();
    let line = if !visible {
        None
    } else if placeholder {
        state.fallback_line.as_ref()
    } else {
        pick_primary_index(&state.lyrics, lyric_time)
            .and_then(|i| state.lyrics.get(i))
    };
    render::LyricScene {
        cfg: &state.cfg,
        line,
        time_ms: lyric_time,
        placeholder,
    }
}

fn install_css(display: &gtk4::gdk::Display) {
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(
        "window, window.splayer-lyrics, window.splayer-controls { background: transparent; border: none; box-shadow: none; }",
    );
    gtk4::style_context_add_provider_for_display(
        display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn find_monitor(display: &gtk4::gdk::Display, connector: &str) -> Option<gtk4::gdk::Monitor> {
    let monitors = display.monitors();
    for i in 0..monitors.n_items() {
        if let Some(item) = monitors.item(i) {
            if let Ok(monitor) = item.downcast::<gtk4::gdk::Monitor>() {
                if monitor
                    .connector()
                    .map(|c| c.as_str() == connector)
                    .unwrap_or(false)
                {
                    return Some(monitor);
                }
            }
        }
    }
    None
}

fn configure_layer(
    window: &gtk4::Window,
    namespace: &str,
    monitor: Option<&gtk4::gdk::Monitor>,
) {
    window.init_layer_shell();
    window.set_namespace(Some(namespace));
    window.set_layer(Layer::Overlay);
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_monitor(monitor);
}

fn apply_input_region(
    window: &gtk4::Window,
    cfg: &Config,
    card_width: i32,
    card_x: i32,
    card_y: i32,
    visible: bool,
    label: &str,
) {
    if let Some(surface) = window.surface() {
        if !visible {
            surface.set_input_region(Some(&cairo::Region::create()));
            logging::debug(&format!("input region {label}: empty (card hidden)"));
            return;
        }
        let rects = render::input_region_rects(&cfg.controls, card_width, cfg.height);
        let region = cairo::Region::create();
        for (x, y, w, h) in &rects {
            let rect = cairo::RectangleInt::new(x + card_x, y + card_y, *w, *h);
            let _ = region.union_rectangle(&rect);
        }
        surface.set_input_region(Some(&region));
        logging::debug(&format!(
            "input region {label}: {} rect(s), card=({card_x},{card_y}) (lyrics click-through)",
            rects.len()
        ));
    } else {
        logging::debug(&format!("cannot apply input region yet ({label})"));
    }
}

fn build_window(
    monitor: Option<&gtk4::gdk::Monitor>,
    ui: Rc<RefCell<UiState>>,
    cmd_tx: Sender<Command>,
) -> (gtk4::Window, gtk4::DrawingArea) {
    let (monitor_w, monitor_h) = monitor
        .map(|m| {
            let g = m.geometry();
            (g.width(), g.height())
        })
        .unwrap_or((1920, 1080));
    let window = gtk4::Window::builder()
        .decorated(false)
        .resizable(false)
        .can_focus(false)
        .default_width(monitor_w)
        .default_height(monitor_h)
        .build();
    window.add_css_class("splayer-overlay");

    let area = gtk4::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    area.set_size_request(monitor_w, monitor_h);

    let ui_draw = ui.clone();
    let last_card = std::cell::Cell::new((-1, -1, -1));
    area.set_draw_func(move |_area, cr, _width, _height| {
        let state = ui_draw.borrow();
        let card_x = state.card_x();
        let card_y = state.card_y();
        let card_w = state.card_width;
        let card_h = state.cfg.height;
        let old = last_card.get();
        if !state.card_visible() {
            if old.0 >= 0 {
                render::clear_rect(cr, old.0, old.1, old.2, card_h);
            }
            last_card.set((-1, -1, -1));
            return;
        }
        if old.0 >= 0 && old != (card_x, card_y, card_w) {
            render::clear_rect(cr, old.0, old.1, old.2, card_h);
        }
        render::clear_rect(cr, card_x, card_y, card_w, card_h);
        last_card.set((card_x, card_y, card_w));
        let scene = build_scene(&state);
        render::draw_overlay(
            cr,
            card_x,
            card_y,
            card_w,
            card_h,
            &scene,
            &state.cfg,
            state.playing,
            state.hover,
            state.hover_handle,
        );
    });

    window.set_child(Some(&area));

    configure_layer(&window, "splayer-lyrics", monitor);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);

    let gesture = gtk4::GestureClick::new();
    gesture.set_button(1);
    let ui_click = ui.clone();
    let tx_click = cmd_tx.clone();
    let window_click = window.clone();
    gesture.connect_pressed(move |_gesture, _n_press, x, y| {
        let hidden = {
            let mut state = ui_click.borrow_mut();
            let local_x = x - f64::from(state.card_x());
            let local_y = y - f64::from(state.card_y());
            if render::hide_hit(
                &state.cfg.controls,
                state.card_width,
                state.cfg.height,
                local_x,
                local_y,
            ) {
                state.visible = false;
                state.redraw();
                state.save_ui_settings();
                let region = (
                    state.cfg.clone(),
                    state.card_width,
                    state.card_x(),
                    state.card_y(),
                    false,
                );
                Some(region)
            } else {
                None
            }
        };
        if let Some(region) = hidden {
            apply_input_region(
                &window_click,
                &region.0,
                region.1,
                region.2,
                region.3,
                region.4,
                "hide-button",
            );
            return;
        }
        let action = {
            let state = ui_click.borrow();
            let card_x = f64::from(state.card_x());
            let card_y = f64::from(state.card_y());
            let hit = render::controls_hit(
                &state.cfg.controls,
                state.card_width,
                state.cfg.height,
                x - card_x,
                y - card_y,
            );
            hit.map(|index| match index {
                0 => ControlAction::Prev,
                1 => {
                    if state.playing {
                        ControlAction::Pause
                    } else {
                        ControlAction::Play
                    }
                }
                _ => ControlAction::Next,
            })
        };
        if let Some(action) = action {
            logging::debug(&format!("control clicked: {}", action.name()));
            let _ = tx_click.send(Command::Control(action));
            let resync = tx_click.clone();
            glib::timeout_add_local_once(Duration::from_millis(180), move || {
                let _ = resync.send(Command::ForcePoll);
            });
        }
    });
    area.add_controller(gesture);

    let drag = gtk4::GestureDrag::new();
    let ui_drag = ui.clone();
    drag.connect_drag_begin(move |_gesture, x, y| {
        let mut state = ui_drag.borrow_mut();
        let card_x = f64::from(state.card_x());
        let card_y = f64::from(state.card_y());
        let local_x = x - card_x;
        let local_y = y - card_y;
        let on_handle = render::handle_hit(
            &state.cfg.controls,
            state.card_width,
            state.cfg.height,
            local_x,
            local_y,
        ) || render::input_contains(
            &state.cfg.controls,
            state.card_width,
            state.cfg.height,
            local_x,
            local_y,
        );
        if on_handle {
            state.drag = Some(DragState {
                start_x: state.win_x,
                start_y: state.win_y,
            });
            logging::debug("drag started on handle");
        } else {
            state.drag = None;
        }
    });
    let ui_drag_update = ui.clone();
    let window_drag = window.clone();
    drag.connect_drag_update(move |_gesture, offset_x, offset_y| {
        let region = {
            let mut state = ui_drag_update.borrow_mut();
            let Some(drag) = state.drag else {
                return;
            };
            let (next_x, next_y) = state.clamped_position(
                drag.start_x + offset_x.round() as i32,
                drag.start_y - offset_y.round() as i32,
            );
            state.win_x = next_x;
            state.win_y = next_y;
            state.redraw();
            (state.cfg.clone(), state.card_width, state.card_x(), state.card_y(), state.card_visible())
        };
        apply_input_region(&window_drag, &region.0, region.1, region.2, region.3, region.4, "drag");
    });
    let ui_drag_end = ui.clone();
    drag.connect_drag_end(move |_gesture, _offset_x, _offset_y| {
        let mut state = ui_drag_end.borrow_mut();
        if state.drag.take().is_some() {
            state.save_position();
            logging::info(&format!(
                "position saved: x={} y={} ({})",
                state.win_x,
                state.win_y,
                state.position_path.display()
            ));
        }
    });
    area.add_controller(drag);

    let motion = gtk4::EventControllerMotion::new();
    let ui_motion = ui.clone();
    let area_motion = area.clone();
    motion.connect_motion(move |_controller, x, y| {
        let changed = {
            let mut state = ui_motion.borrow_mut();
            let local_x = x - f64::from(state.card_x());
            let local_y = y - f64::from(state.card_y());
            let control = render::controls_hit(
                &state.cfg.controls,
                state.card_width,
                state.cfg.height,
                local_x,
                local_y,
            );
            let handle = render::handle_hit(
                &state.cfg.controls,
                state.card_width,
                state.cfg.height,
                local_x,
                local_y,
            );
            let changed = state.hover != control || state.hover_handle != handle;
            state.hover = control;
            state.hover_handle = handle;
            changed
        };
        if changed {
            area_motion.queue_draw();
        }
    });
    let ui_leave = ui.clone();
    let area_leave = area.clone();
    motion.connect_leave(move |_controller| {
        let changed = {
            let mut state = ui_leave.borrow_mut();
            let changed = state.hover.is_some() || state.hover_handle;
            state.hover = None;
            state.hover_handle = false;
            changed
        };
        if changed {
            area_leave.queue_draw();
        }
    });
    area.add_controller(motion);

    let region_ui = ui.clone();
    let region_window = window.clone();
    window.connect_realize(move |_| {
        let state = region_ui.borrow();
        apply_input_region(&region_window, &state.cfg, state.card_width, state.card_x(), state.card_y(), state.card_visible(), "realize");
    });
    let region_ui = ui.clone();
    let region_window = window.clone();
    window.connect_map(move |_| {
        let state = region_ui.borrow();
        apply_input_region(&region_window, &state.cfg, state.card_width, state.card_x(), state.card_y(), state.card_visible(), "map");
    });

    (window, area)
}

fn load_position(cfg: &Config, monitor_w: i32, monitor_h: i32) -> (i32, i32) {
    let default_x = (monitor_w / 2).max(0);
    let default_y = cfg.bottom_margin;
    let path = Path::new(&cfg.position_file);
    if let Ok(text) = fs::read_to_string(path) {
        if let Ok(saved) = serde_json::from_str::<SavedPosition>(&text) {
            // Do not clamp against the initial fallback width: the actual card
            // width is measured from the current line and may be much narrower,
            // allowing a larger X range. UiState::update_card_width() re-clamps
            // once the real width is known.
            let max_x = monitor_w.max(0);
            let max_y = (monitor_h - cfg.height).max(0);
            let x = saved.x.clamp(0, max_x);
            let y = saved.y.clamp(0, max_y);
            logging::info(&format!(
                "loaded position x={x} y={y} from {}",
                path.display()
            ));
            return (x, y);
        }
        logging::warn(&format!("invalid position file {}", path.display()));
    }
    (default_x, default_y)
}


fn start_tick(ui: &Rc<RefCell<UiState>>, tick: &TickCell) {
    if tick.borrow().is_some() {
        return;
    }
    let fps = ui.borrow().cfg.fps.max(1);
    let interval = (1000 / fps).max(16);
    let ui_tick = ui.clone();
    let slot = tick.clone();
    let id = glib::timeout_add_local(Duration::from_millis(u64::from(interval)), move || {
        let (playing, area, line_changed) = {
            let mut state = ui_tick.borrow_mut();
            let index = if state.lyrics.is_empty() {
                None
            } else {
                pick_primary_index(&state.lyrics, state.lyric_time_ms())
            };
            let changed = state.last_line != index;
            if changed {
                state.last_line = index;
            }
            (state.playing, state.area.clone(), changed)
        };
        if line_changed {
            ui_tick.borrow_mut().update_card_width();
        }
        if playing {
            if let Some(area) = area {
                area.queue_draw();
            }
            glib::ControlFlow::Continue
        } else {
            *slot.borrow_mut() = None;
            glib::ControlFlow::Break
        }
    });
    *tick.borrow_mut() = Some(id);
}

fn stop_tick(tick: &TickCell) {
    if let Some(id) = tick.borrow_mut().take() {
        id.remove();
    }
}

fn main() {
    let args = parse_args();
    if let Err(err) = logging::init(std::path::Path::new("runtime/run.log"), args.debug) {
        eprintln!("failed to initialize log: {err}");
        std::process::exit(2);
    }
    let cfg = match Config::load(&args.config) {
        Ok(cfg) => cfg,
        Err(err) => {
            logging::error(&format!("config error: {err:#}"));
            eprintln!("config error: {err:#}");
            std::process::exit(2);
        }
    };
    logging::info(&format!(
        "starting splayer-lyrics (config {}, output {}, api {})",
        args.config.display(),
        cfg.output,
        cfg.base_url()
    ));

    if let Err(err) = run(cfg, &args.config) {
        logging::error(&format!("fatal: {err:#}"));
        eprintln!("fatal: {err:#}");
        std::process::exit(1);
    }
}

fn run(mut cfg: Config, config_path: &Path) -> anyhow::Result<()> {
    let ui_settings_path = PathBuf::from("runtime/ui.json");
    let ui_settings = load_ui_settings(&ui_settings_path);
    let follow_system_color = ui_settings.follow_system_color.unwrap_or(false);
    if let Some(size) = ui_settings.font_size {
        cfg.font_size = size.clamp(8, 300);
    }
    if let Some(family) = &ui_settings.font_family {
        if !family.trim().is_empty() {
            cfg.font_family = family.clone();
        }
    }
    if follow_system_color {
        if let Some(color) = system_accent_color() {
            cfg.played_color = color;
        }
    } else if let Some(color) = &ui_settings.played_color {
        cfg.played_color = color.clone();
    }

    gtk4::init().map_err(|e| anyhow::anyhow!("gtk init failed: {e}"))?;
    let display = gtk4::gdk::Display::default()
        .ok_or_else(|| anyhow::anyhow!("no Gdk display available"))?;
    install_css(&display);

    let monitor = find_monitor(&display, &cfg.output);
    match &monitor {
        Some(_m) => logging::info(&format!("using monitor connector {}", cfg.output)),
        None => logging::warn(&format!(
            "monitor {} not found, letting the compositor choose",
            cfg.output
        )),
    }
    if let Some(m) = &monitor {
        let geometry = m.geometry();
        logging::debug(&format!(
            "monitor geometry x={} y={} w={} h={}",
            geometry.x(),
            geometry.y(),
            geometry.width(),
            geometry.height()
        ));
    }

    let (ui_tx, ui_rx) = mpsc::channel::<UiMsg>();
    let tray_tx = ui_tx.clone();
    let cmd_tx = api::spawn(cfg.base_url(), cfg.api.poll_ms, ui_tx);
    let (monitor_w, monitor_h) = monitor
        .as_ref()
        .map(|m| {
            let g = m.geometry();
            (g.width(), g.height())
        })
        .unwrap_or((1920, 1080));
    let (win_x, win_y) = load_position(&cfg, monitor_w, monitor_h);
    let ui = Rc::new(RefCell::new(UiState::new(
        cfg.clone(),
        win_x,
        win_y,
        PathBuf::from(&cfg.position_file),
        monitor_w,
        monitor_h,
    )));
    {
        let mut state = ui.borrow_mut();
        state.ui_settings_path = ui_settings_path;
        state.visible = ui_settings.visible.unwrap_or(true);
        state.follow_system_color = follow_system_color;
    }
    tray::spawn(
        tray_tx,
        cfg.font_size,
        &cfg.font_family,
        follow_system_color,
        &cfg.played_color,
        &cfg.output,
    );
    let tick: TickCell = Rc::new(RefCell::new(None));

    let (window, area) = build_window(monitor.as_ref(), ui.clone(), cmd_tx.clone());
    {
        let mut state = ui.borrow_mut();
        state.area = Some(area);
        state.window = Some(window.clone());
    }

    window.present();

    let w = window.clone();
    let ui_region = ui.clone();
    glib::timeout_add_local_once(Duration::from_millis(400), move || {
        let state = ui_region.borrow();
        apply_input_region(&w, &state.cfg, state.card_width, state.card_x(), state.card_y(), state.card_visible(), "post-present");
    });

    // niri may remap/reconfigure layer surfaces when entering fullscreen. Re-apply
    // the input region defensively so the controls/handle remain interactive.
    let region_window = window.clone();
    let region_ui = ui.clone();
    glib::timeout_add_local(Duration::from_secs(1), move || {
        let state = region_ui.borrow();
        apply_input_region(
            &region_window,
            &state.cfg,
            state.card_width,
            state.card_x(),
            state.card_y(),
            state.card_visible(),
            "periodic",
        );
        glib::ControlFlow::Continue
    });

    let ui_drain = ui.clone();
    let tick_drain = tick.clone();
    let tx_drain = cmd_tx.clone();
    let window_drain = window.clone();
    let config_path_drain = config_path.to_path_buf();
    let rx = ui_rx;
    glib::timeout_add_local(Duration::from_millis(50), move || {
        loop {
            let msg = match rx.try_recv() {
                Ok(msg) => msg,
                Err(_) => break,
            };
            match msg {
                UiMsg::NowPlaying(Ok(np)) => {
                    let (playing, region) = {
                        let mut state = ui_drain.borrow_mut();
                        state.apply_now_playing(np, &tx_drain);
                        (
                            state.playing,
                            (state.cfg.clone(), state.card_width, state.card_x(), state.card_y(), state.card_visible()),
                        )
                    };
                    if playing {
                        start_tick(&ui_drain, &tick_drain);
                    } else {
                        stop_tick(&tick_drain);
                    }
                    apply_input_region(
                        &window_drain,
                        &region.0,
                        region.1,
                        region.2,
                        region.3,
                        region.4,
                        "state",
                    );
                }
                UiMsg::NowPlaying(Err(err)) => {
                    let (should_log, region) = {
                        let mut state = ui_drain.borrow_mut();
                        state.api_ok = false;
                        let first = !state.api_error_logged;
                        state.api_error_logged = true;
                        state.redraw();
                        (
                            first,
                            (
                                state.cfg.clone(),
                                state.card_width,
                                state.card_x(),
                                state.card_y(),
                                state.card_visible(),
                            ),
                        )
                    };
                    stop_tick(&tick_drain);
                    apply_input_region(
                        &window_drain,
                        &region.0,
                        region.1,
                        region.2,
                        region.3,
                        region.4,
                        "api-down",
                    );
                    if should_log {
                        logging::warn(&format!(
                            "API unreachable, hiding lyrics until it returns: {err}"
                        ));
                    } else {
                        logging::debug(&format!("API still unreachable: {err}"));
                    }
                }
                UiMsg::Lyrics {
                    requested_track,
                    result,
                } => {
                    let region = {
                        let mut state = ui_drain.borrow_mut();
                        state.apply_lyrics(requested_track, result);
                        (state.cfg.clone(), state.card_width, state.card_x(), state.card_y(), state.card_visible())
                    };
                    apply_input_region(
                        &window_drain,
                        &region.0,
                        region.1,
                        region.2,
                        region.3,
                        region.4,
                        "lyrics",
                    );
                }
                UiMsg::ControlDone { action, ok, error } => {
                    if ok {
                        logging::debug(&format!("control {action} accepted"));
                    } else {
                        logging::warn(&format!(
                            "control {action} failed: {}",
                            error.unwrap_or_default()
                        ));
                    }
                }
                UiMsg::ToggleVisible => {
                    let region = {
                        let mut state = ui_drain.borrow_mut();
                        state.visible = !state.visible;
                        state.redraw();
                        state.save_ui_settings();
                        (
                            state.cfg.clone(),
                            state.card_width,
                            state.card_x(),
                            state.card_y(),
                            state.card_visible(),
                        )
                    };
                    apply_input_region(
                        &window_drain,
                        &region.0,
                        region.1,
                        region.2,
                        region.3,
                        region.4,
                        "toggle",
                    );
                }
                UiMsg::SetFontSize(size) => {
                    let region = {
                        let mut state = ui_drain.borrow_mut();
                        state.cfg.font_size = size.clamp(8, 300);
                        state.update_card_width();
                        state.redraw();
                        state.save_ui_settings();
                        (
                            state.cfg.clone(),
                            state.card_width,
                            state.card_x(),
                            state.card_y(),
                            state.card_visible(),
                        )
                    };
                    apply_input_region(
                        &window_drain,
                        &region.0,
                        region.1,
                        region.2,
                        region.3,
                        region.4,
                        "font-size",
                    );
                }
                UiMsg::SetFontFamily(family) => {
                    let region = {
                        let mut state = ui_drain.borrow_mut();
                        if !family.trim().is_empty() {
                            state.cfg.font_family = family;
                        }
                        state.update_card_width();
                        state.redraw();
                        state.save_ui_settings();
                        (
                            state.cfg.clone(),
                            state.card_width,
                            state.card_x(),
                            state.card_y(),
                            state.card_visible(),
                        )
                    };
                    apply_input_region(
                        &window_drain,
                        &region.0,
                        region.1,
                        region.2,
                        region.3,
                        region.4,
                        "font-family",
                    );
                }
                UiMsg::SetColor(color) => {
                    let region = {
                        let mut state = ui_drain.borrow_mut();
                        state.cfg.played_color = color;
                        state.follow_system_color = false;
                        state.redraw();
                        state.save_ui_settings();
                        (
                            state.cfg.clone(),
                            state.card_width,
                            state.card_x(),
                            state.card_y(),
                            state.card_visible(),
                        )
                    };
                    apply_input_region(
                        &window_drain,
                        &region.0,
                        region.1,
                        region.2,
                        region.3,
                        region.4,
                        "color",
                    );
                }
                UiMsg::FollowSystemColor => {
                    let region = {
                        let mut state = ui_drain.borrow_mut();
                        if let Some(color) = system_accent_color() {
                            state.cfg.played_color = color;
                            state.follow_system_color = true;
                        }
                        state.redraw();
                        state.save_ui_settings();
                        (
                            state.cfg.clone(),
                            state.card_width,
                            state.card_x(),
                            state.card_y(),
                            state.card_visible(),
                        )
                    };
                    apply_input_region(
                        &window_drain,
                        &region.0,
                        region.1,
                        region.2,
                        region.3,
                        region.4,
                        "system-color",
                    );
                }
                UiMsg::SetOutput(output) => {
                    restart_with_output(&config_path_drain, &output);
                }
                UiMsg::Quit => std::process::exit(0),
            }
        }
        glib::ControlFlow::Continue
    });

    let _ = cmd_tx.send(Command::ForcePoll);

    // Keep following the desktop theme while the user selected "follow system color".
    let theme_ui = ui.clone();
    glib::timeout_add_seconds_local(5, move || {
        let mut state = theme_ui.borrow_mut();
        if state.follow_system_color {
            if let Some(color) = system_accent_color() {
                if color != state.cfg.played_color {
                    logging::info(&format!("system accent color changed to {color}"));
                    state.cfg.played_color = color;
                    state.redraw();
                }
            }
        }
        glib::ControlFlow::Continue
    });

    let main_loop = glib::MainLoop::new(None, false);
    main_loop.run();

    let _ = &window;
    Ok(())
}
