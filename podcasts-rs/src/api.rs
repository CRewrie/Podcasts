//! HTTP API – same routes and JSON shapes as app.py, so static/index.html works unchanged.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::app::{now, sanitize_country, short_id, App, Podcast};
use crate::jobs::{find_exe, Job};

const INDEX_HTML: &str = include_str!("../../static/index.html");

/// Unix seconds of the last HTTP request (open tabs poll /api/jobs).
static LAST_REQUEST: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------- errors

pub struct ApiError(StatusCode, String);

impl ApiError {
    pub fn new(code: StatusCode, msg: impl Into<String>) -> Self {
        Self(code, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            eprintln!("{}: {}", self.0, self.1);
        }
        (self.0, Json(json!({ "detail": self.1 }))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e.into()))
    }
}

type ApiResult<T = Value> = Result<Json<T>, ApiError>;
type AppState = State<Arc<App>>;

fn ok() -> ApiResult {
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- router

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/api/search", get(search))
        .route("/api/library", get(library))
        .route("/api/podcasts", post(add_podcast))
        .route("/api/podcasts/{pid}", get(podcast).patch(patch_podcast).delete(forget_podcast))
        .route("/api/podcasts/{pid}/status", get(podcast_status))
        .route("/api/jobs", get(list_jobs).post(create_job))
        .route("/api/jobs/clear", post(clear_jobs))
        .route("/api/jobs/{jid}/cancel", post(cancel_job))
        .route("/api/episodes/done", post(toggle_done))
        .route("/api/episodes/reveal", post(reveal))
        .route("/api/episodes/delete", post(delete_files))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/quit", post(quit))
        .layer(middleware::from_fn(touch))
        .with_state(app)
}

async fn touch(req: Request, next: Next) -> Response {
    LAST_REQUEST.store(now() as u64, Ordering::Relaxed);
    next.run(req).await
}

/// Exit once no browser tab has polled for `limit` seconds and no job is running.
pub async fn idle_watchdog(app: Arc<App>, limit: u64) {
    LAST_REQUEST.store(now() as u64, Ordering::Relaxed);
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let busy = app.jobs.lock().unwrap().iter().any(|j| j.is_active());
        if !busy && (now() as u64).saturating_sub(LAST_REQUEST.load(Ordering::Relaxed)) > limit {
            std::process::exit(0);
        }
    }
}

// ---------------------------------------------------------------- search & podcasts

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default)]
    country: String,
}

async fn search(State(app): AppState, Query(p): Query<SearchQuery>) -> ApiResult {
    let q = p.q.trim();
    if q.is_empty() {
        return Ok(Json(json!([])));
    }
    let country = {
        let mut st = app.state.lock().unwrap();
        let country = sanitize_country(if p.country.is_empty() { &st.settings.country } else { &p.country });
        if country != st.settings.country {
            st.settings.country = country.clone();
            app.save(&st);
        }
        country
    };
    let params = [("media", "podcast"), ("entity", "podcast"), ("term", q), ("limit", "40"), ("country", &country)];
    let mut attempt = 0;
    let data: Value = loop {
        let result: anyhow::Result<Value> = async {
            let resp = app.http.get("https://itunes.apple.com/search").query(&params).send().await?;
            Ok(resp.error_for_status()?.json().await?)
        }
        .await;
        match result {
            Ok(data) => break data,
            Err(e) if attempt == 2 => {
                let msg = format!("Apple-Podcast-Suche nicht erreichbar ({e:#})");
                return Err(ApiError::new(StatusCode::BAD_GATEWAY, msg));
            }
            Err(_) => {
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(attempt)).await;
            }
        }
    };
    let str_of = |it: &Value, key: &str| it[key].as_str().unwrap_or("").to_string();
    let results: Vec<Value> = data["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|it| {
            let feed = it["feedUrl"].as_str().filter(|s| !s.is_empty())?;
            let mut artwork = str_of(it, "artworkUrl600");
            if artwork.is_empty() {
                artwork = str_of(it, "artworkUrl100");
            }
            Some(json!({
                "feed": feed, "title": str_of(it, "collectionName"), "author": str_of(it, "artistName"),
                "artwork": artwork, "genre": str_of(it, "primaryGenreName"),
                "episode_count": it["trackCount"], "id": short_id(feed),
            }))
        })
        .collect();
    Ok(Json(json!(results)))
}

#[derive(Deserialize)]
struct PodcastIn {
    feed: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    artwork: String,
}

async fn add_podcast(State(app): AppState, Json(p): Json<PodcastIn>) -> ApiResult {
    let pid = app.register(p.feed.trim(), &p.title, &p.author, &p.artwork);
    if p.title.is_empty() {
        // RSS URL pasted directly: validate & fill metadata
        if let Err(e) = app.get_feed(&pid, true).await {
            let mut st = app.state.lock().unwrap();
            if st.podcasts.get(&pid).is_some_and(|pod| !pod.followed) {
                st.podcasts.remove(&pid);
                app.save(&st);
            }
            return Err(e);
        }
    }
    Ok(Json(json!({ "id": pid })))
}

fn library_entry(app: &App, pid: &str, pod: &Podcast) -> Value {
    let status = app.status_map(pid);
    json!({
        "id": pid, "title": pod.title, "author": pod.author, "artwork": pod.artwork,
        "followed": pod.followed, "episode_count": pod.episode_count,
        "downloaded": status.values().filter(|s| s.media).count(),
        "subtitled": status.values().filter(|s| s.subs || s.done.is_some()).count(),
        "last_opened": pod.last_opened,
    })
}

async fn library(State(app): AppState) -> ApiResult {
    let mut pods: Vec<(String, Podcast)> =
        app.state.lock().unwrap().podcasts.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    pods.sort_by(|(_, a), (_, b)| b.followed.cmp(&a.followed).then(b.last_opened.total_cmp(&a.last_opened)));
    Ok(Json(json!(pods.iter().map(|(pid, pod)| library_entry(&app, pid, pod)).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct PodcastQuery {
    #[serde(default)]
    refresh: bool,
}

async fn podcast(State(app): AppState, Path(pid): Path<String>, Query(q): Query<PodcastQuery>) -> ApiResult {
    let feed = app.get_feed(&pid, q.refresh).await?;
    let pod = {
        let mut st = app.state.lock().unwrap();
        let Some(pod) = st.podcasts.get_mut(&pid) else {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "Unbekannter Podcast"));
        };
        pod.last_opened = now();
        let pod = pod.clone();
        app.save(&st);
        pod
    };
    let mut entry = library_entry(&app, &pid, &pod);
    entry["description"] = json!(feed.info.description);
    entry["folder"] = json!(app.library.join(&pod.dir).to_string_lossy());
    Ok(Json(json!({ "podcast": entry, "episodes": feed.episodes, "status": app.status_map(&pid) })))
}

async fn podcast_status(State(app): AppState, Path(pid): Path<String>) -> ApiResult {
    Ok(Json(json!(app.status_map(&pid))))
}

#[derive(Deserialize)]
struct PodcastPatch {
    followed: Option<bool>,
}

async fn patch_podcast(State(app): AppState, Path(pid): Path<String>, Json(p): Json<PodcastPatch>) -> ApiResult {
    let mut st = app.state.lock().unwrap();
    let pod = st.podcasts.get_mut(&pid).ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unbekannter Podcast"))?;
    if let Some(followed) = p.followed {
        pod.followed = followed;
    }
    app.save(&st);
    ok()
}

/// Remove from the list (downloaded files stay on disk).
async fn forget_podcast(State(app): AppState, Path(pid): Path<String>) -> ApiResult {
    let mut st = app.state.lock().unwrap();
    st.podcasts.remove(&pid);
    app.save(&st);
    ok()
}

// ---------------------------------------------------------------- jobs

fn all_jobs(app: &App) -> Vec<Arc<Job>> {
    app.jobs.lock().unwrap().clone()
}

#[derive(Deserialize)]
struct JobIn {
    podcast_id: String,
    episode_id: String,
    #[serde(default = "yes")]
    subs: bool,
}

fn yes() -> bool {
    true
}

async fn create_job(State(app): AppState, Json(j): Json<JobIn>) -> ApiResult {
    let running = all_jobs(&app)
        .into_iter()
        .find(|job| job.pid == j.podcast_id && job.episode.id == j.episode_id && job.is_active());
    if let Some(job) = running {
        if j.subs {
            job.subs.store(true, Ordering::Relaxed);
        }
        return Ok(Json(job.public(&app)));
    }
    let feed = app.get_feed(&j.podcast_id, false).await?;
    let episode = feed.episodes.iter().find(|e| e.id == j.episode_id).cloned();
    let episode = episode.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Episode nicht im Feed gefunden"))?;
    let job = Job::new(j.podcast_id, episode, j.subs);
    app.jobs.lock().unwrap().push(job.clone());
    let _ = app.queue.send(job.clone());
    Ok(Json(job.public(&app)))
}

async fn list_jobs(State(app): AppState) -> ApiResult {
    Ok(Json(json!(all_jobs(&app).iter().map(|j| j.public(&app)).collect::<Vec<_>>())))
}

async fn cancel_job(State(app): AppState, Path(jid): Path<String>) -> ApiResult {
    if let Some(job) = all_jobs(&app).into_iter().find(|j| j.id == jid) {
        job.cancel.cancel();
        job.update_if_queued_canceled();
    }
    ok()
}

async fn clear_jobs(State(app): AppState) -> ApiResult {
    app.jobs.lock().unwrap().retain(|j| j.is_active());
    ok()
}

async fn quit(State(app): AppState) -> ApiResult {
    for job in all_jobs(&app) {
        job.cancel.cancel();
    }
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::process::exit(0);
    });
    ok()
}

// ---------------------------------------------------------------- episodes

#[derive(Deserialize)]
struct EpisodeRef {
    podcast_id: String,
    episode_id: String,
}

async fn toggle_done(State(app): AppState, Json(e): Json<EpisodeRef>) -> ApiResult {
    let key = format!("{}/{}", e.podcast_id, e.episode_id);
    let mut st = app.state.lock().unwrap();
    if st.done.remove(&key).is_none() {
        st.done.insert(key.clone(), true);
    }
    let done = st.done.contains_key(&key);
    app.save(&st);
    Ok(Json(json!({ "done": done })))
}

async fn reveal(State(app): AppState, Json(e): Json<EpisodeRef>) -> ApiResult {
    let media = app.scan_files(&e.podcast_id).remove(&e.episode_id).and_then(|f| f.media);
    if let Some(media) = media {
        Command::new("open").arg("-R").arg(media).status().await?;
    } else {
        let dir = app.podcast_dir(&e.podcast_id);
        let dir = dir.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unbekannter Podcast"))?;
        tokio::fs::create_dir_all(&dir).await?;
        Command::new("open").arg(dir).status().await?;
    }
    ok()
}

async fn delete_files(State(app): AppState, Json(e): Json<EpisodeRef>) -> ApiResult {
    if let Some(files) = app.scan_files(&e.podcast_id).remove(&e.episode_id) {
        for f in files.media.iter().chain(&files.subs) {
            let _ = tokio::fs::remove_file(f).await;
        }
    }
    ok()
}

// ---------------------------------------------------------------- settings

async fn get_settings(State(app): AppState) -> ApiResult {
    let mut cached: Vec<String> = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache/huggingface/hub"))
        .and_then(|hub| std::fs::read_dir(hub).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|d| d.file_name().to_str()?.strip_prefix("models--").map(|n| n.replace("--", "/")))
        .filter(|n| n.to_lowercase().contains("whisper"))
        .collect();
    cached.sort();
    let mut v = json!(app.state.lock().unwrap().settings);
    v["cached_models"] = json!(cached);
    v["has_mlx"] = json!(find_exe("mlx_whisper").is_some());
    v["has_openai"] = json!(find_exe("whisper").is_some());
    Ok(Json(v))
}

#[derive(Deserialize)]
struct SettingsIn {
    engine: String,
    model: String,
    #[serde(default)]
    language: String,
}

async fn put_settings(State(app): AppState, Json(s): Json<SettingsIn>) -> ApiResult {
    let mut st = app.state.lock().unwrap();
    st.settings.engine = s.engine;
    st.settings.model = s.model.trim().to_string();
    st.settings.language = s.language.trim().to_string();
    app.save(&st);
    Ok(Json(json!(st.settings)))
}
