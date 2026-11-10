mod api;
mod config;
mod interaction;
mod logging;
mod model;
mod render;
mod tray;

use crate::api::{Command, ControlAction, UiMsg};
use crate::config::{Config, ControlsConfig, WidthMode, LYRIC_OFFSET_LIMIT_MS};
use crate::interaction::ClickTarget;
use crate::model::{pick_primary_index, LyricLine, LyricWord, NowPlaying};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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
    #[serde(default)]
    width: Option<i32>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct UiSettings {
    visible: Option<bool>,
    font_size: Option<i32>,
    font_family: Option<String>,
    played_color: Option<String>,
    follow_system_color: Option<bool>,
    width_mode: Option<String>,
    lyric_offset_ms: Option<f64>,
    show_translation: Option<bool>,
    show_romanization: Option<bool>,
    /// Connector the overlay was last placed on. Used for adaptive monitor
    /// selection; `config.json`'s `output` only seeds the very first run.
    last_output: Option<String>,
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

/// Write through a sibling temp file and rename, so a process crash cannot
/// leave a truncated state file behind. Symlinked state files (e.g. dotfiles)
/// are resolved first so the rename updates the target instead of replacing
/// the link, and the existing file mode is preserved.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let temp = target.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let permissions = fs::metadata(&target).ok().map(|meta| meta.permissions());
    let result = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(contents.as_bytes())?;
        if let Some(permissions) = permissions {
            fs::set_permissions(&temp, permissions)?;
        }
        fs::rename(&temp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn save_ui_settings(path: &Path, settings: &UiSettings) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(settings) {
        if let Err(err) = write_atomic(path, &text) {
            logging::warn(&format!(
                "failed to save ui settings to {}: {err}",
                path.display()
            ));
        }
    }
}

/// Everything needed to recompute the input region without holding a borrow
/// on `UiState`. Only the controls config matters to layout, not the whole
/// config, so message handlers can cheaply snapshot it.
#[derive(Clone)]
struct InputRegionSpec {
    controls: ControlsConfig,
    card_height: i32,
    card_width: i32,
    card_x: i32,
    card_y: i32,
    visible: bool,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Small clock differences are normal polling latency. Re-anchoring on every
/// poll makes the karaoke fill twitch; only correct when the local clock has
/// drifted far enough to matter.
const LOCAL_CLOCK_RESYNC_MS: f64 = 120.0;

fn choose_anchor(local_ms: f64, reported_ms: f64, trust_local: bool) -> f64 {
    if trust_local && (local_ms - reported_ms).abs() < LOCAL_CLOCK_RESYNC_MS {
        local_ms
    } else {
        reported_ms
    }
}

/// Returns the new card center X after a width change, preserving the edge
/// selected by `mode`. `card_x = win_x - width / 2` in the rest of the code.
fn next_win_x(mode: WidthMode, old_win_x: i32, old_width: i32, new_width: i32) -> i32 {
    let old_card_x = old_win_x - old_width / 2;
    match mode {
        WidthMode::Center => old_win_x,
        // Grow/shrink towards the left: the right edge stays fixed.
        WidthMode::Left => old_card_x + old_width - new_width + new_width / 2,
        // Grow/shrink towards the right: the left edge stays fixed.
        WidthMode::Right => old_card_x + new_width / 2,
    }
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
                    write_atomic(path, &updated)?;
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
    write_atomic(path, &serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

/// start.sh may live next to the config (normal repo checkout) or in the
/// launch directory. Returns the directory that contains the executable
/// launcher, if any.
fn find_start_dir(config_path: &Path) -> Option<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = config_path
        .canonicalize()
        .unwrap_or_else(|_| cwd.join(config_path));
    let candidates = [config.parent().map(Path::to_path_buf), Some(cwd)];
    candidates.into_iter().flatten().find(|dir| {
        fs::metadata(dir.join("start.sh"))
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// Spawn a detached shell that waits for this process to exit, then runs
/// start.sh. The caller still owns the decision to exit.
fn launch_restart(start_dir: &Path, config_path: &Path) -> std::io::Result<()> {
    let pid = std::process::id();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = config_path
        .canonicalize()
        .unwrap_or_else(|_| cwd.join(config_path));
    let script = format!(
        "while kill -0 {pid} 2>/dev/null; do sleep 0.1; done; cd {} && ./start.sh --config {}",
        shell_quote(&start_dir.display().to_string()),
        shell_quote(&config.display().to_string())
    );

    let mut command = ProcessCommand::new("sh");
    command
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(start_dir.join("runtime/run.log"))
    {
        Ok(log) => {
            command.stderr(Stdio::from(log));
        }
        Err(_) => {
            command.stderr(Stdio::null());
        }
    }
    command.spawn().map(|_| ())
}

/// Writes the new output and restarts this process via start.sh once the old
/// process has exited. A full restart is much simpler and safer than tearing
/// down and rebuilding the layer surface in place.
fn restart_with_output(config_path: &Path, output: &str) {
    let Some(start_dir) = find_start_dir(config_path) else {
        logging::error("cannot restart overlay: start.sh not found next to config or in cwd");
        return;
    };

    if let Err(err) = set_config_output(config_path, output) {
        logging::error(&format!(
            "cannot update output in {}: {err:#}",
            config_path.display()
        ));
        return;
    }

    match launch_restart(&start_dir, config_path) {
        Ok(()) => {
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
    hover_hide: bool,
    pointer_x: Option<f64>,
    pointer_y: Option<f64>,
    last_width_fingerprint: Option<u64>,
    win_x: i32,
    win_y: i32,
    card_width: i32,
    width_initialized: bool,
    position_dirty: bool,
    pointer: interaction::PointerState,
    cursor: Option<interaction::CursorShape>,
    monitor_w: i32,
    monitor_h: i32,
    position_path: PathBuf,
    ui_settings_path: PathBuf,
    visible: bool,
    follow_system_color: bool,
    last_output: Option<String>,
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
            hover_hide: false,
            pointer_x: None,
            pointer_y: None,
            last_width_fingerprint: None,
            win_x,
            win_y,
            card_width,
            width_initialized: false,
            position_dirty: false,
            pointer: interaction::PointerState::default(),
            cursor: None,
            monitor_w,
            monitor_h,
            position_path,
            ui_settings_path: PathBuf::from("runtime/ui.json"),
            visible: true,
            follow_system_color: false,
            last_output: None,
            area: None,
            window: None,
        }
    }

    fn current_time_ms(&self) -> f64 {
        let raw = if self.playing {
            self.anchor_position + self.anchor_at.elapsed().as_secs_f64() * 1000.0 * self.speed
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
        let max_x =
            (self.monitor_w - self.card_width / 2 + self.cfg.controls.right_margin).max(min_x);
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

    /// Returns whether the measured width changed, so the tick loop can
    /// refresh the input region. Message handlers run through `update_ui`,
    /// which applies the region once after the mutation.
    fn update_card_width(&mut self) -> bool {
        // Do not re-anchor while the pointer is down: the drag gesture keeps
        // its press-time start position, so an external win_x change would
        // make the card snap back on the next drag update.
        if self.pointer.is_armed() {
            return false;
        }
        let placeholder = self.lyrics.is_empty();
        let line = if placeholder {
            self.fallback_line.as_ref()
        } else {
            pick_primary_index(&self.lyrics, self.lyric_time_ms())
                .and_then(|index| self.lyrics.get(index))
        };
        let fingerprint = width_fingerprint(&self.cfg, line, placeholder, self.monitor_w);
        if self.last_width_fingerprint == Some(fingerprint) {
            return false;
        }
        let Some(width) = render::measure_card_width(&self.cfg, line) else {
            // Keep the old fingerprint so the next poll retries.
            return false;
        };
        self.last_width_fingerprint = Some(fingerprint);
        let width = width.clamp(320, self.monitor_w.max(320));
        if width != self.card_width {
            let old_width = self.card_width;
            let old_win_x = self.win_x;
            self.card_width = width;
            // position.json stores the width too; keep it current even when
            // the center (center mode) does not move.
            self.position_dirty = true;
            if self.width_initialized {
                self.win_x = next_win_x(self.cfg.width_mode, old_win_x, old_width, width);
            } else {
                self.width_initialized = true;
            }
            logging::debug(&format!(
                "card width {width} mode={}",
                self.cfg.width_mode.as_str()
            ));
            let (x, y) = self.clamped_position(self.win_x, self.win_y);
            self.win_x = x;
            self.win_y = y;
            self.redraw();
            true
        } else {
            if !self.width_initialized {
                self.width_initialized = true;
            }
            false
        }
    }

    fn card_visible(&self) -> bool {
        self.visible && (self.api_ok || !self.cfg.hide_when_disconnected)
    }

    fn input_region_spec(&self) -> InputRegionSpec {
        InputRegionSpec {
            controls: self.cfg.controls.clone(),
            card_height: self.cfg.height,
            card_width: self.card_width,
            card_x: self.card_x(),
            card_y: self.card_y(),
            visible: self.card_visible(),
        }
    }

    fn refresh_input_region(&self, label: &str) {
        if let Some(window) = &self.window {
            apply_input_region(window, &self.input_region_spec(), label);
        }
    }

    fn save_ui_settings(&self) {
        let settings = UiSettings {
            visible: Some(self.visible),
            font_size: Some(self.cfg.font_size),
            font_family: Some(self.cfg.font_family.clone()),
            played_color: Some(self.cfg.played_color.clone()),
            follow_system_color: Some(self.follow_system_color),
            width_mode: Some(self.cfg.width_mode.as_str().to_string()),
            lyric_offset_ms: Some(self.cfg.lyric_offset_ms),
            show_translation: Some(self.cfg.show_translation),
            show_romanization: Some(self.cfg.show_romanization),
            last_output: self.last_output.clone(),
        };
        save_ui_settings(&self.ui_settings_path, &settings);
    }

    fn save_position(&mut self) {
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
            width: Some(self.card_width),
        };
        match serde_json::to_string_pretty(&saved) {
            Ok(text) => match write_atomic(&self.position_path, &text) {
                Ok(()) => self.position_dirty = false,
                Err(err) => {
                    self.position_dirty = true;
                    logging::warn(&format!(
                        "failed to save position to {}: {err}",
                        self.position_path.display()
                    ));
                }
            },
            Err(err) => {
                self.position_dirty = true;
                logging::warn(&format!("failed to serialize position: {err}"));
            }
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
        let reported = if np.playing {
            np.position_ms + delta * speed
        } else {
            np.position_ms
        };
        let trust_local = was_api_ok
            && self.playing
            && np.playing
            && old_track == np.track_id
            && np.duration_ms > 0.0
            && (self.duration_ms - np.duration_ms).abs() < 1.0
            && (self.speed - speed).abs() < 1e-6;
        let anchor = if trust_local {
            choose_anchor(self.current_time_ms(), reported, true)
        } else {
            reported
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

    fn apply_lyrics(&mut self, requested_track: String, result: Result<api::LyricsData, String>) {
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
                    logging::debug(&format!("lyrics empty for {requested_track}, will retry"));
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
                    logging::debug(&format!("lyrics api offset {} ms", data.lyric_offset_ms));
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

/// Identity of everything that affects the measured card width. The API poll
/// runs every 900 ms but the current line and style rarely change, so this
/// avoids rebuilding Pango layouts and a temporary cairo surface each poll.
/// A collision would only keep one stale width until the next line/style
/// change; that is accepted for this in-process fingerprint.
fn width_fingerprint(
    cfg: &Config,
    line: Option<&LyricLine>,
    placeholder: bool,
    monitor_w: i32,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    placeholder.hash(&mut hasher);
    monitor_w.hash(&mut hasher);
    cfg.font_family.hash(&mut hasher);
    cfg.font_size.hash(&mut hasher);
    cfg.font_weight.hash(&mut hasher);
    cfg.translation_size_delta.hash(&mut hasher);
    cfg.show_translation.hash(&mut hasher);
    cfg.show_romanization.hash(&mut hasher);
    cfg.controls.size.hash(&mut hasher);
    cfg.controls.right_margin.hash(&mut hasher);
    cfg.background.padding_x.hash(&mut hasher);
    if let Some(line) = line {
        for word in &line.words {
            word.word.hash(&mut hasher);
        }
        line.translated.hash(&mut hasher);
        line.roman.hash(&mut hasher);
    }
    hasher.finish()
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
        pick_primary_index(&state.lyrics, lyric_time).and_then(|i| state.lyrics.get(i))
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

/// Human-readable connector list for the tray menu. Values stay connector
/// names so `config.json` remains portable; labels include the resolution to
/// distinguish duplicate connectors on hotplug docks.
fn monitor_options(display: &gtk4::gdk::Display, current: &str) -> Vec<(String, String)> {
    let mut outputs = Vec::new();
    let monitors = display.monitors();
    for i in 0..monitors.n_items() {
        let Some(item) = monitors.item(i) else {
            continue;
        };
        let Ok(monitor) = item.downcast::<gtk4::gdk::Monitor>() else {
            continue;
        };
        let Some(connector) = monitor.connector() else {
            continue;
        };
        if connector.as_str().trim().is_empty() {
            continue;
        }
        let geometry = monitor.geometry();
        outputs.push((
            format!(
                "{} ({}×{})",
                connector.as_str(),
                geometry.width(),
                geometry.height()
            ),
            connector.to_string(),
        ));
    }
    if !outputs.iter().any(|(_, value)| value == current) {
        outputs.insert(0, (format!("{current}（当前配置）"), current.to_string()));
    }
    outputs
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorCandidate {
    connector: Option<String>,
    width: i32,
    height: i32,
}

/// Pick a monitor index. An explicitly remembered connector wins; otherwise
/// the largest logical resolution is used, with the earliest entry winning
/// ties so restarts stay deterministic.
fn choose_monitor_index(candidates: &[MonitorCandidate], preferred: Option<&str>) -> Option<usize> {
    if let Some(preferred) = preferred.map(str::trim).filter(|value| !value.is_empty()) {
        if let Some(index) = candidates
            .iter()
            .position(|candidate| candidate.connector.as_deref() == Some(preferred))
        {
            return Some(index);
        }
    }

    let mut best: Option<usize> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.width <= 0 || candidate.height <= 0 {
            continue;
        }
        let better = match best {
            None => true,
            Some(current) => {
                let current = &candidates[current];
                let area = i64::from(candidate.width) * i64::from(candidate.height);
                let best_area = i64::from(current.width) * i64::from(current.height);
                area > best_area
                    || (area == best_area
                        && (candidate.width > current.width
                            || (candidate.width == current.width
                                && candidate.height > current.height)))
            }
        };
        if better {
            best = Some(index);
        }
    }
    best.or_else(|| (!candidates.is_empty()).then_some(0))
}

fn monitor_candidates(
    display: &gtk4::gdk::Display,
) -> (Vec<MonitorCandidate>, Vec<gtk4::gdk::Monitor>) {
    let monitors = display.monitors();
    let mut candidates = Vec::new();
    let mut handles = Vec::new();
    for i in 0..monitors.n_items() {
        let Some(item) = monitors.item(i) else {
            continue;
        };
        let Ok(monitor) = item.downcast::<gtk4::gdk::Monitor>() else {
            continue;
        };
        let geometry = monitor.geometry();
        candidates.push(MonitorCandidate {
            connector: monitor.connector().map(|connector| connector.to_string()),
            width: geometry.width(),
            height: geometry.height(),
        });
        handles.push(monitor);
    }
    (candidates, handles)
}

/// Select the output for this run. `last_output` (runtime/ui.json) wins so a
/// hotplug restart returns to the screen the user last had the overlay on.
/// `config.json`'s `output` is only the seed for the first run.
fn select_monitor(
    display: &gtk4::gdk::Display,
    last_output: Option<&str>,
    config_output: &str,
) -> (Option<gtk4::gdk::Monitor>, Option<String>) {
    let (candidates, handles) = monitor_candidates(display);
    let configured = config_output.trim();
    let preferred = last_output
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            (!configured.is_empty() && !configured.eq_ignore_ascii_case("auto"))
                .then_some(configured)
        });
    let index = choose_monitor_index(&candidates, preferred);
    let selected = index.and_then(|index| handles.get(index).cloned());
    let connector = index.and_then(|index| candidates[index].connector.clone());
    (selected, connector)
}

/// Geometry fingerprint used to notice hotplug and mode changes. Logical
/// sizes are enough: a scale-only change does not require a new surface.
fn monitor_snapshot(display: &gtk4::gdk::Display) -> Vec<(String, i32, i32)> {
    let monitors = display.monitors();
    let mut snapshot = Vec::with_capacity(monitors.n_items() as usize);
    for i in 0..monitors.n_items() {
        let Some(item) = monitors.item(i) else {
            continue;
        };
        let Ok(monitor) = item.downcast::<gtk4::gdk::Monitor>() else {
            continue;
        };
        let geometry = monitor.geometry();
        let name = monitor
            .connector()
            .map(|connector| connector.to_string())
            .unwrap_or_else(|| format!("<monitor-{i}>"));
        snapshot.push((name, geometry.width(), geometry.height()));
    }
    snapshot.sort();
    snapshot
}

struct MonitorWatch {
    baseline: Vec<(String, i32, i32)>,
    handlers: Vec<(gtk4::gdk::Monitor, glib::SignalHandlerId)>,
    pending: Option<glib::SourceId>,
    restarting: bool,
}

#[derive(Clone)]
struct MonitorWatchCtx {
    display: gtk4::gdk::Display,
    monitors: gtk4::gio::ListModel,
    ui: Rc<RefCell<UiState>>,
    config_path: PathBuf,
    start_dir: PathBuf,
    watch: Rc<RefCell<MonitorWatch>>,
}

impl MonitorWatchCtx {
    fn attach_invalidates(&self) {
        let mut watch = self.watch.borrow_mut();
        for (monitor, handler) in watch.handlers.drain(..) {
            monitor.disconnect(handler);
        }
        for i in 0..self.monitors.n_items() {
            let Some(item) = self.monitors.item(i) else {
                continue;
            };
            let Ok(monitor) = item.downcast::<gtk4::gdk::Monitor>() else {
                continue;
            };
            let ctx = self.clone();
            let handler = monitor.connect_invalidate(move |_| ctx.schedule());
            watch.handlers.push((monitor, handler));
        }
    }

    /// Coalesce the burst of signals a hotplug produces before acting.
    fn schedule(&self) {
        let mut watch = self.watch.borrow_mut();
        if watch.restarting {
            return;
        }
        if let Some(pending) = watch.pending.take() {
            pending.remove();
        }
        let ctx = self.clone();
        let pending = glib::timeout_add_local_once(Duration::from_millis(350), move || {
            ctx.watch.borrow_mut().pending = None;
            ctx.check();
        });
        watch.pending = Some(pending);
    }

    fn check(&self) {
        if self.watch.borrow().restarting {
            return;
        }
        let current = monitor_snapshot(&self.display);
        let previous = self.watch.borrow().baseline.clone();
        if current == previous {
            return;
        }
        if current.is_empty() {
            logging::warn("all monitors disappeared; keeping the overlay running");
            self.watch.borrow_mut().baseline = current;
            return;
        }

        {
            let mut state = self.ui.borrow_mut();
            if state.position_dirty {
                state.save_position();
            }
        }
        logging::info(&format!(
            "monitor layout changed: {previous:?} -> {current:?}; restarting overlay"
        ));
        self.watch.borrow_mut().restarting = true;
        match launch_restart(&self.start_dir, &self.config_path) {
            Ok(()) => std::process::exit(0),
            Err(err) => {
                logging::error(&format!(
                    "cannot restart overlay after monitor change: {err}"
                ));
                let mut watch = self.watch.borrow_mut();
                watch.restarting = false;
                watch.baseline = current;
            }
        }
    }
}

/// Restart on any monitor layout change so the layer surface is rebuilt at
/// the new size and the tray monitor list is refreshed. The restart picks
/// `last_output` from runtime/ui.json.
fn start_monitor_watch(
    display: &gtk4::gdk::Display,
    ui: Rc<RefCell<UiState>>,
    config_path: &Path,
    baseline: Vec<(String, i32, i32)>,
) {
    let Some(start_dir) = find_start_dir(config_path) else {
        logging::warn("monitor watch disabled: start.sh not found");
        return;
    };
    let watch = Rc::new(RefCell::new(MonitorWatch {
        baseline,
        handlers: Vec::new(),
        pending: None,
        restarting: false,
    }));
    let ctx = MonitorWatchCtx {
        display: display.clone(),
        monitors: display.monitors(),
        ui,
        config_path: config_path.to_path_buf(),
        start_dir,
        watch,
    };

    let ctx_items = ctx.clone();
    ctx.monitors.connect_items_changed(move |_, _, _, _| {
        ctx_items.attach_invalidates();
        ctx_items.schedule();
    });
    ctx.attach_invalidates();

    // Catch a change that happened while the window was being built.
    if monitor_snapshot(display) != ctx.watch.borrow().baseline {
        ctx.schedule();
    }
}

fn configure_layer(window: &gtk4::Window, namespace: &str, monitor: Option<&gtk4::gdk::Monitor>) {
    window.init_layer_shell();
    window.set_namespace(Some(namespace));
    window.set_layer(Layer::Overlay);
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_monitor(monitor);
}

fn apply_input_region(window: &gtk4::Window, spec: &InputRegionSpec, label: &str) {
    if let Some(surface) = window.surface() {
        if !spec.visible {
            surface.set_input_region(Some(&cairo::Region::create()));
            logging::debug(&format!("input region {label}: empty (card hidden)"));
            return;
        }
        let rects = render::input_region_rects(&spec.controls, spec.card_width, spec.card_height);
        let region = cairo::Region::create();
        for (x, y, w, h) in &rects {
            let rect = cairo::RectangleInt::new(x + spec.card_x, y + spec.card_y, *w, *h);
            let _ = region.union_rectangle(&rect);
        }
        surface.set_input_region(Some(&region));
        logging::debug(&format!(
            "input region {label}: {} rect(s), card=({},{}) (lyrics click-through)",
            rects.len(),
            spec.card_x,
            spec.card_y
        ));
    } else {
        logging::debug(&format!("cannot apply input region yet ({label})"));
    }
}

/// Apply one state mutation on the GTK thread and refresh the layer-surface
/// input region from the resulting state. Centralising this avoids the
/// borrow/mutate/snapshot/apply dance in every `UiMsg` arm.
fn update_ui<R>(
    ui: &Rc<RefCell<UiState>>,
    window: &gtk4::Window,
    label: &str,
    update: impl FnOnce(&mut UiState) -> R,
) -> R {
    let (result, spec) = {
        let mut state = ui.borrow_mut();
        let result = update(&mut state);
        (result, state.input_region_spec())
    };
    apply_input_region(window, &spec, label);
    result
}

/// Only call `set_cursor_from_name` when the shape actually changes; motion
/// events are frequent and each call would allocate a new GdkCursor.
fn refresh_cursor(area: &gtk4::DrawingArea, ui: &Rc<RefCell<UiState>>) {
    let (shape, changed) = {
        let mut state = ui.borrow_mut();
        let shape = interaction::cursor_for(
            state.hover,
            state.hover_handle,
            state.hover_hide,
            state.pointer.is_moving(),
        );
        let changed = state.cursor != Some(shape);
        state.cursor = Some(shape);
        (shape, changed)
    };
    if changed {
        area.set_cursor_from_name(shape.name());
    }
}

fn refresh_cursor_for(ui: &Rc<RefCell<UiState>>) {
    let area = ui.borrow().area.clone();
    if let Some(area) = area {
        refresh_cursor(&area, ui);
    }
}

/// Store the pointer position and recompute hover from the current card
/// geometry. Skipped while moving: the drag end recomputes it once the card
/// has settled.
fn update_hover(area: &gtk4::DrawingArea, ui: &Rc<RefCell<UiState>>, x: f64, y: f64) {
    let changed = {
        let mut state = ui.borrow_mut();
        state.pointer_x = Some(x);
        state.pointer_y = Some(y);
        if state.pointer.is_moving() {
            false
        } else {
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
            let hide = render::hide_hit(
                &state.cfg.controls,
                state.card_width,
                state.cfg.height,
                local_x,
                local_y,
            );
            let changed =
                state.hover != control || state.hover_handle != handle || state.hover_hide != hide;
            state.hover = control;
            state.hover_handle = handle;
            state.hover_hide = hide;
            changed
        }
    };
    if changed {
        area.queue_draw();
    }
    refresh_cursor(area, ui);
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
            render::CardRect {
                x: card_x,
                y: card_y,
                width: card_w,
                height: card_h,
            },
            &scene,
            &state.cfg,
            state.playing,
            render::HoverState {
                control: state.hover,
                handle: state.hover_handle,
                hide: state.hover_hide,
            },
        );
    });

    window.set_child(Some(&area));

    configure_layer(&window, "splayer-lyrics", monitor);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);

    let gesture = gtk4::GestureClick::new();
    gesture.set_button(1);

    let ui_pressed = ui.clone();
    gesture.connect_pressed(move |_gesture, _n_press, x, y| {
        let mut state = ui_pressed.borrow_mut();
        let target = render::press_target(
            &state.cfg.controls,
            state.card_width,
            state.cfg.height,
            x - f64::from(state.card_x()),
            y - f64::from(state.card_y()),
        );
        state.pointer.pressed(target);
    });

    let ui_released = ui.clone();
    let tx_released = cmd_tx.clone();
    let window_released = window.clone();
    let area_released = area.clone();
    gesture.connect_released(move |_gesture, _n_press, _x, _y| {
        let Some(target) = ui_released.borrow_mut().pointer.release() else {
            return;
        };
        match target {
            ClickTarget::Hide => {
                let spec = {
                    let mut state = ui_released.borrow_mut();
                    state.visible = false;
                    state.pointer.cancel();
                    state.hover = None;
                    state.hover_handle = false;
                    state.hover_hide = false;
                    state.redraw();
                    state.save_ui_settings();
                    let mut spec = state.input_region_spec();
                    spec.visible = false;
                    spec
                };
                refresh_cursor(&area_released, &ui_released);
                apply_input_region(&window_released, &spec, "hide-button");
            }
            ClickTarget::Prev | ClickTarget::PlayPause | ClickTarget::Next => {
                let action = {
                    let state = ui_released.borrow();
                    match target {
                        ClickTarget::Prev => ControlAction::Prev,
                        ClickTarget::PlayPause => {
                            if state.playing {
                                ControlAction::Pause
                            } else {
                                ControlAction::Play
                            }
                        }
                        _ => ControlAction::Next,
                    }
                };
                logging::debug(&format!("control clicked: {}", action.name()));
                let _ = tx_released.send(Command::Control(action));
                let resync = tx_released.clone();
                glib::timeout_add_local_once(Duration::from_millis(180), move || {
                    let _ = resync.send(Command::ForcePoll);
                });
            }
        }
    });

    let ui_click_stopped = ui.clone();
    gesture.connect_stopped(move |_gesture| {
        ui_click_stopped.borrow_mut().pointer.stopped();
    });
    let ui_click_cancel = ui.clone();
    gesture.connect_cancel(move |_gesture, _sequence| {
        ui_click_cancel.borrow_mut().pointer.cancel_click();
    });
    area.add_controller(gesture);

    let drag = gtk4::GestureDrag::new();
    drag.set_button(1);

    let ui_drag = ui.clone();
    drag.connect_drag_begin(move |_gesture, x, y| {
        let mut state = ui_drag.borrow_mut();
        let target = render::press_target(
            &state.cfg.controls,
            state.card_width,
            state.cfg.height,
            x - f64::from(state.card_x()),
            y - f64::from(state.card_y()),
        );
        let (win_x, win_y) = (state.win_x, state.win_y);
        state.pointer.drag_begin(target, win_x, win_y);
    });

    let ui_drag_update = ui.clone();
    let window_drag = window.clone();
    let area_drag = area.clone();
    drag.connect_drag_update(move |_gesture, offset_x, offset_y| {
        let (region, became_moving) = {
            let mut state = ui_drag_update.borrow_mut();
            let was_moving = state.pointer.is_moving();
            let effect = state.pointer.drag_update(offset_x, offset_y);
            let moving = state.pointer.is_moving();
            let region = match effect {
                interaction::DragUpdate::Move { x, y } => {
                    let (next_x, next_y) = state.clamped_position(x, y);
                    state.win_x = next_x;
                    state.win_y = next_y;
                    // Persist even if the drag is cancelled before drag-end.
                    state.position_dirty = true;
                    state.redraw();
                    Some(state.input_region_spec())
                }
                interaction::DragUpdate::Ignored | interaction::DragUpdate::Blocked => None,
            };
            (region, !was_moving && moving)
        };
        if became_moving {
            refresh_cursor(&area_drag, &ui_drag_update);
        }
        if let Some(spec) = region {
            apply_input_region(&window_drag, &spec, "drag");
        }
    });

    let ui_drag_end = ui.clone();
    let area_drag_end = area.clone();
    drag.connect_drag_end(move |_gesture, _offset_x, _offset_y| {
        let last_pointer = {
            let mut state = ui_drag_end.borrow_mut();
            let moved = state.pointer.drag_end();
            let last_pointer = state.pointer_x.zip(state.pointer_y);
            // Width updates were deferred while the pointer was down.
            if state.update_card_width() {
                state.refresh_input_region("drag-end");
            }
            if moved {
                state.save_position();
                logging::info(&format!(
                    "position saved: x={} y={} ({})",
                    state.win_x,
                    state.win_y,
                    state.position_path.display()
                ));
            }
            last_pointer
        };
        if let Some((x, y)) = last_pointer {
            update_hover(&area_drag_end, &ui_drag_end, x, y);
        } else {
            refresh_cursor(&area_drag_end, &ui_drag_end);
        }
    });

    let ui_drag_cancel = ui.clone();
    let area_drag_cancel = area.clone();
    drag.connect_cancel(move |_gesture, _sequence| {
        let last_pointer = {
            let mut state = ui_drag_cancel.borrow_mut();
            state.pointer.cancel();
            let last_pointer = state.pointer_x.zip(state.pointer_y);
            state.hover = None;
            state.hover_handle = false;
            state.hover_hide = false;
            if state.update_card_width() {
                state.refresh_input_region("drag-cancel");
            }
            state.redraw();
            last_pointer
        };
        if let Some((x, y)) = last_pointer {
            update_hover(&area_drag_cancel, &ui_drag_cancel, x, y);
        } else {
            refresh_cursor(&area_drag_cancel, &ui_drag_cancel);
        }
    });
    area.add_controller(drag);

    let motion = gtk4::EventControllerMotion::new();
    let ui_motion = ui.clone();
    let area_motion = area.clone();
    motion.connect_motion(move |_controller, x, y| {
        update_hover(&area_motion, &ui_motion, x, y);
    });
    let ui_enter = ui.clone();
    let area_enter = area.clone();
    motion.connect_enter(move |_controller, x, y| {
        update_hover(&area_enter, &ui_enter, x, y);
    });
    let ui_leave = ui.clone();
    let area_leave = area.clone();
    motion.connect_leave(move |_controller| {
        let changed = {
            let mut state = ui_leave.borrow_mut();
            state.pointer_x = None;
            state.pointer_y = None;
            let changed = state.hover.is_some() || state.hover_handle || state.hover_hide;
            state.hover = None;
            state.hover_handle = false;
            state.hover_hide = false;
            changed
        };
        if changed {
            area_leave.queue_draw();
        }
        refresh_cursor(&area_leave, &ui_leave);
    });
    area.add_controller(motion);

    let region_ui = ui.clone();
    let region_window = window.clone();
    window.connect_realize(move |_| {
        let spec = region_ui.borrow().input_region_spec();
        apply_input_region(&region_window, &spec, "realize");
    });
    let region_ui = ui.clone();
    let region_window = window.clone();
    window.connect_map(move |_| {
        let spec = region_ui.borrow().input_region_spec();
        apply_input_region(&region_window, &spec, "map");
    });

    (window, area)
}

fn load_position(cfg: &Config, monitor_w: i32, monitor_h: i32) -> (i32, i32, Option<i32>) {
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
            let width = saved
                .width
                .map(|value| value.clamp(320, monitor_w.max(320)));
            logging::info(&format!(
                "loaded position x={x} y={y} width={:?} from {}",
                width,
                path.display()
            ));
            return (x, y, width);
        }
        logging::warn(&format!("invalid position file {}", path.display()));
    }
    (default_x, default_y, None)
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
        let (playing, area) = {
            let mut state = ui_tick.borrow_mut();
            let index = if state.lyrics.is_empty() {
                None
            } else {
                pick_primary_index(&state.lyrics, state.lyric_time_ms())
            };
            let line_changed = state.last_line != index;
            if line_changed {
                state.last_line = index;
                if state.update_card_width() {
                    state.refresh_input_region("tick");
                }
            }
            (state.playing, state.area.clone())
        };
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
    if let Some(mode) = ui_settings.width_mode.as_deref().and_then(WidthMode::parse) {
        cfg.width_mode = mode;
    }
    if let Some(offset) = ui_settings.lyric_offset_ms {
        cfg.lyric_offset_ms = offset.clamp(-LYRIC_OFFSET_LIMIT_MS, LYRIC_OFFSET_LIMIT_MS);
    }
    if let Some(show) = ui_settings.show_translation {
        cfg.show_translation = show;
    }
    if let Some(show) = ui_settings.show_romanization {
        cfg.show_romanization = show;
    }
    if follow_system_color {
        if let Some(color) = system_accent_color() {
            cfg.played_color = color;
        }
    } else if let Some(color) = &ui_settings.played_color {
        cfg.played_color = color.clone();
    }

    gtk4::init().map_err(|e| anyhow::anyhow!("gtk init failed: {e}"))?;
    let display =
        gtk4::gdk::Display::default().ok_or_else(|| anyhow::anyhow!("no Gdk display available"))?;
    install_css(&display);

    let last_output = ui_settings
        .last_output
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (monitor, selected_output) = select_monitor(&display, last_output, &cfg.output);
    match &selected_output {
        Some(connector) => logging::info(&format!(
            "selected monitor {connector} (last_output={last_output:?}, config output={})",
            cfg.output
        )),
        None => logging::warn(&format!(
            "no monitor matched (last_output={last_output:?}, config output={}); letting the compositor choose",
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
    // Snapshot before building the window: a hotplug during setup must still
    // be noticed by the watcher.
    let monitor_baseline = monitor_snapshot(&display);

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
    let (win_x, win_y, saved_width) = load_position(&cfg, monitor_w, monitor_h);
    let ui = Rc::new(RefCell::new(UiState::new(
        cfg.clone(),
        win_x,
        win_y,
        PathBuf::from(&cfg.position_file),
        monitor_w,
        monitor_h,
    )));
    let last_output_changed =
        selected_output.is_some() && last_output != selected_output.as_deref();
    {
        let mut state = ui.borrow_mut();
        state.ui_settings_path = ui_settings_path;
        state.visible = ui_settings.visible.unwrap_or(true);
        state.follow_system_color = follow_system_color;
        // Keep the previous memory when no monitor is currently available.
        state.last_output = selected_output
            .clone()
            .or_else(|| last_output.map(str::to_string));
        let drag_threshold = gtk4::Settings::default()
            .map(|settings| f64::from(settings.gtk_dnd_drag_threshold()))
            .unwrap_or(8.0);
        state.pointer = interaction::PointerState::new(drag_threshold);
        if let Some(width) = saved_width {
            state.card_width = width;
            state.width_initialized = true;
        }
    }
    if last_output_changed {
        ui.borrow().save_ui_settings();
    }
    let _tray_available = tray::spawn(
        tray_tx,
        tray::TrayOptions {
            font_size: cfg.font_size,
            font_family: cfg.font_family.clone(),
            follow_system_color,
            played_color: cfg.played_color.clone(),
            output: selected_output
                .clone()
                .unwrap_or_else(|| cfg.output.clone()),
            width_mode: cfg.width_mode.as_str().to_string(),
            lyric_offset_ms: cfg.lyric_offset_ms,
            show_translation: cfg.show_translation,
            show_romanization: cfg.show_romanization,
            outputs: monitor_options(&display, selected_output.as_deref().unwrap_or(&cfg.output)),
        },
    );
    let tick: TickCell = Rc::new(RefCell::new(None));

    let (window, area) = build_window(monitor.as_ref(), ui.clone(), cmd_tx.clone());
    {
        let mut state = ui.borrow_mut();
        state.area = Some(area);
        state.window = Some(window.clone());
    }

    window.present();

    // The layer surface is bound to one output and sized at startup, so a
    // hotplug or mode change is handled by restarting with the new geometry.
    // The restart re-selects last_output (runtime/ui.json) or the largest
    // connected monitor.
    start_monitor_watch(&display, ui.clone(), config_path, monitor_baseline);

    let w = window.clone();
    let ui_region = ui.clone();
    glib::timeout_add_local_once(Duration::from_millis(400), move || {
        let spec = ui_region.borrow().input_region_spec();
        apply_input_region(&w, &spec, "post-present");
    });

    // niri may remap/reconfigure layer surfaces when entering fullscreen. Re-apply
    // the input region defensively so the controls/handle remain interactive.
    let region_window = window.clone();
    let region_ui = ui.clone();
    glib::timeout_add_local(Duration::from_secs(1), move || {
        let spec = region_ui.borrow().input_region_spec();
        apply_input_region(&region_window, &spec, "periodic");
        glib::ControlFlow::Continue
    });

    let ui_drain = ui.clone();
    let tick_drain = tick.clone();
    let tx_drain = cmd_tx.clone();
    let window_drain = window.clone();
    let config_path_drain = config_path.to_path_buf();
    let rx = ui_rx;
    glib::timeout_add_local(Duration::from_millis(50), move || {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                UiMsg::NowPlaying(Ok(np)) => {
                    let playing = update_ui(&ui_drain, &window_drain, "state", |state| {
                        state.apply_now_playing(np, &tx_drain);
                        state.playing
                    });
                    if playing {
                        start_tick(&ui_drain, &tick_drain);
                    } else {
                        stop_tick(&tick_drain);
                    }
                }
                UiMsg::NowPlaying(Err(err)) => {
                    let should_log = update_ui(&ui_drain, &window_drain, "api-down", |state| {
                        if state.playing {
                            state.anchor_position = state.current_time_ms();
                            state.playing = false;
                        }
                        state.api_ok = false;
                        state.pointer.cancel();
                        state.hover = None;
                        state.hover_handle = false;
                        state.hover_hide = false;
                        let first = !state.api_error_logged;
                        state.api_error_logged = true;
                        state.redraw();
                        first
                    });
                    refresh_cursor_for(&ui_drain);
                    stop_tick(&tick_drain);
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
                    update_ui(&ui_drain, &window_drain, "lyrics", |state| {
                        state.apply_lyrics(requested_track, result);
                    });
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
                    update_ui(&ui_drain, &window_drain, "toggle", |state| {
                        state.visible = !state.visible;
                        if !state.visible {
                            state.pointer.cancel();
                            state.hover = None;
                            state.hover_handle = false;
                            state.hover_hide = false;
                        }
                        state.redraw();
                        state.save_ui_settings();
                    });
                    refresh_cursor_for(&ui_drain);
                }
                UiMsg::SetFontSize(size) => {
                    update_ui(&ui_drain, &window_drain, "font-size", |state| {
                        state.cfg.font_size = size.clamp(8, 300);
                        state.update_card_width();
                        state.redraw();
                        state.save_ui_settings();
                    });
                }
                UiMsg::SetFontFamily(family) => {
                    update_ui(&ui_drain, &window_drain, "font-family", |state| {
                        if !family.trim().is_empty() {
                            state.cfg.font_family = family;
                        }
                        state.update_card_width();
                        state.redraw();
                        state.save_ui_settings();
                    });
                }
                UiMsg::SetColor(color) => {
                    update_ui(&ui_drain, &window_drain, "color", |state| {
                        state.cfg.played_color = color;
                        state.follow_system_color = false;
                        state.redraw();
                        state.save_ui_settings();
                    });
                }
                UiMsg::FollowSystemColor => {
                    update_ui(&ui_drain, &window_drain, "system-color", |state| {
                        if let Some(color) = system_accent_color() {
                            state.cfg.played_color = color;
                        } else {
                            logging::warn(
                                "follow system color requested, but no accent color was found; will retry",
                            );
                        }
                        // Keep following even when no accent is readable yet;
                        // the 5 s watcher applies it once the file appears.
                        state.follow_system_color = true;
                        state.redraw();
                        state.save_ui_settings();
                    });
                }
                UiMsg::SetWidthMode(mode) => {
                    if let Some(mode) = WidthMode::parse(&mode) {
                        update_ui(&ui_drain, &window_drain, "width-mode", |state| {
                            state.cfg.width_mode = mode;
                            state.save_ui_settings();
                        });
                        logging::info(&format!("width mode set to {}", mode.as_str()));
                    }
                }
                UiMsg::SetLyricOffset(offset) => {
                    update_ui(&ui_drain, &window_drain, "lyric-offset", |state| {
                        state.cfg.lyric_offset_ms =
                            offset.clamp(-LYRIC_OFFSET_LIMIT_MS, LYRIC_OFFSET_LIMIT_MS);
                        state.update_card_width();
                        state.redraw();
                        state.save_ui_settings();
                        logging::info(&format!(
                            "lyric offset set to {} ms",
                            state.cfg.lyric_offset_ms
                        ));
                    });
                }
                UiMsg::ToggleTranslation => {
                    update_ui(&ui_drain, &window_drain, "translation", |state| {
                        state.cfg.show_translation = !state.cfg.show_translation;
                        state.update_card_width();
                        state.redraw();
                        state.save_ui_settings();
                    });
                }
                UiMsg::ToggleRomanization => {
                    update_ui(&ui_drain, &window_drain, "romanization", |state| {
                        state.cfg.show_romanization = !state.cfg.show_romanization;
                        state.update_card_width();
                        state.redraw();
                        state.save_ui_settings();
                    });
                }
                UiMsg::SetOutput(output) => {
                    {
                        let mut state = ui_drain.borrow_mut();
                        if state.position_dirty {
                            state.save_position();
                        }
                        state.last_output = Some(output.clone());
                        state.save_ui_settings();
                    }
                    restart_with_output(&config_path_drain, &output);
                }
                UiMsg::Quit => {
                    let mut state = ui_drain.borrow_mut();
                    if state.position_dirty {
                        state.save_position();
                    }
                    std::process::exit(0);
                }
            }
        }
        glib::ControlFlow::Continue
    });

    let _ = cmd_tx.send(Command::ForcePoll);

    // Flush deferred position writes. Drag-end still saves immediately; this
    // only coalesces the per-line width anchoring updates.
    let save_ui = ui.clone();
    glib::timeout_add_seconds_local(3, move || {
        let mut state = save_ui.borrow_mut();
        if state.position_dirty {
            state.save_position();
        }
        glib::ControlFlow::Continue
    });

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

#[cfg(test)]
mod tests {
    use super::*;

    fn left_edge(win_x: i32, width: i32) -> i32 {
        win_x - width / 2
    }

    fn right_edge(win_x: i32, width: i32) -> i32 {
        win_x - width / 2 + width
    }

    #[test]
    fn width_mode_parses_aliases() {
        assert_eq!(WidthMode::parse("center"), Some(WidthMode::Center));
        assert_eq!(WidthMode::parse("symmetric"), Some(WidthMode::Center));
        assert_eq!(WidthMode::parse("LEFT"), Some(WidthMode::Left));
        assert_eq!(WidthMode::parse("right"), Some(WidthMode::Right));
        assert_eq!(WidthMode::parse("bogus"), None);
    }

    #[test]
    fn monitor_selection_prefers_remembered_then_largest() {
        let candidates = vec![
            MonitorCandidate {
                connector: Some("eDP-1".to_string()),
                width: 1920,
                height: 1080,
            },
            MonitorCandidate {
                connector: Some("HDMI-A-1".to_string()),
                width: 2560,
                height: 1440,
            },
            MonitorCandidate {
                connector: Some("HDMI-A-2".to_string()),
                width: 1920,
                height: 1080,
            },
        ];

        // The remembered connector wins even when another screen is larger.
        assert_eq!(choose_monitor_index(&candidates, Some("eDP-1")), Some(0));
        // Surrounding whitespace is ignored.
        assert_eq!(choose_monitor_index(&candidates, Some(" eDP-1 ")), Some(0));
        // A missing remembered screen falls back to the highest resolution.
        assert_eq!(choose_monitor_index(&candidates, Some("DP-9")), Some(1));
        assert_eq!(choose_monitor_index(&candidates, None), Some(1));
        assert_eq!(choose_monitor_index(&[], None), None);
    }

    #[test]
    fn monitor_selection_ties_keep_first_and_skip_invalid_size() {
        let candidates = vec![
            MonitorCandidate {
                connector: None,
                width: 1920,
                height: 1080,
            },
            MonitorCandidate {
                connector: Some("B".to_string()),
                width: 1920,
                height: 1080,
            },
            MonitorCandidate {
                connector: Some("C".to_string()),
                width: 0,
                height: 0,
            },
        ];

        // Equal areas are deterministic: the earliest entry wins.
        assert_eq!(choose_monitor_index(&candidates, None), Some(0));
        // An explicit connector still wins.
        assert_eq!(choose_monitor_index(&candidates, Some("B")), Some(1));
        // Zero-sized entries are only used as a last resort.
        assert_eq!(choose_monitor_index(&candidates[1..], None), Some(0));
    }

    #[test]
    fn local_clock_deadband_ignores_small_poll_jitter() {
        assert!((choose_anchor(1000.0, 1050.0, true) - 1000.0).abs() < f64::EPSILON);
        assert!((choose_anchor(1000.0, 1200.0, true) - 1200.0).abs() < f64::EPSILON);
        assert!((choose_anchor(1000.0, 1000.0, false) - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn width_fingerprint_tracks_text_and_style() {
        let cfg = Config::default();
        let line = LyricLine {
            words: vec![LyricWord {
                word: "hello".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let base = width_fingerprint(&cfg, Some(&line), false, 1920);
        assert_eq!(base, width_fingerprint(&cfg, Some(&line), false, 1920));

        let color_only = Config {
            played_color: "#000000".to_string(),
            ..Default::default()
        };
        assert_eq!(
            base,
            width_fingerprint(&color_only, Some(&line), false, 1920)
        );

        let larger = Config {
            font_size: Config::default().font_size + 1,
            ..Default::default()
        };
        assert_ne!(base, width_fingerprint(&larger, Some(&line), false, 1920));

        let wider = Config {
            background: crate::config::BackgroundConfig {
                padding_x: Config::default().background.padding_x + 1,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_ne!(base, width_fingerprint(&wider, Some(&line), false, 1920));

        let other_line = LyricLine {
            words: vec![LyricWord {
                word: "world".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_ne!(
            base,
            width_fingerprint(&cfg, Some(&other_line), false, 1920)
        );
    }

    #[test]
    fn width_modes_keep_the_expected_edge() {
        for (old_width, new_width) in [(800, 500), (500, 800), (801, 499), (799, 1200)] {
            let old_x = 1000;
            assert_eq!(
                next_win_x(WidthMode::Center, old_x, old_width, new_width),
                old_x
            );

            let left = next_win_x(WidthMode::Left, old_x, old_width, new_width);
            assert_eq!(right_edge(left, new_width), right_edge(old_x, old_width));

            let right = next_win_x(WidthMode::Right, old_x, old_width, new_width);
            assert_eq!(left_edge(right, new_width), left_edge(old_x, old_width));
        }
    }
}
