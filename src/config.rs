use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

pub const LYRIC_OFFSET_LIMIT_MS: f64 = 30_000.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub api: ApiConfig,
    pub output: String,
    pub width_mode: WidthMode,
    pub width: i32,
    pub height: i32,
    pub bottom_margin: i32,
    pub position_file: String,
    pub font_family: String,
    pub font_size: i32,
    pub font_weight: i32,
    pub supersample: f64,
    pub played_color: String,
    pub unplayed_color: String,
    pub translation_color: String,
    pub translation_size_delta: i32,
    pub show_translation: bool,
    pub show_romanization: bool,
    pub word_by_word: bool,
    pub fps: u32,
    pub lyric_offset_ms: f64,
    pub hide_when_disconnected: bool,
    pub background: BackgroundConfig,
    pub controls: ControlsConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WidthMode {
    #[default]
    Center,
    Left,
    Right,
}

impl WidthMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "center" | "centre" | "symmetric" | "symmetrical" => Some(Self::Center),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Center => "center",
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

impl<'de> Deserialize<'de> for WidthMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::parse(&value).unwrap_or_default())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub host: String,
    pub port: u16,
    pub poll_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BackgroundConfig {
    pub enabled: bool,
    pub color: String,
    pub alpha: f64,
    pub radius: i32,
    pub padding_x: i32,
    pub padding_y: i32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ControlsConfig {
    pub enabled: bool,
    pub right_margin: i32,
    pub bottom_margin: i32,
    pub size: i32,
    pub opacity: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api: ApiConfig::default(),
            output: "HDMI-A-1".to_string(),
            width_mode: WidthMode::Center,
            width: 1800,
            height: 200,
            bottom_margin: 96,
            position_file: "runtime/position.json".to_string(),
            font_family: "Noto Sans, Noto Sans CJK SC".to_string(),
            font_size: 36,
            font_weight: 600,
            supersample: 2.0,
            played_color: "#fe7971".to_string(),
            unplayed_color: "#ffffff".to_string(),
            translation_color: "#d8d8d8".to_string(),
            translation_size_delta: 4,
            show_translation: true,
            show_romanization: false,
            word_by_word: true,
            fps: 30,
            lyric_offset_ms: 0.0,
            hide_when_disconnected: true,
            background: BackgroundConfig::default(),
            controls: ControlsConfig::default(),
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 14558,
            poll_ms: 900,
        }
    }
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            color: "#000000".to_string(),
            alpha: 0.22,
            radius: 14,
            padding_x: 28,
            padding_y: 12,
        }
    }
}

impl Default for ControlsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            right_margin: 48,
            bottom_margin: 24,
            size: 44,
            opacity: 0.85,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let mut cfg: Config = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        cfg.normalize();
        Ok(cfg)
    }

    fn normalize(&mut self) {
        self.width = self.width.clamp(320, 4096);
        self.height = self.height.clamp(80, 2000);
        self.bottom_margin = self.bottom_margin.max(0);
        self.font_size = self.font_size.clamp(8, 300);
        self.translation_size_delta = self.translation_size_delta.clamp(0, 100);
        self.font_weight = self.font_weight.clamp(100, 1000);
        self.supersample = if self.supersample.is_finite() {
            self.supersample.clamp(1.0, 3.0)
        } else {
            2.0
        };
        self.fps = self.fps.clamp(1, 30);
        self.lyric_offset_ms = self
            .lyric_offset_ms
            .clamp(-LYRIC_OFFSET_LIMIT_MS, LYRIC_OFFSET_LIMIT_MS);
        self.api.poll_ms = self.api.poll_ms.clamp(150, 60_000);
        self.background.alpha = self.background.alpha.clamp(0.0, 1.0);
        self.background.radius = self.background.radius.max(0);
        self.background.padding_x = self.background.padding_x.max(0);
        self.background.padding_y = self.background.padding_y.max(0);
        self.controls.size = self.controls.size.clamp(16, 200);
        self.controls.opacity = self.controls.opacity.clamp(0.05, 1.0);
        self.controls.right_margin = self.controls.right_margin.max(0);
        self.controls.bottom_margin = self.controls.bottom_margin.max(0);
        if self.api.host.trim().is_empty() {
            self.api.host = "127.0.0.1".to_string();
        }
        if self.font_family.trim().is_empty() {
            self.font_family = "Noto Sans, Noto Sans CJK SC".to_string();
        }
    }

    pub fn translation_px(&self) -> i32 {
        (self.font_size - self.translation_size_delta).clamp(6, 200)
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.api.host, self.api.port)
    }
}
