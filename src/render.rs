use crate::config::{BackgroundConfig, Config, ControlsConfig};
use crate::model::{line_progress, line_text, word_progress, word_timing_usable, LyricLine};
use cairo::Context;
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::f64::consts::PI;
use std::hash::{Hash, Hasher};

pub const CONTROLS_GAP: f64 = 6.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaintMode {
    Base,
    Played,
    Both,
}

#[derive(Clone, Default)]
struct LyricGeometry {
    word_ranges: Vec<(usize, usize, f64, f64)>,
    text_left: f64,
    text_width: f64,
    ink_top: f64,
    ink_height: f64,
    word_usable: bool,
}

struct CachedLyrics {
    key: u64,
    base: cairo::ImageSurface,
    played: cairo::ImageSurface,
    geometry: Option<LyricGeometry>,
}

thread_local! {
    static CACHE: RefCell<Option<CachedLyrics>> = const { RefCell::new(None) };
}

pub fn parse_color(hex: &str) -> (f64, f64, f64) {
    let value = hex.trim().trim_start_matches('#');
    let parse = |s: &str| u8::from_str_radix(s, 16).ok();
    match value.len() {
        3 => {
            let r = value.get(0..1).and_then(parse);
            let g = value.get(1..2).and_then(parse);
            let b = value.get(2..3).and_then(parse);
            match (r, g, b) {
                (Some(r), Some(g), Some(b)) => (
                    (r * 17) as f64 / 255.0,
                    (g * 17) as f64 / 255.0,
                    (b * 17) as f64 / 255.0,
                ),
                _ => (1.0, 1.0, 1.0),
            }
        }
        6 => {
            let r = value.get(0..2).and_then(parse);
            let g = value.get(2..4).and_then(parse);
            let b = value.get(4..6).and_then(parse);
            match (r, g, b) {
                (Some(r), Some(g), Some(b)) => {
                    (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0)
                }
                _ => (1.0, 1.0, 1.0),
            }
        }
        _ => (1.0, 1.0, 1.0),
    }
}

fn clear(cr: &Context) {
    let _ = cr.save();
    cr.set_operator(cairo::Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    let _ = cr.paint();
    cr.set_operator(cairo::Operator::Over);
    let _ = cr.restore();
}

pub fn clear_rect(cr: &Context, x: i32, y: i32, width: i32, height: i32) {
    if width <= 0 || height <= 0 {
        return;
    }
    let _ = cr.save();
    cr.rectangle(
        f64::from(x),
        f64::from(y),
        f64::from(width),
        f64::from(height),
    );
    cr.clip();
    clear(cr);
    let _ = cr.restore();
}

fn enable_grayscale_aa(cr: &Context) {
    cr.set_antialias(cairo::Antialias::Gray);
    if let Ok(mut options) = cr.font_options() {
        options.set_antialias(cairo::Antialias::Gray);
        options.set_hint_style(cairo::HintStyle::None);
        options.set_hint_metrics(cairo::HintMetrics::On);
        cr.set_font_options(&options);
    }
}

fn set_rgb(cr: &Context, color: (f64, f64, f64)) {
    cr.set_source_rgb(color.0, color.1, color.2);
}

fn rounded_rect(cr: &Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    let r = radius.max(0.0).min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * PI / 2.0);
    cr.close_path();
}

fn make_layout(
    cr: &Context,
    family: &str,
    size: i32,
    weight: i32,
    text: &str,
    width_px: i32,
) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(cr);
    let mut desc = pango::FontDescription::from_string(family);
    desc.set_absolute_size(f64::from(size) * f64::from(pango::SCALE));
    desc.set_weight(pango::Weight::__Unknown(weight));
    layout.set_font_description(Some(&desc));
    layout.set_text(text);
    layout.set_width(width_px.max(1) * pango::SCALE);
    layout.set_alignment(pango::Alignment::Center);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    layout.set_single_paragraph_mode(true);
    layout
}

struct TextBlock {
    layout: pango::Layout,
    ink_x: f64,
    ink_y: f64,
    ink_w: f64,
    ink_h: f64,
    y: f64,
}

fn measure(layout: &pango::Layout, y: f64) -> TextBlock {
    let (ink, _) = layout.pixel_extents();
    TextBlock {
        ink_x: f64::from(ink.x()),
        ink_y: f64::from(ink.y()),
        ink_w: f64::from(ink.width()),
        ink_h: f64::from(ink.height()),
        y,
        layout: layout.clone(),
    }
}

fn word_rects_cached(geometry: &LyricGeometry, line: &LyricLine, time_ms: f64) -> Vec<(f64, f64)> {
    const BLEED: f64 = 2.0;
    let mut rects = Vec::new();
    let mut first_visible = true;
    for (word, (_start_byte, _end_byte, x0, x1)) in line.words.iter().zip(&geometry.word_ranges) {
        if word.word.is_empty() {
            continue;
        }
        let progress = word_progress(line, word, time_ms);
        if progress <= 0.0 {
            continue;
        }
        let (lo, hi) = if *x1 >= *x0 { (*x0, *x1) } else { (*x1, *x0) };
        // The first visible word is clipped from the left edge of the surface.
        // The played layer contains only glyphs, so this cannot paint outside
        // the text, and it guarantees the first glyph is fully covered even if
        // Pango's advance origin has an unexpected bearing.
        let start = if first_visible {
            0.0
        } else {
            (lo - BLEED).max(0.0)
        };
        first_visible = false;
        let end = if progress >= 1.0 {
            hi + BLEED
        } else {
            lo + (hi - lo) * progress + BLEED * progress
        };
        let end = end.min(hi + BLEED);
        if end > start {
            rects.push((start, end));
        }
    }
    rects
}

fn pango_x(layout: &pango::Layout, byte_index: usize, margin: f64, hi: f64) -> f64 {
    let text_len = layout.text().len();
    let index = byte_index.min(text_len) as i32;
    let rect = layout.index_to_pos(index);
    let x = margin + f64::from(rect.x()) / f64::from(pango::SCALE);
    x.clamp(0.0, hi)
}

fn draw_card_background(cr: &Context, width: i32, top: f64, bottom: f64, cfg: &BackgroundConfig) {
    if !cfg.enabled {
        return;
    }
    let (r, g, b) = parse_color(&cfg.color);
    let height = (bottom - top).max(1.0);
    cr.set_source_rgba(r, g, b, cfg.alpha);
    rounded_rect(
        cr,
        0.0,
        top,
        f64::from(width),
        height,
        f64::from(cfg.radius),
    );
    let _ = cr.fill();
}

pub struct LyricScene<'a> {
    pub cfg: &'a Config,
    pub line: Option<&'a LyricLine>,
    pub time_ms: f64,
    pub placeholder: bool,
}

/// Identifies everything baked into the cached text surfaces. The card
/// background is painted separately in `draw_overlay`, so its config must not
/// invalidate this cache. A 64-bit hash keeps the per-frame lookup
/// allocation-free (this runs at the lyric fps). A collision would only reuse
/// one stale frame until the key changes; that is accepted for this
/// in-process visual cache.
fn cache_key(scene: &LyricScene, width: i32, height: i32) -> u64 {
    let cfg = scene.cfg;
    let mut hasher = DefaultHasher::new();
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    scene.placeholder.hash(&mut hasher);
    cfg.font_family.hash(&mut hasher);
    cfg.font_size.hash(&mut hasher);
    cfg.font_weight.hash(&mut hasher);
    cfg.supersample.to_bits().hash(&mut hasher);
    cfg.played_color.hash(&mut hasher);
    cfg.unplayed_color.hash(&mut hasher);
    cfg.translation_color.hash(&mut hasher);
    cfg.translation_px().hash(&mut hasher);
    cfg.show_translation.hash(&mut hasher);
    cfg.show_romanization.hash(&mut hasher);
    if let Some(line) = scene.line {
        for word in &line.words {
            word.word.hash(&mut hasher);
        }
        line.translated.hash(&mut hasher);
        line.roman.hash(&mut hasher);
        (cfg.word_by_word && word_timing_usable(line)).hash(&mut hasher);
    }
    hasher.finish()
}

fn build_cache(scene: &LyricScene, width: i32, height: i32, key: u64) -> Option<CachedLyrics> {
    let w = width.max(1);
    let h = height.max(1);
    let ss = scene.cfg.supersample;
    let sw = (f64::from(w) * ss).round().max(1.0) as i32;
    let sh = (f64::from(h) * ss).round().max(1.0) as i32;
    let base_hi = cairo::ImageSurface::create(cairo::Format::ARgb32, sw, sh).ok()?;
    let base_cr = Context::new(&base_hi).ok()?;
    base_cr.scale(ss, ss);
    let geometry = draw_lyrics_mode(&base_cr, width, height, scene, PaintMode::Base);

    let played_hi = cairo::ImageSurface::create(cairo::Format::ARgb32, sw, sh).ok()?;
    let played_cr = Context::new(&played_hi).ok()?;
    played_cr.scale(ss, ss);
    let _ = draw_lyrics_mode(&played_cr, width, height, scene, PaintMode::Played);

    let base = downscale_surface(&base_hi, w, h, ss)?;
    let played = downscale_surface(&played_hi, w, h, ss)?;
    Some(CachedLyrics {
        key,
        base,
        played,
        geometry,
    })
}

fn downscale_surface(
    source: &cairo::ImageSurface,
    width: i32,
    height: i32,
    scale: f64,
) -> Option<cairo::ImageSurface> {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).ok()?;
    let cr = Context::new(&surface).ok()?;
    let _ = cr.save();
    cr.scale(1.0 / scale, 1.0 / scale);
    if cr.set_source_surface(source, 0.0, 0.0).is_ok() {
        cr.source().set_filter(cairo::Filter::Best);
        let _ = cr.paint();
    }
    let _ = cr.restore();
    Some(surface)
}

fn draw_cached_lyrics(cr: &Context, width: i32, height: i32, scene: &LyricScene) {
    if scene.line.is_none() {
        return;
    }
    let key = cache_key(scene, width, height);
    CACHE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().map(|c| c.key != key).unwrap_or(true) {
            *slot = build_cache(scene, width, height, key);
        }
        let Some(cache) = slot.as_ref() else {
            draw_lyrics_mode(cr, width, height, scene, PaintMode::Both);
            return;
        };

        let _ = cr.save();
        if cr.set_source_surface(&cache.base, 0.0, 0.0).is_ok() {
            cr.source().set_filter(cairo::Filter::Best);
            let _ = cr.paint();
        }

        if let Some(geometry) = &cache.geometry {
            let line = scene.line.unwrap();
            let rects: Vec<(f64, f64)> = if scene.placeholder {
                Vec::new()
            } else if geometry.word_usable {
                word_rects_cached(geometry, line, scene.time_ms)
            } else {
                let progress = line_progress(line, scene.time_ms);
                if progress > 0.0 {
                    vec![(
                        0.0,
                        geometry.text_left + geometry.text_width * progress + 2.0,
                    )]
                } else {
                    Vec::new()
                }
            };

            if !rects.is_empty() {
                let clip_top = geometry.ink_top - 1.0;
                let clip_h = geometry.ink_height + 2.0;
                let _ = cr.save();
                cr.new_path();
                for (x0, x1) in &rects {
                    let (lo, hi) = if *x1 >= *x0 { (*x0, *x1) } else { (*x1, *x0) };
                    if hi > lo {
                        cr.rectangle(lo, clip_top, hi - lo, clip_h);
                    }
                }
                cr.clip();
                if cr.set_source_surface(&cache.played, 0.0, 0.0).is_ok() {
                    cr.source().set_filter(cairo::Filter::Best);
                    let _ = cr.paint();
                }
                let _ = cr.restore();
            }
        }
        let _ = cr.restore();
    });
}

fn draw_lyrics_mode(
    cr: &Context,
    width: i32,
    height: i32,
    scene: &LyricScene,
    mode: PaintMode,
) -> Option<LyricGeometry> {
    clear(cr);
    enable_grayscale_aa(cr);

    let line = scene.line?;

    let cfg = scene.cfg;
    let h = f64::from(height);
    let margin = 24.0f64;
    let max_width = (width - 2 * margin as i32).max(1);

    let main_text = line_text(line);
    let gap = 6.0;
    let mut main_layout = None;
    let mut main_height = 0.0;

    if !main_text.is_empty() {
        let layout = make_layout(
            cr,
            &cfg.font_family,
            cfg.font_size,
            cfg.font_weight,
            &main_text,
            max_width,
        );
        let (_, logical) = layout.pixel_extents();
        main_height = f64::from(logical.height());
        main_layout = Some(layout);
    }

    // The played layer contains only the main lyric. Laying out translation,
    // romanization and word geometry for this pass is wasted work.
    if mode == PaintMode::Played {
        if let Some(layout) = &main_layout {
            let y = ((h - main_height) / 2.0).max(0.0);
            let _ = cr.save();
            cr.translate(margin, y.round());
            set_rgb(cr, parse_color(&cfg.played_color));
            pangocairo::functions::show_layout(cr, layout);
            let _ = cr.restore();
        }
        return None;
    }

    let translation = if cfg.show_translation {
        line.translated.trim().to_string()
    } else {
        String::new()
    };
    let roman = if cfg.show_romanization {
        line.roman.trim().to_string()
    } else {
        String::new()
    };

    let mut translation_layout = None;
    let mut roman_layout = None;
    let mut trans_height = 0.0;
    let mut roman_height = 0.0;

    if !translation.is_empty() {
        let layout = make_layout(
            cr,
            &cfg.font_family,
            cfg.translation_px(),
            400,
            &translation,
            max_width,
        );
        let (_, logical) = layout.pixel_extents();
        trans_height = f64::from(logical.height());
        translation_layout = Some(layout);
    }
    if !roman.is_empty() {
        let layout = make_layout(
            cr,
            &cfg.font_family,
            (cfg.translation_px() - 2).max(8),
            400,
            &roman,
            max_width,
        );
        let (_, logical) = layout.pixel_extents();
        roman_height = f64::from(logical.height());
        roman_layout = Some(layout);
    }

    let mut block_height = main_height;
    if trans_height > 0.0 {
        block_height += gap + trans_height;
    }
    if roman_height > 0.0 {
        block_height += gap + roman_height;
    }
    // Anchor the main lyric line, not the whole block, so adding/removing a
    // translation line does not move the lyric line vertically.
    let y = if main_height > 0.0 {
        ((h - main_height) / 2.0).max(0.0)
    } else {
        ((h - block_height) / 2.0).max(0.0)
    };

    let mut main_block: Option<TextBlock> = None;

    if let Some(layout) = &main_layout {
        main_block = Some(measure(layout, y));
    }

    let mut geometry: Option<LyricGeometry> = None;

    if let Some(block) = &main_block {
        let ink_left = margin + block.ink_x;
        let ink_right = ink_left + block.ink_w;
        let ink_top = block.y + block.ink_y;
        let ink_h = block.ink_h.max(1.0);

        let text_left =
            margin + f64::from(block.layout.index_to_pos(0).x()) / f64::from(pango::SCALE);
        let text_end = margin
            + f64::from(
                block
                    .layout
                    .index_to_pos(block.layout.text().len() as i32)
                    .x(),
            ) / f64::from(pango::SCALE);
        let text_width = (text_end - text_left).max(1.0);
        let word_usable = cfg.word_by_word && word_timing_usable(line);
        let mut word_ranges = Vec::new();
        let mut byte_offset = 0usize;
        for word in &line.words {
            let start_byte = byte_offset;
            byte_offset += word.word.len();
            let x0 = pango_x(&block.layout, start_byte, margin, ink_right);
            let x1 = pango_x(&block.layout, byte_offset, margin, ink_right);
            word_ranges.push((start_byte, byte_offset, x0, x1));
        }
        geometry = Some(LyricGeometry {
            word_ranges,
            text_left,
            text_width,
            ink_top,
            ink_height: ink_h,
            word_usable,
        });

        let _ = cr.save();
        cr.translate(margin, block.y.round());
        set_rgb(cr, parse_color(&cfg.unplayed_color));
        pangocairo::functions::show_layout(cr, &block.layout);
        let _ = cr.restore();

        if mode == PaintMode::Both {
            let rects: Vec<(f64, f64)> = if scene.placeholder {
                Vec::new()
            } else if word_usable {
                word_rects_cached(geometry.as_ref().unwrap(), line, scene.time_ms)
            } else {
                let progress = line_progress(line, scene.time_ms);
                if progress > 0.0 {
                    vec![(0.0, text_left + text_width * progress + 2.0)]
                } else {
                    Vec::new()
                }
            };

            if !rects.is_empty() {
                let clip_top = ink_top - 1.0;
                let clip_h = ink_h + 2.0;
                let _ = cr.save();
                cr.new_path();
                for (x0, x1) in &rects {
                    let (lo, hi) = if *x1 >= *x0 { (*x0, *x1) } else { (*x1, *x0) };
                    if hi > lo {
                        cr.rectangle(lo, clip_top, hi - lo, clip_h);
                    }
                }
                cr.clip();
                cr.translate(margin, block.y.round());
                set_rgb(cr, parse_color(&cfg.played_color));
                pangocairo::functions::show_layout(cr, &block.layout);
                let _ = cr.restore();
            }
        }
    }

    if mode != PaintMode::Played {
        if let Some(layout) = &translation_layout {
            // Recompute y position from the block height chain: translation sits
            // directly below the main block.
            let ty = if let Some(block) = &main_block {
                block.y + main_height + gap
            } else {
                (h - block_height) / 2.0
            };
            let _ = cr.save();
            cr.translate(margin, ty);
            set_rgb(cr, parse_color(&cfg.translation_color));
            pangocairo::functions::show_layout(cr, layout);
            let _ = cr.restore();
        }

        if let Some(layout) = &roman_layout {
            let ty = if let Some(block) = &main_block {
                block.y + main_height + gap
            } else {
                (h - block_height) / 2.0
            };
            let ty = ty
                + if trans_height > 0.0 {
                    trans_height + gap
                } else {
                    0.0
                };
            let _ = cr.save();
            cr.translate(margin, ty);
            set_rgb(cr, parse_color(&cfg.translation_color));
            pangocairo::functions::show_layout(cr, layout);
            let _ = cr.restore();
        }
    }

    geometry
}

pub const HANDLE_WIDTH: f64 = 20.0;
pub const HANDLE_GAP: f64 = 8.0;

pub fn controls_total_width(cfg: &ControlsConfig) -> i32 {
    (f64::from(cfg.size) * 3.0 + CONTROLS_GAP * 2.0 + HANDLE_GAP + HANDLE_WIDTH).round() as i32
}

#[derive(Debug, Clone)]
pub struct ControlsGeometry {
    pub buttons: Vec<(f64, f64, f64)>,
    pub handle: (f64, f64, f64, f64),
    pub hide: (f64, f64, f64, f64),
    pub input_rects: Vec<(i32, i32, i32, i32)>,
}

pub fn controls_geometry(cfg: &ControlsConfig, width: i32, height: i32) -> ControlsGeometry {
    let w = f64::from(width);
    let h = f64::from(height);
    let size = f64::from(cfg.size);
    let gap = CONTROLS_GAP;
    let controls_w = if cfg.enabled {
        size * 3.0 + gap * 2.0
    } else {
        0.0
    };
    let controls_left = ((w - controls_w) / 2.0).max(0.0);
    let top = (h - f64::from(cfg.bottom_margin) - size).max(0.0);

    let buttons = if cfg.enabled {
        (0..3)
            .map(|i| {
                let cx = controls_left + i as f64 * (size + gap) + size / 2.0;
                (cx, top + size / 2.0, size / 2.0)
            })
            .collect()
    } else {
        Vec::new()
    };

    let handle_x = (w - f64::from(cfg.right_margin) - HANDLE_WIDTH).max(0.0);
    let handle = (handle_x, top, HANDLE_WIDTH, size);
    let hide_size = 28.0;
    let hide: (f64, f64, f64, f64) = (16.0, top + (size - hide_size) / 2.0, hide_size, hide_size);
    let pad = 6.0;
    let mut input_rects = Vec::new();
    input_rects.push((
        (hide.0 - pad).max(0.0).round() as i32,
        (hide.1 - pad).max(0.0).round() as i32,
        (hide.2 + pad * 2.0).round() as i32,
        (hide.3 + pad * 2.0).round() as i32,
    ));
    if cfg.enabled {
        input_rects.push((
            (controls_left - pad).max(0.0).round() as i32,
            (top - pad).max(0.0).round() as i32,
            (controls_w + pad * 2.0).round() as i32,
            (size + pad * 2.0).round() as i32,
        ));
    }
    input_rects.push((
        (handle_x - pad).max(0.0).round() as i32,
        (top - pad).max(0.0).round() as i32,
        (HANDLE_WIDTH + pad * 2.0).round() as i32,
        (size + pad * 2.0).round() as i32,
    ));

    ControlsGeometry {
        buttons,
        handle,
        hide,
        input_rects,
    }
}

pub fn input_region_rects(
    cfg: &ControlsConfig,
    width: i32,
    height: i32,
) -> Vec<(i32, i32, i32, i32)> {
    controls_geometry(cfg, width, height).input_rects
}

pub fn controls_hit(
    cfg: &ControlsConfig,
    width: i32,
    height: i32,
    x: f64,
    y: f64,
) -> Option<usize> {
    for (i, (cx, cy, r)) in controls_geometry(cfg, width, height)
        .buttons
        .into_iter()
        .enumerate()
    {
        let dx = x - cx;
        let dy = y - cy;
        if dx * dx + dy * dy <= r * r {
            return Some(i);
        }
    }
    None
}

pub fn handle_hit(cfg: &ControlsConfig, width: i32, height: i32, x: f64, y: f64) -> bool {
    let (hx, hy, hw, hh) = controls_geometry(cfg, width, height).handle;
    x >= hx - 4.0 && x <= hx + hw + 4.0 && y >= hy - 4.0 && y <= hy + hh + 4.0
}

pub fn hide_hit(cfg: &ControlsConfig, width: i32, height: i32, x: f64, y: f64) -> bool {
    let (hx, hy, hw, hh) = controls_geometry(cfg, width, height).hide;
    x >= hx - 4.0 && x <= hx + hw + 4.0 && y >= hy - 4.0 && y <= hy + hh + 4.0
}

/// What the pointer is over at press time. `Pad` covers the input-region
/// padding around controls and is draggable but not clickable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressTarget {
    Prev,
    PlayPause,
    Next,
    Hide,
    Handle,
    Pad,
}

pub fn press_target(
    cfg: &ControlsConfig,
    width: i32,
    height: i32,
    x: f64,
    y: f64,
) -> Option<PressTarget> {
    // Hide and handle are drawn above the controls; keep the original
    // hide-first priority when a very large control circle overlaps them.
    if hide_hit(cfg, width, height, x, y) {
        return Some(PressTarget::Hide);
    }
    if handle_hit(cfg, width, height, x, y) {
        return Some(PressTarget::Handle);
    }
    if let Some(index) = controls_hit(cfg, width, height, x, y) {
        return Some(match index {
            0 => PressTarget::Prev,
            1 => PressTarget::PlayPause,
            _ => PressTarget::Next,
        });
    }
    if input_contains(cfg, width, height, x, y) {
        return Some(PressTarget::Pad);
    }
    None
}

pub fn input_contains(cfg: &ControlsConfig, width: i32, height: i32, x: f64, y: f64) -> bool {
    controls_geometry(cfg, width, height)
        .input_rects
        .iter()
        .any(|(ix, iy, iw, ih)| {
            x >= f64::from(*ix)
                && x <= f64::from(*ix + *iw)
                && y >= f64::from(*iy)
                && y <= f64::from(*iy + *ih)
        })
}

/// Returns `None` only when the temporary Cairo surface cannot be created, so
/// the caller can retry on the next poll instead of caching a fallback width.
pub fn measure_card_width(cfg: &Config, line: Option<&LyricLine>) -> Option<i32> {
    let Some(line) = line else {
        return Some(600);
    };
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).ok()?;
    let cr = Context::new(&surface).ok()?;
    let margin = 24.0f64;
    let mut text_width = 0.0f64;
    let main = line_text(line);
    if !main.is_empty() {
        let layout = make_layout(
            &cr,
            &cfg.font_family,
            cfg.font_size,
            cfg.font_weight,
            &main,
            10000,
        );
        text_width = f64::from(layout.pixel_extents().0.width());
    }
    if cfg.show_translation && !line.translated.trim().is_empty() {
        let layout = make_layout(
            &cr,
            &cfg.font_family,
            cfg.translation_px(),
            400,
            line.translated.trim(),
            10000,
        );
        text_width = text_width.max(f64::from(layout.pixel_extents().0.width()));
    }
    if cfg.show_romanization && !line.roman.trim().is_empty() {
        let layout = make_layout(
            &cr,
            &cfg.font_family,
            (cfg.translation_px() - 2).max(8),
            400,
            line.roman.trim(),
            10000,
        );
        text_width = text_width.max(f64::from(layout.pixel_extents().0.width()));
    }

    let controls_total = f64::from(cfg.controls.size) * 3.0
        + CONTROLS_GAP * 2.0
        + HANDLE_GAP
        + HANDLE_WIDTH
        + f64::from(cfg.controls.right_margin)
        + 24.0;
    let horizontal_padding = f64::from(cfg.background.padding_x) * 2.0;
    let content = if text_width <= 0.0 {
        controls_total
    } else {
        (text_width + margin * 2.0 + horizontal_padding).max(controls_total)
    };
    Some((content.round() as i32).clamp(320, 4096))
}

#[derive(Debug, Clone, Copy)]
pub struct CardRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HoverState {
    pub control: Option<usize>,
    pub handle: bool,
    pub hide: bool,
}

pub fn draw_overlay(
    cr: &Context,
    card: CardRect,
    scene: &LyricScene,
    cfg: &Config,
    playing: bool,
    hover: HoverState,
) {
    let width = card.width;
    let height = card.height;
    let controls = &cfg.controls;
    let controls_h = if controls.enabled {
        (controls.size + controls.bottom_margin).max(0)
    } else {
        0
    };
    let lyric_h = (height - controls_h).max(60);
    let _ = cr.save();
    cr.translate(f64::from(card.x), f64::from(card.y));
    cr.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
    cr.clip();
    let font_h = f64::from(cfg.font_size) * 1.35;
    let main_top = ((f64::from(lyric_h) - font_h) / 2.0).max(0.0);
    let has_translation = cfg.show_translation
        && scene
            .line
            .map(|line| !line.translated.trim().is_empty())
            .unwrap_or(false);
    let trans_h = if has_translation {
        f64::from(cfg.translation_px()) * 1.35 + 6.0
    } else {
        0.0
    };
    let text_bottom = main_top + font_h + trans_h;
    let controls_bottom = if controls.enabled {
        f64::from(height - controls.bottom_margin)
    } else {
        0.0
    };
    let pad_y = f64::from(cfg.background.padding_y);
    let shadow_top = (main_top - pad_y).max(0.0);
    let shadow_bottom = (text_bottom.max(controls_bottom) + pad_y).min(f64::from(height));
    draw_card_background(
        cr,
        width,
        shadow_top,
        shadow_bottom.max(shadow_top + 1.0),
        &cfg.background,
    );
    draw_cached_lyrics(cr, width, lyric_h, scene);

    let geometry = controls_geometry(controls, width, height);
    if controls.enabled {
        draw_controls(cr, &geometry, controls, playing, hover.control);
    }
    draw_handle(cr, &geometry, hover.handle, controls.opacity);
    draw_hide_button(cr, &geometry, hover.hide, controls.opacity);
    let _ = cr.restore();
}

fn draw_hide_button(cr: &Context, geometry: &ControlsGeometry, hovered: bool, opacity: f64) {
    let (hx, hy, hw, hh) = geometry.hide;
    let cx = hx + hw / 2.0;
    let cy = hy + hh / 2.0;
    let alpha = if hovered { 0.95 } else { 0.75 };
    cr.set_source_rgba(1.0, 1.0, 1.0, alpha * opacity);
    cr.set_line_width(2.0);
    cr.move_to(cx - 7.0, cy - 2.0);
    cr.line_to(cx, cy + 5.0);
    cr.line_to(cx + 7.0, cy - 2.0);
    let _ = cr.stroke();
}

fn draw_handle(cr: &Context, geometry: &ControlsGeometry, hovered: bool, opacity: f64) {
    let (hx, hy, hw, hh) = geometry.handle;
    let cx = hx + hw / 2.0;
    let cy = hy + hh / 2.0;
    let alpha = if hovered { 0.98 } else { 0.72 };
    cr.set_source_rgba(1.0, 1.0, 1.0, alpha * opacity);
    for row in -1..=1 {
        for col in 0..2 {
            let x = cx + (col as f64 - 0.5) * 5.0;
            let y = cy + row as f64 * 5.0;
            cr.arc(x, y, 1.15, 0.0, 2.0 * PI);
            let _ = cr.fill();
        }
    }
}

fn draw_controls(
    cr: &Context,
    geometry: &ControlsGeometry,
    cfg: &ControlsConfig,
    playing: bool,
    hover: Option<usize>,
) {
    cr.set_antialias(cairo::Antialias::Gray);
    for (i, (cx, cy, r)) in geometry.buttons.iter().copied().enumerate() {
        let hovered = hover == Some(i);
        let alpha = if hovered { 1.0 } else { 0.86 };
        cr.set_source_rgba(1.0, 1.0, 1.0, alpha * cfg.opacity);
        let u = r * 0.34;
        match i {
            0 => {
                // previous: bar + left triangle
                cr.rectangle(cx - u - 2.0, cy - u, 2.4, u * 2.0);
                let _ = cr.fill();
                cr.move_to(cx + u, cy - u);
                cr.line_to(cx - u + 1.0, cy);
                cr.line_to(cx + u, cy + u);
                cr.close_path();
                let _ = cr.fill();
            }
            1 => {
                if playing {
                    let bar_w = u * 0.55;
                    cr.rectangle(cx - u, cy - u, bar_w, u * 2.0);
                    cr.rectangle(cx + u - bar_w, cy - u, bar_w, u * 2.0);
                    let _ = cr.fill();
                } else {
                    cr.move_to(cx - u * 0.7, cy - u);
                    cr.line_to(cx + u, cy);
                    cr.line_to(cx - u * 0.7, cy + u);
                    cr.close_path();
                    let _ = cr.fill();
                }
            }
            _ => {
                // next: right triangle + bar
                cr.move_to(cx - u, cy - u);
                cr.line_to(cx + u - 1.0, cy);
                cr.line_to(cx - u, cy + u);
                cr.close_path();
                let _ = cr.fill();
                cr.rectangle(cx + u - 0.4, cy - u, 2.4, u * 2.0);
                let _ = cr.fill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ControlsConfig;

    #[test]
    fn parses_colors() {
        let (r, g, b) = parse_color("#fe7971");
        assert!((r - 254.0 / 255.0).abs() < 1e-6);
        assert!((g - 121.0 / 255.0).abs() < 1e-6);
        assert!((b - 113.0 / 255.0).abs() < 1e-6);
        assert_eq!(parse_color("nope"), (1.0, 1.0, 1.0));
    }

    #[test]
    fn controls_hit_three_targets() {
        let cfg = ControlsConfig::default();
        let width = 800;
        let height = 200;
        let geometry = controls_geometry(&cfg, width, height);
        for (i, (cx, cy, _r)) in geometry.buttons.iter().copied().enumerate() {
            assert_eq!(controls_hit(&cfg, width, height, cx, cy), Some(i));
        }
        assert_eq!(controls_hit(&cfg, width, height, 5.0, 2.0), None);
        assert_eq!(controls_hit(&cfg, width, height, 300.0, 22.0), None);

        let (hx, hy, hw, hh) = geometry.handle;
        assert!(handle_hit(
            &cfg,
            width,
            height,
            hx + hw / 2.0,
            hy + hh / 2.0
        ));
        assert!(!geometry.input_rects.is_empty());
        for (ix, iy, iw, ih) in &geometry.input_rects {
            assert!(*ix >= 0 && *iy >= 0 && *iw > 0 && *ih > 0);
        }
    }

    #[test]
    fn cache_key_tracks_text_but_not_background() {
        use crate::model::LyricWord;

        let line = LyricLine {
            words: vec![LyricWord {
                word: "hello".to_string(),
                ..Default::default()
            }],
            translated: "你好".to_string(),
            ..Default::default()
        };

        let base = {
            let cfg = Config::default();
            let scene = LyricScene {
                cfg: &cfg,
                line: Some(&line),
                time_ms: 0.0,
                placeholder: false,
            };
            cache_key(&scene, 800, 200)
        };

        let background_changed = {
            let mut cfg = Config::default();
            cfg.background.alpha = 0.9;
            cfg.background.padding_x = 1;
            let scene = LyricScene {
                cfg: &cfg,
                line: Some(&line),
                time_ms: 0.0,
                placeholder: false,
            };
            cache_key(&scene, 800, 200)
        };
        assert_eq!(base, background_changed);

        let font_changed = {
            let mut cfg = Config::default();
            cfg.font_size += 1;
            let scene = LyricScene {
                cfg: &cfg,
                line: Some(&line),
                time_ms: 0.0,
                placeholder: false,
            };
            cache_key(&scene, 800, 200)
        };
        assert_ne!(base, font_changed);
    }

    #[test]
    fn press_target_maps_all_interactive_zones() {
        let cfg = ControlsConfig::default();
        let width = 800;
        let height = 200;
        let geometry = controls_geometry(&cfg, width, height);

        for (index, target) in [
            (0, PressTarget::Prev),
            (1, PressTarget::PlayPause),
            (2, PressTarget::Next),
        ] {
            let (cx, cy, _) = geometry.buttons[index];
            assert_eq!(press_target(&cfg, width, height, cx, cy), Some(target));
        }

        let (hx, hy, hw, hh) = geometry.hide;
        assert_eq!(
            press_target(&cfg, width, height, hx + hw / 2.0, hy + hh / 2.0),
            Some(PressTarget::Hide)
        );
        let (hx, hy, hw, hh) = geometry.handle;
        assert_eq!(
            press_target(&cfg, width, height, hx + hw / 2.0, hy + hh / 2.0),
            Some(PressTarget::Handle)
        );

        // Inside the padded input rect but outside every control's shape.
        let controls_left =
            (f64::from(width) - 3.0 * f64::from(cfg.size) - 2.0 * CONTROLS_GAP) / 2.0;
        let top = f64::from(height - cfg.bottom_margin - cfg.size);
        assert_eq!(
            press_target(&cfg, width, height, controls_left - 4.0, top + 4.0),
            Some(PressTarget::Pad)
        );

        assert_eq!(press_target(&cfg, width, height, 400.0, 60.0), None);
    }

    #[test]
    fn hide_and_handle_win_over_overlapping_controls() {
        let large = ControlsConfig {
            size: 100,
            right_margin: 0,
            ..Default::default()
        };
        let geometry = controls_geometry(&large, 364, 200);
        let (hx, hy, hw, hh) = geometry.hide;
        assert_eq!(
            press_target(&large, 364, 200, hx + hw / 2.0, hy + hh / 2.0),
            Some(PressTarget::Hide)
        );

        let tight = ControlsConfig {
            size: 70,
            right_margin: 48,
            ..Default::default()
        };
        let geometry = controls_geometry(&tight, 322, 200);
        let (hx, hy, hw, hh) = geometry.handle;
        assert_eq!(
            press_target(&tight, 322, 200, hx + hw / 2.0, hy + hh / 2.0),
            Some(PressTarget::Handle)
        );
    }

    #[test]
    fn hide_button_hit_and_input_region() {
        let cfg = ControlsConfig::default();
        let width = 800;
        let height = 200;
        let geometry = controls_geometry(&cfg, width, height);
        let (hx, hy, hw, hh) = geometry.hide;

        let cx = hx + hw / 2.0;
        let cy = hy + hh / 2.0;
        assert!(hide_hit(&cfg, width, height, cx, cy));
        assert!(!hide_hit(&cfg, width, height, f64::from(width) / 2.0, cy));

        // The hide button must be covered by one of the interactive input rects.
        assert!(geometry.input_rects.iter().any(|(ix, iy, iw, ih)| {
            let ix = f64::from(*ix);
            let iy = f64::from(*iy);
            let iw = f64::from(*iw);
            let ih = f64::from(*ih);
            cx >= ix && cx <= ix + iw && cy >= iy && cy <= iy + ih
        }));
    }
}
