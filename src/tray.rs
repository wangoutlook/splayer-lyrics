use crate::api::UiMsg;
use crate::config::LYRIC_OFFSET_LIMIT_MS;
use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use std::sync::mpsc::Sender;

const SIZES: [i32; 4] = [28, 32, 36, 40];
const FONTS: [(&str, &str); 4] = [
    (
        "Noto Sans + Noto Sans CJK SC",
        "Noto Sans, Noto Sans CJK SC",
    ),
    ("Noto Sans CJK SC", "Noto Sans CJK SC"),
    ("霞鹜文楷 Screen", "LXGW WenKai Screen"),
    ("Noto Serif CJK SC", "Noto Serif CJK SC"),
];
const COLORS: [(&str, Option<&str>); 5] = [
    ("跟随系统主题色", None),
    ("SPlayer 粉", Some("#fe7971")),
    ("白色", Some("#ffffff")),
    ("蓝色", Some("#8ecae6")),
    ("绿色", Some("#a7c957")),
];
const WIDTH_MODES: [(&str, &str); 3] = [
    ("对称伸缩（中心不动）", "center"),
    ("向左伸缩（右边缘不动）", "left"),
    ("向右伸缩（左边缘不动）", "right"),
];
const OFFSET_STEPS_MS: [f64; 4] = [-100.0, -50.0, 50.0, 100.0];

pub struct TrayOptions {
    pub font_size: i32,
    pub font_family: String,
    pub follow_system_color: bool,
    pub played_color: String,
    pub output: String,
    pub width_mode: String,
    pub lyric_offset_ms: f64,
    pub show_translation: bool,
    pub show_romanization: bool,
    pub outputs: Vec<(String, String)>,
}

pub struct TrayApp {
    tx: Sender<UiMsg>,
    size_idx: usize,
    font_idx: usize,
    color_idx: usize,
    output_idx: usize,
    width_idx: usize,
    offset_ms: f64,
    show_translation: bool,
    show_romanization: bool,
    outputs: Vec<(String, String)>,
}

impl TrayApp {
    fn offset_items(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: format!("当前: {:+.0} ms", self.offset_ms),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];
        items.extend(
            OFFSET_STEPS_MS
                .iter()
                .map(|step| {
                    let step = *step;
                    StandardItem {
                        label: format!("{step:+.0} ms"),
                        activate: Box::new(move |this: &mut Self| {
                            this.offset_ms = (this.offset_ms + step)
                                .clamp(-LYRIC_OFFSET_LIMIT_MS, LYRIC_OFFSET_LIMIT_MS);
                            let _ = this.tx.send(UiMsg::SetLyricOffset(this.offset_ms));
                        }),
                        ..Default::default()
                    }
                    .into()
                })
                .collect::<Vec<MenuItem<Self>>>(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "重置 (0 ms)".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.offset_ms = 0.0;
                    let _ = this.tx.send(UiMsg::SetLyricOffset(0.0));
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

impl ksni::Tray for TrayApp {
    fn id(&self) -> String {
        "splayer-lyrics".to_string()
    }

    fn title(&self) -> String {
        "SPlayer Lyrics".to_string()
    }

    fn icon_name(&self) -> String {
        "audio-x-generic".to_string()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let size_options: Vec<RadioItem> = SIZES
            .iter()
            .map(|size| RadioItem {
                label: format!("{size}px"),
                ..Default::default()
            })
            .collect();

        let font_options: Vec<RadioItem> = FONTS
            .iter()
            .map(|(label, _)| RadioItem {
                label: (*label).to_string(),
                ..Default::default()
            })
            .collect();

        let color_options: Vec<RadioItem> = COLORS
            .iter()
            .map(|(label, _)| RadioItem {
                label: (*label).to_string(),
                ..Default::default()
            })
            .collect();

        let output_options: Vec<RadioItem> = self
            .outputs
            .iter()
            .map(|(label, _)| RadioItem {
                label: label.clone(),
                ..Default::default()
            })
            .collect();

        let width_options: Vec<RadioItem> = WIDTH_MODES
            .iter()
            .map(|(label, _)| RadioItem {
                label: (*label).to_string(),
                ..Default::default()
            })
            .collect();

        vec![
            StandardItem {
                label: "显示/隐藏歌词".to_string(),
                icon_name: "view-conceal-symbolic".to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(UiMsg::ToggleVisible);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "显示翻译".to_string(),
                checked: self.show_translation,
                activate: Box::new(|this: &mut Self| {
                    this.show_translation = !this.show_translation;
                    let _ = this.tx.send(UiMsg::ToggleTranslation);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "显示罗马音".to_string(),
                checked: self.show_romanization,
                activate: Box::new(|this: &mut Self| {
                    this.show_romanization = !this.show_romanization;
                    let _ = this.tx.send(UiMsg::ToggleRomanization);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: "字号".to_string(),
                submenu: vec![RadioGroup {
                    selected: self.size_idx,
                    select: Box::new(|this: &mut Self, selected: usize| {
                        this.size_idx = selected.min(SIZES.len() - 1);
                        let _ = this.tx.send(UiMsg::SetFontSize(SIZES[this.size_idx]));
                    }),
                    options: size_options,
                }
                .into()],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "字体".to_string(),
                submenu: vec![RadioGroup {
                    selected: self.font_idx,
                    select: Box::new(|this: &mut Self, selected: usize| {
                        this.font_idx = selected.min(FONTS.len() - 1);
                        let _ = this
                            .tx
                            .send(UiMsg::SetFontFamily(FONTS[this.font_idx].1.to_string()));
                    }),
                    options: font_options,
                }
                .into()],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "宽度变化".to_string(),
                submenu: vec![RadioGroup {
                    selected: self.width_idx,
                    select: Box::new(|this: &mut Self, selected: usize| {
                        this.width_idx = selected.min(WIDTH_MODES.len() - 1);
                        let _ = this.tx.send(UiMsg::SetWidthMode(
                            WIDTH_MODES[this.width_idx].1.to_string(),
                        ));
                    }),
                    options: width_options,
                }
                .into()],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "歌词偏移".to_string(),
                submenu: self.offset_items(),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "显示器".to_string(),
                submenu: vec![RadioGroup {
                    selected: self.output_idx,
                    select: Box::new(|this: &mut Self, selected: usize| {
                        this.output_idx = selected.min(this.outputs.len().saturating_sub(1));
                        if let Some((_, output)) = this.outputs.get(this.output_idx) {
                            let _ = this.tx.send(UiMsg::SetOutput(output.clone()));
                        }
                    }),
                    options: output_options,
                }
                .into()],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "配色".to_string(),
                submenu: vec![RadioGroup {
                    selected: self.color_idx,
                    select: Box::new(|this: &mut Self, selected: usize| {
                        this.color_idx = selected.min(COLORS.len() - 1);
                        if let Some(color) = COLORS[this.color_idx].1 {
                            let _ = this.tx.send(UiMsg::SetColor(color.to_string()));
                        } else {
                            let _ = this.tx.send(UiMsg::FollowSystemColor);
                        }
                    }),
                    options: color_options,
                }
                .into()],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "退出".to_string(),
                icon_name: "application-exit".to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(UiMsg::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Returns whether the StatusNotifierItem was registered. When registration
/// fails (e.g. Waybar's tray module is not running), the caller still has a
/// working overlay, but only config-file controls are available.
pub fn spawn(tx: Sender<UiMsg>, options: TrayOptions) -> bool {
    let size_idx = SIZES
        .iter()
        .position(|size| *size == options.font_size)
        .unwrap_or(2);
    let font_idx = FONTS
        .iter()
        .position(|(_, family)| *family == options.font_family.as_str())
        .unwrap_or(0);
    let color_idx = if options.follow_system_color {
        0
    } else {
        COLORS
            .iter()
            .position(|(_, color)| {
                color
                    .map(|value| value == options.played_color.as_str())
                    .unwrap_or(false)
            })
            .unwrap_or(COLORS.len())
    };
    let width_idx = WIDTH_MODES
        .iter()
        .position(|(_, value)| *value == options.width_mode.as_str())
        .unwrap_or(0);

    let mut outputs = options.outputs;
    if !outputs.iter().any(|(_, value)| value == &options.output) {
        outputs.insert(
            0,
            (
                format!("{}（当前配置）", options.output),
                options.output.clone(),
            ),
        );
    }
    let output_idx = outputs
        .iter()
        .position(|(_, value)| value == &options.output)
        .unwrap_or(0);

    let tray = TrayApp {
        tx,
        size_idx,
        font_idx,
        color_idx,
        output_idx,
        width_idx,
        offset_ms: options
            .lyric_offset_ms
            .clamp(-LYRIC_OFFSET_LIMIT_MS, LYRIC_OFFSET_LIMIT_MS),
        show_translation: options.show_translation,
        show_romanization: options.show_romanization,
        outputs,
    };
    match tray.spawn() {
        Ok(handle) => {
            // Keep the service alive for the whole process lifetime.
            std::mem::forget(handle);
            true
        }
        Err(err) => {
            crate::logging::warn(&format!("tray unavailable: {err}"));
            false
        }
    }
}
