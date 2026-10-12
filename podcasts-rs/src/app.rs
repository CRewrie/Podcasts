//! Shared application state: state.json (same format as app.py), library folder scan, feed cache.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use axum::http::StatusCode;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::api::ApiError;
use crate::feed::{self, Feed};
use crate::jobs::Job;

pub const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) PodcastBrowser/1.0";
const FEED_TTL: Duration = Duration::from_secs(600);
const MEDIA_EXT: &[&str] = &["mp4", "mkv", "webm", "ogg", "mp3", "m4a", "aac"];
const SUB_EXT: &[&str] = &["srt", "vtt", "ass"];

static FILE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([0-9a-f]{10})\]\.([a-z0-9]+)$").unwrap());
static UNSAFE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[\\/:*?"<>|\x00-\x1f]"#).unwrap());
static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

// ---------------------------------------------------------------- state.json

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub engine: String, // "mlx" (mlx_whisper) or "openai" (whisper)
    pub model: String,
    pub language: String, // "" = auto-detect
    pub country: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            engine: "mlx".into(),
            model: "mlx-community/whisper-large-v3-turbo".into(),
            language: String::new(),
            country: "DE".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Podcast {
    pub feed: String,
    pub followed: bool,
    pub title: String,
    pub author: String,
    pub artwork: String,
    pub dir: String,
    pub last_opened: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_count: Option<usize>,
    /// fields written by other versions are kept as they are
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct State {
    pub podcasts: BTreeMap<String, Podcast>,
    /// "podcast_id/episode_id" -> true (manually marked as done)
    pub done: BTreeMap<String, bool>,
    pub settings: Settings,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------- helpers

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

pub fn short_id(value: &str) -> String {
    sha1_smol::Sha1::from(value).digest().to_string()[..10].to_string()
}

pub fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

pub fn safe_name(value: &str, limit: usize) -> String {
    let v = UNSAFE_RE.replace_all(value, " ");
    let v = WS_RE.replace_all(&v, " ");
    let v = truncate(v.trim_matches([' ', '.']), limit);
    let v = v.trim_end_matches([' ', '.']);
    if v.is_empty() { "untitled".into() } else { v.into() }
}

pub fn sanitize_country(value: &str) -> String {
    let c: String = value.chars().filter(char::is_ascii_alphabetic).take(2).collect();
    if c.is_empty() { "DE".into() } else { c.to_ascii_uppercase() }
}

#[derive(Default)]
pub struct Files {
    pub media: Option<PathBuf>,
    pub subs: Vec<PathBuf>,
}

#[derive(Serialize)]
pub struct EpStatus {
    pub media: bool,
    pub subs: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
}

// ---------------------------------------------------------------- app

pub struct App {
    pub library: PathBuf,
    state_path: PathBuf,
    pub state: Mutex<State>,
    feeds: Mutex<HashMap<String, (Instant, Arc<Feed>)>>,
    /// short requests (search, feeds, covers)
    pub http: reqwest::Client,
    /// episode downloads: no overall timeout, only per read
    pub dl: reqwest::Client,
    pub jobs: Mutex<Vec<Arc<Job>>>,
    pub queue: mpsc::UnboundedSender<Arc<Job>>,
}

impl App {
    pub fn new(root: PathBuf) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Arc<Job>>)> {
        let library = root.join("library");
        std::fs::create_dir_all(&library).with_context(|| format!("{} anlegen", library.display()))?;
        let state_path = root.join("state.json");
        let mut state: State = match std::fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} ist ungültig – nicht überschrieben", state_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => bail!("{} lesen: {e}", state_path.display()),
        };
        state.settings.country = sanitize_country(&state.settings.country);
        let http = reqwest::Client::builder().user_agent(UA).timeout(Duration::from_secs(30)).build()?;
        let dl = reqwest::Client::builder()
            .user_agent(UA)
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .build()?;
        let (queue, rx) = mpsc::unbounded_channel();
        let app = Self {
            library,
            state_path,
            state: Mutex::new(state),
            feeds: Mutex::default(),
            http,
            dl,
            jobs: Mutex::default(),
            queue,
        };
        Ok((Arc::new(app), rx))
    }

    pub fn save(&self, state: &State) {
        let tmp = self.state_path.with_extension("tmp");
        let result = serde_json::to_vec_pretty(state)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(std::fs::write(&tmp, bytes)?))
            .and_then(|()| Ok(std::fs::rename(&tmp, &self.state_path)?));
        if let Err(e) = result {
            eprintln!("state.json konnte nicht gespeichert werden: {e:#}");
        }
    }

    pub fn podcast_dir(&self, pid: &str) -> Option<PathBuf> {
        let st = self.state.lock().unwrap();
        st.podcasts.get(pid).map(|p| self.library.join(&p.dir))
    }

    /// episode_id -> files found in the podcast folder (`… [episode_id].ext`)
    pub fn scan_files(&self, pid: &str) -> HashMap<String, Files> {
        let mut out: HashMap<String, Files> = HashMap::new();
        let Some(dir) = self.podcast_dir(pid) else { return out };
        let Ok(entries) = std::fs::read_dir(dir) else { return out };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(caps) = FILE_RE.captures(&name) else { continue };
            let ext = caps.get(2).unwrap().as_str();
            let files = out.entry(caps[1].to_string()).or_default();
            if SUB_EXT.contains(&ext) {
                files.subs.push(entry.path());
            } else if MEDIA_EXT.contains(&ext) && (files.media.is_none() || ext == "mp4" || ext == "mkv") {
                // prefer mp4/mkv over raw audio if both exist
                files.media = Some(entry.path());
            }
        }
        out
    }

    pub fn status_map(&self, pid: &str) -> BTreeMap<String, EpStatus> {
        let mut out: BTreeMap<String, EpStatus> = self
            .scan_files(pid)
            .into_iter()
            .map(|(eid, f)| (eid, EpStatus { media: f.media.is_some(), subs: !f.subs.is_empty(), done: None }))
            .collect();
        let st = self.state.lock().unwrap();
        for key in st.done.keys() {
            if let Some((p, eid)) = key.split_once('/') {
                if p == pid {
                    out.entry(eid.to_string())
                        .or_insert(EpStatus { media: false, subs: false, done: None })
                        .done = Some(true);
                }
            }
        }
        out
    }

    pub fn register(&self, feed_url: &str, title: &str, author: &str, artwork: &str) -> String {
        let pid = short_id(feed_url);
        let mut st = self.state.lock().unwrap();
        let pod = st
            .podcasts
            .entry(pid.clone())
            .or_insert_with(|| Podcast { feed: feed_url.into(), ..Default::default() });
        for (field, value) in [(&mut pod.title, title), (&mut pod.author, author), (&mut pod.artwork, artwork)] {
            if !value.is_empty() {
                *field = value.into();
            }
        }
        if pod.dir.is_empty() {
            pod.dir = format!("{} [{pid}]", safe_name(if title.is_empty() { &pid } else { title }, 80));
        }
        pod.last_opened = now();
        self.save(&st);
        pid
    }

    pub async fn get_feed(&self, pid: &str, force: bool) -> Result<Arc<Feed>, ApiError> {
        if !force {
            if let Some((fetched, feed)) = self.feeds.lock().unwrap().get(pid) {
                if fetched.elapsed() < FEED_TTL {
                    return Ok(feed.clone());
                }
            }
        }
        let url = self.state.lock().unwrap().podcasts.get(pid).map(|p| p.feed.clone());
        let url = url.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unbekannter Podcast"))?;
        let fetched: Result<Feed> = async {
            let body = self.http.get(&url).send().await?.error_for_status()?.bytes().await?;
            feed::parse(&body)
        }
        .await;
        let feed = Arc::new(fetched.map_err(|e| {
            ApiError::new(StatusCode::BAD_GATEWAY, format!("Feed konnte nicht geladen werden: {e:#}"))
        })?);
        self.feeds.lock().unwrap().insert(pid.to_string(), (Instant::now(), feed.clone()));

        let mut st = self.state.lock().unwrap();
        if let Some(pod) = st.podcasts.get_mut(pid) {
            let info = &feed.info;
            for (field, value) in [(&mut pod.title, &info.title), (&mut pod.author, &info.author), (&mut pod.artwork, &info.artwork)] {
                if field.is_empty() {
                    *field = value.clone();
                }
            }
            pod.episode_count = Some(feed.episodes.len());
        }
        self.save(&st);
        Ok(feed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_ids() {
        assert_eq!(safe_name(r#"  a/b:c?  "d".. "#, 100), "a b c d");
        assert_eq!(safe_name("...", 100), "untitled");
        assert_eq!(sanitize_country("de\u{200b}"), "DE");
        assert_eq!(short_id("abc"), "a9993e3647");
    }
}
