use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct NowPlaying {
    pub track_id: Option<String>,
    pub title: String,
    pub artists: Vec<String>,
    pub duration_ms: f64,
    pub position_ms: f64,
    pub playing: bool,
    pub speed: f64,
    pub lyric_offset_ms: f64,
    pub lyric_available: bool,
    pub lyric_line_count: i64,
    pub send_timestamp_ms: f64,
}

#[derive(Debug, Clone, Default)]
pub struct LyricLine {
    pub words: Vec<LyricWord>,
    pub translated: String,
    pub roman: String,
    pub start_time: f64,
    pub end_time: f64,
    pub is_bg: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LyricWord {
    pub word: String,
    pub start_time: f64,
    pub end_time: f64,
}

#[derive(Debug, Deserialize)]
struct NowPlayingWire {
    #[serde(default)]
    track: Option<TrackWire>,
    #[serde(default)]
    position: Option<f64>,
    #[serde(default)]
    playing: Option<bool>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    speed: Option<f64>,
    #[serde(rename = "lyricOffsetMs", default)]
    lyric_offset_ms: Option<f64>,
    #[serde(rename = "lyricAvailable", default)]
    lyric_available: Option<bool>,
    #[serde(rename = "lyricLineCount", default)]
    lyric_line_count: Option<i64>,
    #[serde(rename = "sendTimestamp", default)]
    send_timestamp: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct TrackWire {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artists: Option<Vec<ArtistWire>>,
    #[serde(default)]
    duration: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ArtistWire {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LyricsWire {
    #[serde(rename = "trackId", default)]
    track_id: Option<Value>,
    #[serde(default)]
    lyric: Option<Vec<LyricLineWire>>,
    #[serde(rename = "lyricOffsetMs", default)]
    lyric_offset_ms: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct LyricLineWire {
    #[serde(default)]
    words: Option<Vec<LyricWordWire>>,
    #[serde(rename = "translatedLyric", default)]
    translated: Option<String>,
    #[serde(rename = "romanLyric", default)]
    roman: Option<String>,
    #[serde(rename = "startTime", default)]
    start_time: Option<f64>,
    #[serde(rename = "endTime", default)]
    end_time: Option<f64>,
    #[serde(rename = "isBG", default)]
    is_bg: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct LyricWordWire {
    #[serde(default)]
    word: Option<String>,
    #[serde(rename = "startTime", default)]
    start_time: Option<f64>,
    #[serde(rename = "endTime", default)]
    end_time: Option<f64>,
}

fn value_to_id(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub fn parse_now_playing(body: &str) -> Result<NowPlaying> {
    let wire: NowPlayingWire =
        serde_json::from_str(body).context("invalid /api/now-playing JSON")?;
    let track = wire.track;
    let track_id = track
        .as_ref()
        .and_then(|t| t.id.as_ref())
        .and_then(value_to_id);
    let title = track
        .as_ref()
        .and_then(|t| t.title.clone())
        .unwrap_or_default();
    let artists = track
        .as_ref()
        .and_then(|t| t.artists.as_ref())
        .map(|list| {
            list.iter()
                .filter_map(|artist| artist.name.clone())
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let duration = track
        .as_ref()
        .and_then(|t| t.duration)
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(0.0);
    let state = wire.state.unwrap_or_default();
    let playing = wire
        .playing
        .unwrap_or_else(|| state.eq_ignore_ascii_case("playing"));
    let speed = wire
        .speed
        .filter(|s| s.is_finite() && *s > 0.05)
        .unwrap_or(1.0);

    Ok(NowPlaying {
        track_id,
        title,
        artists,
        duration_ms: duration,
        position_ms: wire.position.unwrap_or(0.0).max(0.0),
        playing,
        speed,
        lyric_offset_ms: wire.lyric_offset_ms.unwrap_or(0.0),
        lyric_available: wire.lyric_available.unwrap_or(false),
        lyric_line_count: wire.lyric_line_count.unwrap_or(0),
        send_timestamp_ms: wire.send_timestamp.unwrap_or(0.0),
    })
}

/// Returns (trackId, lines, lyricOffsetMs)
pub fn parse_lyrics(body: &str) -> Result<(Option<String>, Vec<LyricLine>, f64)> {
    let wire: LyricsWire = serde_json::from_str(body).context("invalid /api/lyrics JSON")?;
    let lines = wire
        .lyric
        .unwrap_or_default()
        .into_iter()
        .map(|line| {
            let words = line
                .words
                .unwrap_or_default()
                .into_iter()
                .map(|w| LyricWord {
                    word: w.word.unwrap_or_default(),
                    start_time: w.start_time.unwrap_or(0.0),
                    end_time: w.end_time.unwrap_or(0.0),
                })
                .collect();
            LyricLine {
                words,
                translated: line.translated.unwrap_or_default(),
                roman: line.roman.unwrap_or_default(),
                start_time: line.start_time.unwrap_or(0.0),
                end_time: line.end_time.unwrap_or(0.0),
                is_bg: line.is_bg.unwrap_or(false),
            }
        })
        .collect();
    Ok((
        wire.track_id.as_ref().and_then(value_to_id),
        lines,
        wire.lyric_offset_ms.unwrap_or(0.0),
    ))
}

pub fn word_timing_usable(line: &LyricLine) -> bool {
    line.words.len() > 1 && line.words[0].end_time > line.words[0].start_time
}

pub fn pick_primary_index(lines: &[LyricLine], time_ms: f64) -> Option<usize> {
    if lines.is_empty() {
        return None;
    }
    if time_ms < lines[0].start_time {
        return Some(0);
    }

    // Upper-bound binary search: last line with start_time <= time_ms.
    let mut lo = 0usize;
    let mut hi = lines.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if lines[mid].start_time <= time_ms {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let mut idx = lo.saturating_sub(1);

    if lines[idx].is_bg {
        // Search backwards (including the current line) for a non-background
        // line that is still active at this time.
        for j in (0..=idx).rev() {
            let line = &lines[j];
            if !line.is_bg && time_ms >= line.start_time && time_ms < line.end_time {
                idx = j;
                break;
            }
        }
    }

    if time_ms >= lines[idx].end_time && idx + 1 < lines.len() {
        idx += 1;
    }
    Some(idx)
}

pub fn line_progress(line: &LyricLine, time_ms: f64) -> f64 {
    let duration = (line.end_time - line.start_time).abs();
    if duration <= 0.0 {
        return if time_ms >= line.start_time { 1.0 } else { 0.0 };
    }
    ((time_ms - line.start_time) / duration).clamp(0.0, 1.0)
}

pub fn word_progress(line: &LyricLine, word: &LyricWord, time_ms: f64) -> f64 {
    let word_duration = (word.end_time - word.start_time).abs().max(1.0);
    let pre_roll = 80.0f64.min(word_duration * 0.3);
    let adjusted_start = line.start_time.max(word.start_time - pre_roll);
    let span = word.end_time - adjusted_start;
    if span.abs() <= f64::EPSILON {
        return if time_ms >= word.end_time { 1.0 } else { 0.0 };
    }
    ((time_ms - adjusted_start) / span).clamp(0.0, 1.0)
}

pub fn line_text(line: &LyricLine) -> String {
    line.words.iter().map(|w| w.word.as_str()).collect()
}

#[allow(dead_code)]
pub fn require_track_id(id: &Option<String>) -> Result<&str> {
    id.as_deref().ok_or_else(|| anyhow!("missing track id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(start: f64, end: f64, is_bg: bool) -> LyricLine {
        LyricLine {
            start_time: start,
            end_time: end,
            is_bg,
            ..Default::default()
        }
    }

    #[test]
    fn picks_first_before_start() {
        let lines = vec![line(1000.0, 2000.0, false), line(3000.0, 4000.0, false)];
        assert_eq!(pick_primary_index(&lines, 0.0), Some(0));
        assert_eq!(pick_primary_index(&[], 0.0), None);
    }

    #[test]
    fn picks_latest_started_line_and_advances() {
        let lines = vec![
            line(1000.0, 2000.0, false),
            line(3000.0, 4000.0, false),
            line(5000.0, 6000.0, false),
        ];
        assert_eq!(pick_primary_index(&lines, 1999.0), Some(0));
        // A gap after the current line advances to the next line.
        assert_eq!(pick_primary_index(&lines, 2999.0), Some(1));
        assert_eq!(pick_primary_index(&lines, 4000.0), Some(2));
        assert_eq!(pick_primary_index(&lines, 9000.0), Some(2));
    }

    #[test]
    fn skips_background_lines() {
        let lines = vec![
            line(1000.0, 3000.0, false),
            line(3200.0, 5000.0, true),
            line(5000.0, 7000.0, false),
        ];
        // No earlier non-BG line is active, so the BG line itself is used.
        assert_eq!(pick_primary_index(&lines, 3500.0), Some(1));
        // After the BG line's end, move to the next line.
        assert_eq!(pick_primary_index(&lines, 5200.0), Some(2));
    }

    #[test]
    fn word_progress_applies_preroll() {
        let mut l = line(1000.0, 3000.0, false);
        l.words = vec![
            LyricWord {
                word: "a".into(),
                start_time: 2000.0,
                end_time: 2400.0,
            },
            LyricWord {
                word: "b".into(),
                start_time: 2400.0,
                end_time: 2800.0,
            },
        ];
        let w = &l.words[0];
        assert_eq!(word_progress(&l, w, 1920.0), 0.0);
        // pre-roll = 80ms, so 1920..2400.
        assert!((word_progress(&l, w, 2160.0) - 0.5).abs() < 0.01);
        assert_eq!(word_progress(&l, w, 9999.0), 1.0);
        assert!(word_timing_usable(&l));
    }

    #[test]
    fn parses_now_playing_and_lyrics() {
        let np = parse_now_playing(
            r#"{"track":{"id":"42","title":"x","artists":[{"name":"A"},{"name":"B"}],"duration":1000},"position":10,"playing":true,"state":"playing","speed":1,"lyricOffsetMs":5,"lyricAvailable":true,"lyricLineCount":2,"sendTimestamp":1700000000000}"#,
        )
        .unwrap();
        assert_eq!(np.track_id.as_deref(), Some("42"));
        assert_eq!(np.artists, vec!["A".to_string(), "B".to_string()]);
        assert!(np.playing);
        assert_eq!(np.duration_ms, 1000.0);
        let (id, lines, off) = parse_lyrics(
            r#"{"trackId":"42","lyric":[{"words":[{"word":"hello ","startTime":0,"endTime":100}],"translatedLyric":null,"romanLyric":"","startTime":0,"endTime":100,"isBG":false}],"lyricOffsetMs":5}"#,
        )
        .unwrap();
        assert_eq!(id.as_deref(), Some("42"));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].words.len(), 1);
        assert_eq!(off, 5.0);
    }
}
