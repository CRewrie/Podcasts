//! Job queue: download → MP4 with cover (ffmpeg) → subtitles (Whisper), one job at a time.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{bail, Context, Result};
use chrono::{Local, TimeZone};
use regex::Regex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_stream::wrappers::SplitStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::app::{safe_name, truncate, App};
use crate::feed::Episode;

const ACTIVE: [&str; 4] = ["queued", "downloading", "converting", "transcribing"];

static TS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-->\s*(?:([0-9]+):)?([0-9]+):([0-9]+(?:\.[0-9]+)?)\]").unwrap());

pub struct Progress {
    pub state: &'static str,
    pub progress: f64,
    pub message: String,
}

pub struct Job {
    pub id: String,
    pub pid: String,
    pub episode: Episode,
    pub subs: AtomicBool,
    pub cancel: CancellationToken,
    pub status: Mutex<Progress>,
}

impl Job {
    pub fn new(pid: String, episode: Episode, subs: bool) -> Arc<Self> {
        Arc::new(Self {
            id: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
            pid,
            episode,
            subs: AtomicBool::new(subs),
            cancel: CancellationToken::new(),
            status: Mutex::new(Progress { state: "queued", progress: 0.0, message: String::new() }),
        })
    }

    pub fn set(&self, state: &'static str, progress: f64, message: &str) {
        *self.status.lock().unwrap() = Progress { state, progress, message: message.into() };
    }

    fn update(&self, f: impl FnOnce(&mut Progress)) {
        f(&mut self.status.lock().unwrap());
    }

    /// A queued job is skipped by the worker, so mark it as canceled right away.
    pub fn update_if_queued_canceled(&self) {
        self.update(|s| {
            if s.state == "queued" {
                s.state = "canceled";
            }
        });
    }

    pub fn is_active(&self) -> bool {
        ACTIVE.contains(&self.status.lock().unwrap().state)
    }

    pub fn public(&self, app: &App) -> Value {
        let podcast_title =
            app.state.lock().unwrap().podcasts.get(&self.pid).map(|p| p.title.clone()).unwrap_or_default();
        let st = self.status.lock().unwrap();
        json!({
            "id": self.id, "podcast_id": self.pid, "episode_id": self.episode.id,
            "title": self.episode.title, "podcast_title": podcast_title,
            "subs": self.subs.load(Ordering::Relaxed), "state": st.state,
            "progress": (st.progress * 1000.0).round() / 1000.0, "message": st.message,
        })
    }
}

#[derive(Debug)]
struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("abgebrochen")
    }
}

impl std::error::Error for Cancelled {}

pub async fn worker(app: Arc<App>, mut rx: mpsc::UnboundedReceiver<Arc<Job>>) {
    while let Some(job) = rx.recv().await {
        if job.cancel.is_cancelled() {
            continue;
        }
        let result = process(&app, &job).await;
        job.update(|s| match result {
            Ok(()) => *s = Progress { state: "done", progress: 1.0, message: String::new() },
            Err(e) if e.is::<Cancelled>() => (s.state, s.message) = ("canceled", String::new()),
            Err(e) => {
                let msg = format!("{e:#}");
                let skip = msg.chars().count().saturating_sub(1500);
                (s.state, s.message) = ("error", msg.chars().skip(skip).collect());
            }
        });
    }
}

fn base_name(ep: &Episode) -> String {
    let date = ep
        .date
        .and_then(|t| Local.timestamp_opt(t as i64, 0).single())
        .map_or_else(|| "0000-00-00".to_string(), |d| d.format("%Y-%m-%d").to_string());
    format!("{date} {} [{}]", safe_name(&ep.title, 110), ep.id)
}

/// File extension of the URL path (".mp3" if there is none).
fn url_ext(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    match last.rfind('.') {
        Some(i) if i > 0 && i + 1 < last.len() => last[i..].to_lowercase(),
        _ => ".mp3".into(),
    }
}

/// `path` + suffix, e.g. "x.mp3" -> "x.mp3.part"
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

async fn process(app: &App, job: &Job) -> Result<()> {
    let ep = &job.episode;
    let (folder, artwork) = {
        let st = app.state.lock().unwrap();
        let pod = st.podcasts.get(&job.pid).context("Podcast ist nicht mehr in der Liste")?;
        (app.library.join(&pod.dir), pod.artwork.clone())
    };
    tokio::fs::create_dir_all(&folder).await?;
    let base = base_name(ep);
    let existing = app.scan_files(&job.pid).remove(&ep.id).unwrap_or_default();

    let media = match existing.media {
        Some(media) => media,
        None => {
            job.set("downloading", 0.0, "");
            // ".orig.ext" is not matched by the library scan
            let raw = folder.join(format!("{base}.orig{}", url_ext(&ep.url)));
            download(app, job, &ep.url, &raw).await?;

            job.set("converting", 0.0, "");
            let ep_cover = folder.join(format!(".cover-{}", ep.id));
            let mut cover = None;
            if !ep.image.is_empty() {
                cover = fetch_cover(app, &ep.image, &ep_cover).await;
            }
            if cover.is_none() {
                cover = fetch_cover(app, &artwork, &folder.join(".cover")).await;
            }
            let media = folder.join(format!("{base}.mp4"));
            let result = make_mp4(job, &raw, cover.as_deref(), &media).await;
            let _ = tokio::fs::remove_file(&raw).await;
            if !ep.image.is_empty() {
                let _ = tokio::fs::remove_file(&ep_cover).await;
            }
            result?;
            media
        }
    };

    if job.subs.load(Ordering::Relaxed) && existing.subs.is_empty() {
        job.set("transcribing", 0.0, "Modell wird geladen…");
        let stem = media.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        transcribe(app, job, &media, &stem).await?;
    }
    Ok(())
}

async fn download(app: &App, job: &Job, url: &str, dest: &Path) -> Result<()> {
    let part = with_suffix(dest, ".part");
    let result: Result<()> = async {
        let resp = app.dl.get(url).send().await?.error_for_status()?;
        let total = resp.content_length().unwrap_or(0);
        let mut file = tokio::fs::File::create(&part).await?;
        let mut stream = resp.bytes_stream();
        let mut done = 0u64;
        loop {
            let chunk = tokio::select! {
                _ = job.cancel.cancelled() => return Err(Cancelled.into()),
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            done += chunk.len() as u64;
            job.update(|s| {
                if total > 0 {
                    s.progress = done as f64 / total as f64;
                }
                s.message = format!("{:.1} MB", done as f64 / 1e6);
                if total > 0 {
                    s.message += &format!(" / {:.1} MB", total as f64 / 1e6);
                }
            });
        }
        file.flush().await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result?;
    tokio::fs::rename(&part, dest).await?;
    Ok(())
}

async fn fetch_cover(app: &App, url: &str, dest: &Path) -> Option<PathBuf> {
    if url.is_empty() {
        return None;
    }
    if tokio::fs::metadata(dest).await.is_ok_and(|m| m.len() > 0) {
        return Some(dest.into());
    }
    let bytes = app.http.get(url).send().await.ok()?.error_for_status().ok()?.bytes().await.ok()?;
    tokio::fs::write(dest, &bytes).await.ok()?;
    Some(dest.into())
}

async fn probe(path: &Path, entries: &str, stream: Option<&str>) -> String {
    let mut cmd = Command::new("ffprobe");
    cmd.args(["-v", "error"]);
    if let Some(stream) = stream {
        cmd.args(["-select_streams", stream]);
    }
    cmd.args(["-show_entries", entries, "-of", "default=nw=1:nk=1"]).arg(path);
    match cmd.output().await {
        Ok(out) => String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string(),
        Err(_) => String::new(),
    }
}

async fn duration_of(path: &Path) -> f64 {
    probe(path, "format=duration", None).await.parse().unwrap_or(0.0)
}

/// Wrap the audio into an MP4 with a still cover image (H.264 + MP3/AAC) for Migaku.
async fn make_mp4(job: &Job, audio: &Path, cover: Option<&Path>, out: &Path) -> Result<()> {
    let codec = probe(audio, "stream=codec_name", Some("a:0")).await;
    let duration = duration_of(audio).await;
    let tmp = out.with_extension("tmp.mp4");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-y"]);
    match cover {
        Some(cover) => cmd.args(["-loop", "1", "-framerate", "1", "-i"]).arg(cover),
        None => cmd.args(["-f", "lavfi", "-i", "color=c=0x202020:s=720x720:r=1"]),
    };
    cmd.arg("-i").arg(audio).args([
        "-map", "0:v:0", "-map", "1:a:0", "-vf", "scale=720:-2,format=yuv420p", "-c:v", "libx264",
        "-tune", "stillimage", "-preset", "veryfast", "-r", "1",
    ]);
    if codec == "mp3" || codec == "aac" {
        cmd.args(["-c:a", "copy"]);
    } else {
        cmd.args(["-c:a", "aac", "-b:a", "160k"]);
    }
    cmd.args(["-shortest", "-movflags", "+faststart", "-progress", "pipe:1"]).arg(&tmp);

    let result = run(job, cmd, |line| {
        let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()) else {
            return;
        };
        if duration > 0.0 {
            job.update(|s| s.progress = (us / 1e6 / duration).clamp(0.0, 1.0));
        }
    })
    .await;
    match result {
        Ok(()) => Ok(tokio::fs::rename(&tmp, out).await?),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

/// Executable from PATH, falling back to ~/.local/bin (pipx).
pub fn find_exe(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let home = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin"));
    std::env::split_paths(&path)
        .chain(home)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

async fn transcribe(app: &App, job: &Job, media: &Path, base: &str) -> Result<()> {
    let s = app.state.lock().unwrap().settings.clone();
    let duration = duration_of(media).await;
    let folder = media.parent().unwrap_or(Path::new("."));
    let mut cmd;
    if s.engine == "openai" {
        cmd = Command::new(find_exe("whisper").unwrap_or_else(|| "whisper".into()));
        cmd.arg(media)
            .args(["--model", s.model.as_str(), "--output_format", "srt", "--output_dir"])
            .arg(folder)
            .args(["--verbose", "True"]);
    } else {
        cmd = Command::new(find_exe("mlx_whisper").unwrap_or_else(|| "mlx_whisper".into()));
        cmd.arg(media)
            .args(["--model", s.model.as_str(), "--output-format", "srt", "--output-dir"])
            .arg(folder)
            .args(["--output-name", base, "--verbose", "True"]);
    }
    if !s.language.is_empty() {
        cmd.args(["--language", s.language.as_str()]);
    }

    run(job, cmd, |line| {
        let Some(c) = TS_RE.captures(line) else { return };
        if duration <= 0.0 {
            return;
        }
        let num = |i: usize| c.get(i).map_or(0.0, |m| m.as_str().parse::<f64>().unwrap_or(0.0));
        let t = num(1) * 3600.0 + num(2) * 60.0 + num(3);
        let text = line.split_once(']').map_or("", |(_, rest)| rest).trim();
        job.update(|s| {
            s.progress = (t / duration).min(1.0);
            s.message = truncate(text, 80);
        });
    })
    .await
}

/// Run a command, feeding stdout+stderr lines to `on_line`; killed when the job is cancelled.
async fn run(job: &Job, mut cmd: Command, mut on_line: impl FnMut(&str)) -> Result<()> {
    let program = cmd.as_std().get_program().to_owned();
    let name = Path::new(&program).file_name().unwrap_or(&program).to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONUNBUFFERED", "1")
        .kill_on_drop(true);
    let mut child = cmd.spawn().with_context(|| format!("{name} konnte nicht gestartet werden"))?;
    let stdout = SplitStream::new(BufReader::new(child.stdout.take().unwrap()).split(b'\n'));
    let stderr = SplitStream::new(BufReader::new(child.stderr.take().unwrap()).split(b'\n'));
    let mut lines = stdout.merge(stderr);
    let mut tail = VecDeque::with_capacity(15);
    loop {
        tokio::select! {
            _ = job.cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(Cancelled.into());
            }
            next = lines.next() => match next {
                Some(Ok(raw)) => {
                    let line = String::from_utf8_lossy(&raw);
                    let line = line.trim_end();
                    on_line(line);
                    if tail.len() == 15 {
                        tail.pop_front();
                    }
                    tail.push_back(line.to_string());
                }
                _ => break,
            },
        }
    }
    let status = tokio::select! {
        _ = job.cancel.cancelled() => {
            let _ = child.kill().await;
            return Err(Cancelled.into());
        }
        status = child.wait() => status?,
    };
    if !status.success() {
        bail!("{name} fehlgeschlagen:\n{}", Vec::from(tail).join("\n"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(url_ext("https://x.com/a/b.MP3?x=1"), ".mp3");
        assert_eq!(url_ext("https://x.com/a/b"), ".mp3");
        assert_eq!(url_ext("https://x.com/a/ep.m4a#t"), ".m4a");
    }

    #[test]
    fn whisper_timestamps() {
        let c = TS_RE.captures("[01:02.500 --> 01:05.000]  こんにちは").unwrap();
        assert_eq!((c.get(1).is_none(), &c[2], &c[3]), (true, "01", "05.000"));
        let c = TS_RE.captures("[00:59:58.000 --> 01:00:01.000] x").unwrap();
        assert_eq!((&c[1], &c[2], &c[3]), ("01", "00", "01.000"));
    }
}
