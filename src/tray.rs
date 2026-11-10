use crate::api::UiMsg;
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use std::sync::mpsc::Sender;

const SIZES: [i32; 4] = [28, 32, 36, 40];
const FONTS: [(&str, &str); 4] = [
    ("Noto Sans + Noto Sans CJK SC", "Noto Sans, Noto Sans CJK SC"),
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
const OUTPUTS: [(&str, &str); 2] = [("HDMI-A-1（主屏）", "HDMI-A-1"), ("eDP-1（笔记本）", "eDP-1")];

pub struct TrayApp {
    tx: Sender<UiMsg>,
    size_idx: usize,
    font_idx: usize,
    color_idx: usize,
    output_idx: usize,
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
        let size_options = SIZES
            .iter()
            .map(|size| {
                RadioItem {
                    label: format!("{size}px"),
                    ..Default::default()
                }
                .into()
            })
            .collect();

        let font_options = FONTS
            .iter()
            .map(|(label, _)| {
                RadioItem {
                    label: (*label).to_string(),
                    ..Default::default()
                }
                .into()
            })
            .collect();

        let color_options = COLORS
            .iter()
            .map(|(label, _)| {
                RadioItem {
                    label: (*label).to_string(),
                    ..Default::default()
                }
                .into()
            })
            .collect();

        let output_options = OUTPUTS
            .iter()
            .map(|(label, _)| {
                RadioItem {
                    label: (*label).to_string(),
                    ..Default::default()
                }
                .into()
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
            MenuItem::Separator,
            SubMenu {
                label: "字号".to_string(),
                submenu: vec![
                    RadioGroup {
                        selected: self.size_idx,
                        select: Box::new(|this: &mut Self, selected: usize| {
                            this.size_idx = selected.min(SIZES.len() - 1);
                            let _ = this.tx.send(UiMsg::SetFontSize(SIZES[this.size_idx]));
                        }),
                        options: size_options,
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "字体".to_string(),
                submenu: vec![
                    RadioGroup {
                        selected: self.font_idx,
                        select: Box::new(|this: &mut Self, selected: usize| {
                            this.font_idx = selected.min(FONTS.len() - 1);
                            let _ = this.tx.send(UiMsg::SetFontFamily(
                                FONTS[this.font_idx].1.to_string(),
                            ));
                        }),
                        options: font_options,
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "显示器".to_string(),
                submenu: vec![
                    RadioGroup {
                        selected: self.output_idx,
                        select: Box::new(|this: &mut Self, selected: usize| {
                            this.output_idx = selected.min(OUTPUTS.len() - 1);
                            let _ = this
                                .tx
                                .send(UiMsg::SetOutput(OUTPUTS[this.output_idx].1.to_string()));
                        }),
                        options: output_options,
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "配色".to_string(),
                submenu: vec![
                    RadioGroup {
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
                        ..Default::default()
                    }
                    .into(),
                ],
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

pub fn spawn(
    tx: Sender<UiMsg>,
    font_size: i32,
    font_family: &str,
    follow_system_color: bool,
    played_color: &str,
    output: &str,
) {
    let size_idx = SIZES
        .iter()
        .position(|size| *size == font_size)
        .unwrap_or(2);
    let font_idx = FONTS
        .iter()
        .position(|(_, family)| *family == font_family)
        .unwrap_or(0);
    let color_idx = if follow_system_color {
        0
    } else {
        COLORS
            .iter()
            .position(|(_, color)| color.map(|value| value == played_color).unwrap_or(false))
            .unwrap_or(1)
    };
    let output_idx = OUTPUTS
        .iter()
        .position(|(_, value)| *value == output)
        .unwrap_or(0);
    let tray = TrayApp {
        tx,
        size_idx,
        font_idx,
        color_idx,
        output_idx,
    };
    match tray.spawn() {
        Ok(handle) => {
            // Keep the service alive for the whole process lifetime.
            std::mem::forget(handle);
        }
        Err(err) => {
            crate::logging::warn(&format!("tray unavailable: {err}"));
        }
    }
}
