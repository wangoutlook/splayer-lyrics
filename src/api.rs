use crate::logging;
use crate::model::{self, LyricLine, NowPlaying};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};
use ureq::Agent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAction {
    Play,
    Pause,
    Next,
    Prev,
}

impl ControlAction {
    pub fn endpoint(self) -> &'static str {
        match self {
            ControlAction::Play => "/api/play",
            ControlAction::Pause => "/api/pause",
            ControlAction::Next => "/api/next",
            ControlAction::Prev => "/api/prev",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ControlAction::Play => "play",
            ControlAction::Pause => "pause",
            ControlAction::Next => "next",
            ControlAction::Prev => "prev",
        }
    }
}

pub enum Command {
    FetchLyrics(String),
    ForcePoll,
    Control(ControlAction),
}

pub struct LyricsData {
    pub track_id: Option<String>,
    pub lines: Vec<LyricLine>,
    pub lyric_offset_ms: f64,
}

pub enum UiMsg {
    NowPlaying(Result<NowPlaying, String>),
    Lyrics {
        requested_track: String,
        result: Result<LyricsData, String>,
    },
    ControlDone {
        action: &'static str,
        ok: bool,
        error: Option<String>,
    },
    ToggleVisible,
    SetFontSize(i32),
    SetFontFamily(String),
    SetColor(String),
    FollowSystemColor,
    SetOutput(String),
    Quit,
}

fn agent() -> Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(1500))
        .timeout_read(Duration::from_millis(1500))
        .timeout_write(Duration::from_millis(1500))
        .build()
}

fn describe_ureq_error(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            let body = body.chars().take(160).collect::<String>();
            format!("HTTP {code}: {body}")
        }
        ureq::Error::Transport(t) => t.to_string(),
    }
}

fn fetch_now_playing(agent: &Agent, base_url: &str) -> Result<NowPlaying, String> {
    let url = format!("{base_url}/api/now-playing");
    let response = agent.get(&url).call().map_err(describe_ureq_error)?;
    let body = response
        .into_string()
        .map_err(|e| format!("read now-playing body: {e}"))?;
    model::parse_now_playing(&body).map_err(|e| format!("{e:#}"))
}

fn fetch_lyrics(agent: &Agent, base_url: &str, track_id: &str) -> Result<LyricsData, String> {
    let url = format!("{base_url}/api/lyrics");
    let response = agent.get(&url).call().map_err(describe_ureq_error)?;
    let body = response
        .into_string()
        .map_err(|e| format!("read lyrics body: {e}"))?;
    let (resp_track, lines, offset) =
        model::parse_lyrics(&body).map_err(|e| format!("{e:#}"))?;
    if let Some(resp_id) = &resp_track {
        if resp_id != track_id {
            logging::debug(&format!(
                "ignoring lyrics for track {resp_id} (current {track_id})"
            ));
        }
    }
    Ok(LyricsData {
        track_id: resp_track,
        lines,
        lyric_offset_ms: offset,
    })
}

fn post_control(base_url: String, action: ControlAction, tx: Sender<UiMsg>) {
    thread::spawn(move || {
        let agent = agent();
        let url = format!("{base_url}{}", action.endpoint());
        let result = agent.post(&url).send_string("");
        match result {
            Ok(response) => {
                let status = response.status();
                let body = response.into_string().unwrap_or_default();
                let ok = (200..300).contains(&status);
                if !ok {
                    let _ = tx.send(UiMsg::ControlDone {
                        action: action.name(),
                        ok: false,
                        error: Some(format!("HTTP {status}: {body}")),
                    });
                } else {
                    let _ = tx.send(UiMsg::ControlDone {
                        action: action.name(),
                        ok: true,
                        error: None,
                    });
                }
            }
            Err(err) => {
                let _ = tx.send(UiMsg::ControlDone {
                    action: action.name(),
                    ok: false,
                    error: Some(describe_ureq_error(err)),
                });
            }
        }
    });
}

pub fn spawn(base_url: String, poll_ms: u64, tx: Sender<UiMsg>) -> Sender<Command> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    thread::spawn(move || {
        let agent = agent();
        let mut next_poll = Instant::now();
        loop {
            let now = Instant::now();
            let wait = next_poll.saturating_duration_since(now);
            match cmd_rx.recv_timeout(wait) {
                Ok(Command::FetchLyrics(track_id)) => {
                    match fetch_lyrics(&agent, &base_url, &track_id) {
                        Ok(data) => {
                            let _ = tx.send(UiMsg::Lyrics {
                                requested_track: track_id,
                                result: Ok(data),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::Lyrics {
                                requested_track: track_id,
                                result: Err(err),
                            });
                        }
                    }
                }
                Ok(Command::ForcePoll) => {
                    next_poll = Instant::now();
                }
                Ok(Command::Control(action)) => {
                    post_control(base_url.clone(), action, tx.clone());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if Instant::now() >= next_poll {
                let result = fetch_now_playing(&agent, &base_url);
                if tx.send(UiMsg::NowPlaying(result)).is_err() {
                    break;
                }
                next_poll = Instant::now() + Duration::from_millis(poll_ms);
            }
        }
    });
    cmd_tx
}
