#!/usr/bin/env python3
"""Local podcast browser: search podcasts, list episodes, download them as
Migaku-compatible MP4 files and optionally create subtitles with Whisper.

Start:  python3 app.py   ->  http://127.0.0.1:8765
"""
from __future__ import annotations

import email.utils
import hashlib
import html
import json
import os
import queue
import re
import shutil
import subprocess
import threading
import time
import traceback
import uuid
import webbrowser
import xml.etree.ElementTree as ET
from pathlib import Path

import httpx
import uvicorn
from fastapi import FastAPI, HTTPException
from fastapi.responses import FileResponse, JSONResponse
from pydantic import BaseModel

ROOT = Path(__file__).resolve().parent
LIBRARY = ROOT / "library"
STATE_FILE = ROOT / "state.json"
HOST, PORT = "127.0.0.1", 8765

MEDIA_EXT = {"mp4", "mkv", "webm", "ogg", "mp3", "m4a", "aac"}
SUB_EXT = {"srt", "vtt", "ass"}
FILE_RE = re.compile(r"\[([0-9a-f]{10})\]\.([a-z0-9]+)$")
ITUNES_NS = "{http://www.itunes.com/dtds/podcast-1.0.dtd}"
UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) PodcastBrowser/1.0"

DEFAULT_SETTINGS = {
    "engine": "mlx",  # "mlx" (mlx_whisper) or "openai" (whisper)
    "model": "mlx-community/whisper-large-v3-turbo",
    "language": "",  # "" = auto-detect
    "country": "DE",
}

# ---------------------------------------------------------------- state

_lock = threading.RLock()


def _load_state() -> dict:
    if STATE_FILE.exists():
        try:
            data = json.loads(STATE_FILE.read_text())
        except json.JSONDecodeError:
            data = {}
    else:
        data = {}
    data.setdefault("podcasts", {})
    data.setdefault("done", {})  # "podcast_id/episode_id" -> True (manually marked)
    data["settings"] = {**DEFAULT_SETTINGS, **data.get("settings", {})}
    data["settings"]["country"] = re.sub(r"[^A-Z]", "", data["settings"]["country"].upper())[:2] or "DE"
    return data


state = _load_state()


def save_state() -> None:
    with _lock:
        tmp = STATE_FILE.with_suffix(".tmp")
        tmp.write_text(json.dumps(state, ensure_ascii=False, indent=1))
        tmp.replace(STATE_FILE)


def short_id(value: str) -> str:
    return hashlib.sha1(value.encode("utf-8")).hexdigest()[:10]


def safe_name(value: str, limit: int = 100) -> str:
    value = re.sub(r'[\\/:*?"<>|\x00-\x1f]', " ", value)
    value = re.sub(r"\s+", " ", value).strip(" .")
    return value[:limit].rstrip(" .") or "untitled"


def podcast_dir(pid: str) -> Path:
    pod = state["podcasts"][pid]
    return LIBRARY / pod["dir"]


def scan_files(pid: str) -> dict[str, dict]:
    """episode_id -> {"media": Path|None, "subs": [Path]} from the podcast folder."""
    result: dict[str, dict] = {}
    if pid not in state["podcasts"]:
        return result
    folder = podcast_dir(pid)
    if not folder.is_dir():
        return result
    for f in folder.iterdir():
        m = FILE_RE.search(f.name)
        if not m:
            continue
        eid, ext = m.group(1), m.group(2)
        entry = result.setdefault(eid, {"media": None, "subs": []})
        if ext in SUB_EXT:
            entry["subs"].append(f)
        elif ext in MEDIA_EXT:
            # prefer mp4/mkv over raw audio if both exist
            if entry["media"] is None or ext in ("mp4", "mkv"):
                entry["media"] = f
    return result


def status_map(pid: str) -> dict[str, dict]:
    files = scan_files(pid)
    out = {}
    for eid, entry in files.items():
        out[eid] = {"media": entry["media"] is not None, "subs": bool(entry["subs"])}
    for key in state["done"]:
        p, eid = key.split("/", 1)
        if p == pid:
            out.setdefault(eid, {"media": False, "subs": False})["done"] = True
    return out


# ---------------------------------------------------------------- feeds

_http = httpx.Client(headers={"User-Agent": UA}, follow_redirects=True, timeout=30)
_feed_cache: dict[str, tuple[float, dict]] = {}
FEED_TTL = 600


def _text(el, tag: str) -> str:
    child = el.find(tag)
    return (child.text or "").strip() if child is not None and child.text else ""


def _parse_duration(raw: str) -> int | None:
    raw = raw.strip()
    if not raw:
        return None
    try:
        if ":" in raw:
            secs = 0
            for part in raw.split(":"):
                secs = secs * 60 + int(float(part))
            return secs
        return int(float(raw))
    except ValueError:
        return None


def _strip_html(raw: str) -> str:
    raw = re.sub(r"<[^>]+>", " ", raw)
    return re.sub(r"\s+", " ", html.unescape(raw)).strip()


EP_NUM_RE = re.compile(r"(?:#|ep(?:isode)?\.?\s*|folge\s*|第)\s*(\d{1,5})", re.I)


def parse_feed(xml_bytes: bytes) -> dict:
    root = ET.fromstring(xml_bytes)
    channel = root.find("channel")
    if channel is None:
        raise ValueError("Kein RSS-Channel gefunden")
    img = channel.find(f"{ITUNES_NS}image")
    artwork = img.get("href") if img is not None else ""
    if not artwork:
        artwork = _text(channel, "image/url")
    info = {
        "title": _text(channel, "title"),
        "author": _text(channel, f"{ITUNES_NS}author"),
        "artwork": artwork,
        "description": _strip_html(_text(channel, "description"))[:600],
    }
    episodes = []
    for item in channel.findall("item"):
        enc = item.find("enclosure")
        if enc is None or not enc.get("url"):
            continue
        url = enc.get("url")
        guid = _text(item, "guid") or url
        title = _text(item, "title") or "(ohne Titel)"
        pub = _text(item, "pubDate")
        ts = None
        if pub:
            try:
                ts = email.utils.parsedate_to_datetime(pub).timestamp()
            except (TypeError, ValueError):
                ts = None
        num_raw = _text(item, f"{ITUNES_NS}episode")
        number = int(num_raw) if num_raw.isdigit() else None
        if number is None:
            m = EP_NUM_RE.search(title)
            number = int(m.group(1)) if m else None
        ep_img = item.find(f"{ITUNES_NS}image")
        desc = _text(item, f"{ITUNES_NS}summary") or _text(item, "description")
        episodes.append({
            "id": short_id(guid),
            "title": title,
            "date": ts,
            "duration": _parse_duration(_text(item, f"{ITUNES_NS}duration")),
            "number": number,
            "url": url,
            "type": enc.get("type", ""),
            "image": ep_img.get("href") if ep_img is not None else "",
            "description": _strip_html(desc)[:500],
        })
    return {"info": info, "episodes": episodes}


def get_feed(pid: str, force: bool = False) -> dict:
    cached = _feed_cache.get(pid)
    if cached and not force and time.time() - cached[0] < FEED_TTL:
        return cached[1]
    pod = state["podcasts"].get(pid)
    if not pod:
        raise HTTPException(404, "Unbekannter Podcast")
    try:
        r = _http.get(pod["feed"])
        r.raise_for_status()
        feed = parse_feed(r.content)
    except (httpx.HTTPError, ET.ParseError, ValueError) as e:
        raise HTTPException(502, f"Feed konnte nicht geladen werden: {e}")
    _feed_cache[pid] = (time.time(), feed)
    with _lock:
        info = feed["info"]
        pod["title"] = pod.get("title") or info["title"]
        pod["author"] = pod.get("author") or info["author"]
        pod["artwork"] = pod.get("artwork") or info["artwork"]
        pod["episode_count"] = len(feed["episodes"])
        save_state()
    return feed


def register_podcast(feed_url: str, title: str = "", author: str = "", artwork: str = "") -> str:
    pid = short_id(feed_url)
    with _lock:
        pod = state["podcasts"].setdefault(pid, {"feed": feed_url, "followed": False})
        if title:
            pod["title"] = title
        if author:
            pod["author"] = author
        if artwork:
            pod["artwork"] = artwork
        if "dir" not in pod:
            pod["dir"] = f"{safe_name(title or pid, 80)} [{pid}]"
        pod["last_opened"] = time.time()
        save_state()
    return pid


# ---------------------------------------------------------------- jobs

class Job:
    def __init__(self, pid: str, episode: dict, subs: bool):
        self.id = uuid.uuid4().hex[:8]
        self.pid = pid
        self.episode = episode
        self.subs = subs
        self.state = "queued"
        self.progress = 0.0
        self.message = ""
        self.cancel = False
        self.proc: subprocess.Popen | None = None
        self.created = time.time()

    def public(self) -> dict:
        return {
            "id": self.id, "podcast_id": self.pid, "episode_id": self.episode["id"],
            "title": self.episode["title"],
            "podcast_title": state["podcasts"].get(self.pid, {}).get("title", ""),
            "subs": self.subs, "state": self.state, "progress": round(self.progress, 3),
            "message": self.message,
        }


jobs: list[Job] = []
job_queue: "queue.Queue[Job]" = queue.Queue()


class Cancelled(Exception):
    pass


def _base_name(ep: dict) -> str:
    date = time.strftime("%Y-%m-%d", time.localtime(ep["date"])) if ep.get("date") else "0000-00-00"
    return f"{date} {safe_name(ep['title'], 110)} [{ep['id']}]"


def _run(job: Job, cmd: list[str], on_line=None) -> None:
    env = {**os.environ, "PYTHONUNBUFFERED": "1"}
    job.proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, errors="replace", env=env)
    tail: list[str] = []
    assert job.proc.stdout
    for line in job.proc.stdout:
        tail = (tail + [line.rstrip()])[-15:]
        if on_line:
            on_line(line)
        if job.cancel:
            job.proc.terminate()
    code = job.proc.wait()
    job.proc = None
    if job.cancel:
        raise Cancelled()
    if code != 0:
        raise RuntimeError(f"{Path(cmd[0]).name} fehlgeschlagen:\n" + "\n".join(tail))


def _probe(path: Path, entries: str, stream: str | None = None) -> str:
    cmd = ["ffprobe", "-v", "error"]
    if stream:
        cmd += ["-select_streams", stream]
    cmd += ["-show_entries", entries, "-of", "default=nw=1:nk=1", str(path)]
    out = subprocess.run(cmd, capture_output=True, text=True).stdout.strip().splitlines()
    return out[0] if out else ""


def _download(job: Job, url: str, dest: Path) -> None:
    part = dest.with_name(dest.name + ".part")
    with _http.stream("GET", url, timeout=httpx.Timeout(30, read=120)) as r:
        r.raise_for_status()
        total = int(r.headers.get("content-length") or 0)
        done = 0
        with open(part, "wb") as fh:
            for chunk in r.iter_bytes(1 << 16):
                if job.cancel:
                    fh.close()
                    part.unlink(missing_ok=True)
                    raise Cancelled()
                fh.write(chunk)
                done += len(chunk)
                if total:
                    job.progress = done / total
                job.message = f"{done / 1e6:.1f} MB" + (f" / {total / 1e6:.1f} MB" if total else "")
    part.replace(dest)


def _make_mp4(job: Job, audio: Path, cover: Path | None, out: Path) -> None:
    """Wrap the audio into an MP4 with a still cover image (H.264 + MP3/AAC) for Migaku."""
    codec = _probe(audio, "stream=codec_name", "a:0")
    duration = float(_probe(audio, "format=duration") or 0)
    tmp = out.with_name(out.stem + ".tmp.mp4")
    if cover:
        video_in = ["-loop", "1", "-framerate", "1", "-i", str(cover)]
    else:
        video_in = ["-f", "lavfi", "-i", "color=c=0x202020:s=720x720:r=1"]
    audio_codec = ["-c:a", "copy"] if codec in ("mp3", "aac") else ["-c:a", "aac", "-b:a", "160k"]
    cmd = ["ffmpeg", "-hide_banner", "-nostdin", "-y", *video_in, "-i", str(audio),
           "-map", "0:v:0", "-map", "1:a:0",
           "-vf", "scale=720:-2,format=yuv420p", "-c:v", "libx264", "-tune", "stillimage",
           "-preset", "veryfast", "-r", "1", *audio_codec,
           "-shortest", "-movflags", "+faststart", "-progress", "pipe:1", str(tmp)]

    def on_line(line: str):
        if line.startswith("out_time_us=") and duration:
            try:
                job.progress = min(1.0, int(line.split("=")[1]) / 1e6 / duration)
            except ValueError:
                pass

    try:
        _run(job, cmd, on_line)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise
    tmp.replace(out)


TS_RE = re.compile(r"-->\s*(?:(\d+):)?(\d+):(\d+(?:\.\d+)?)\]")


def _transcribe(job: Job, media: Path, base: str) -> None:
    s = state["settings"]
    duration = float(_probe(media, "format=duration") or 0)
    folder = media.parent
    if s["engine"] == "openai":
        exe = shutil.which("whisper") or str(Path.home() / ".local/bin/whisper")
        cmd = [exe, str(media), "--model", s["model"], "--output_format", "srt",
               "--output_dir", str(folder), "--verbose", "True"]
    else:
        exe = shutil.which("mlx_whisper") or str(Path.home() / ".local/bin/mlx_whisper")
        cmd = [exe, str(media), "--model", s["model"], "--output-format", "srt",
               "--output-dir", str(folder), "--output-name", base, "--verbose", "True"]
    if s["language"]:
        cmd += ["--language", s["language"]]

    def on_line(line: str):
        m = TS_RE.search(line)
        if m and duration:
            h, mnt, sec = m.groups()
            t = int(h or 0) * 3600 + int(mnt) * 60 + float(sec)
            job.progress = min(1.0, t / duration)
            job.message = line.split("]", 1)[-1].strip()[:80]

    _run(job, cmd, on_line)


def _fetch_cover(url: str, dest: Path) -> Path | None:
    if not url:
        return None
    if dest.exists() and dest.stat().st_size > 0:
        return dest
    try:
        r = _http.get(url)
        r.raise_for_status()
        dest.write_bytes(r.content)
        return dest
    except httpx.HTTPError:
        return None


def process(job: Job) -> None:
    ep = job.episode
    folder = podcast_dir(job.pid)
    folder.mkdir(parents=True, exist_ok=True)
    base = _base_name(ep)
    existing = scan_files(job.pid).get(ep["id"], {})
    media = existing.get("media")

    if media is None:
        job.state, job.progress, job.message = "downloading", 0, ""
        ext = Path(ep["url"].split("?")[0]).suffix.lower() or ".mp3"
        raw = folder / f"{base}.orig{ext}"  # ".orig.ext" is not matched by FILE_RE
        _download(job, ep["url"], raw)
        job.state, job.progress, job.message = "converting", 0, ""
        pod = state["podcasts"][job.pid]
        cover = None
        if ep.get("image"):
            cover = _fetch_cover(ep["image"], folder / f".cover-{ep['id']}")
        cover = cover or _fetch_cover(pod.get("artwork", ""), folder / ".cover")
        media = folder / f"{base}.mp4"
        try:
            _make_mp4(job, raw, cover, media)
        finally:
            raw.unlink(missing_ok=True)
            if ep.get("image"):
                (folder / f".cover-{ep['id']}").unlink(missing_ok=True)

    if job.subs and not existing.get("subs"):
        job.state, job.progress, job.message = "transcribing", 0, "Modell wird geladen…"
        _transcribe(job, media, media.stem)


def worker() -> None:
    while True:
        job = job_queue.get()
        if job.cancel:
            continue
        try:
            process(job)
            job.state, job.progress, job.message = "done", 1.0, ""
        except Cancelled:
            job.state, job.message = "canceled", ""
        except Exception as e:  # noqa: BLE001 - shown in the UI
            job.state, job.message = "error", str(e)[-1500:]


threading.Thread(target=worker, daemon=True).start()


# ---------------------------------------------------------------- API

app = FastAPI()
_last_request = time.time()


@app.middleware("http")
async def touch(request, call_next):
    global _last_request
    _last_request = time.time()
    return await call_next(request)


@app.post("/api/quit")
def quit_server():
    for job in jobs:
        job.cancel = True
        if job.proc:
            job.proc.terminate()
    threading.Timer(0.5, lambda: os._exit(0)).start()
    return {"ok": True}


def idle_watchdog(limit: float) -> None:
    """Exit once no browser tab has polled for `limit` seconds and no job is running."""
    while True:
        time.sleep(15)
        busy = any(j.state in ("queued", "downloading", "converting", "transcribing") for j in jobs)
        if not busy and time.time() - _last_request > limit:
            os._exit(0)


@app.exception_handler(Exception)
async def unhandled(_request, exc: Exception):
    traceback.print_exception(exc)
    return JSONResponse({"detail": f"{type(exc).__name__}: {exc}"}, status_code=500)


@app.get("/")
def index():
    return FileResponse(ROOT / "static" / "index.html")


@app.get("/api/search")
def search(q: str, country: str = ""):
    q = q.strip()
    if not q:
        return []
    country = re.sub(r"[^A-Za-z]", "", country or state["settings"]["country"]).upper()[:2] or "DE"
    if country != state["settings"]["country"]:
        state["settings"]["country"] = country
        save_state()
    params = {"media": "podcast", "entity": "podcast", "term": q, "limit": 40, "country": country}
    for attempt in range(3):
        try:
            r = _http.get("https://itunes.apple.com/search", params=params)
            r.raise_for_status()
            data = r.json()
            break
        except (httpx.HTTPError, ValueError) as e:
            if attempt == 2:
                raise HTTPException(502, f"Apple-Podcast-Suche nicht erreichbar ({type(e).__name__}: {e})")
            time.sleep(1 + attempt)
    results = []
    for it in data.get("results", []):
        if not it.get("feedUrl"):
            continue
        results.append({
            "feed": it["feedUrl"],
            "title": it.get("collectionName", ""),
            "author": it.get("artistName", ""),
            "artwork": it.get("artworkUrl600") or it.get("artworkUrl100", ""),
            "genre": it.get("primaryGenreName", ""),
            "episode_count": it.get("trackCount"),
            "id": short_id(it["feedUrl"]),
        })
    return results


class PodcastIn(BaseModel):
    feed: str
    title: str = ""
    author: str = ""
    artwork: str = ""


@app.post("/api/podcasts")
def add_podcast(p: PodcastIn):
    pid = register_podcast(p.feed.strip(), p.title, p.author, p.artwork)
    if not p.title:  # RSS URL pasted directly: validate & fill metadata
        try:
            get_feed(pid, force=True)
        except HTTPException:
            with _lock:
                if not state["podcasts"][pid].get("followed"):
                    del state["podcasts"][pid]
                    save_state()
            raise
    return {"id": pid}


def _library_entry(pid: str, pod: dict) -> dict:
    st = status_map(pid)
    return {
        "id": pid, "title": pod.get("title", ""), "author": pod.get("author", ""),
        "artwork": pod.get("artwork", ""), "followed": pod.get("followed", False),
        "episode_count": pod.get("episode_count"),
        "downloaded": sum(1 for v in st.values() if v["media"]),
        "subtitled": sum(1 for v in st.values() if v["subs"] or v.get("done")),
        "last_opened": pod.get("last_opened", 0),
    }


@app.get("/api/library")
def library():
    items = [_library_entry(pid, pod) for pid, pod in state["podcasts"].items()]
    items.sort(key=lambda x: (not x["followed"], -x["last_opened"]))
    return items


@app.get("/api/podcasts/{pid}")
def podcast(pid: str, refresh: bool = False):
    feed = get_feed(pid, force=refresh)
    with _lock:
        state["podcasts"][pid]["last_opened"] = time.time()
        save_state()
    return {
        "podcast": {**_library_entry(pid, state["podcasts"][pid]),
                    "description": feed["info"]["description"],
                    "folder": str(podcast_dir(pid))},
        "episodes": feed["episodes"],
        "status": status_map(pid),
    }


@app.get("/api/podcasts/{pid}/status")
def podcast_status(pid: str):
    return status_map(pid)


class PodcastPatch(BaseModel):
    followed: bool | None = None


@app.patch("/api/podcasts/{pid}")
def patch_podcast(pid: str, p: PodcastPatch):
    with _lock:
        pod = state["podcasts"].get(pid)
        if not pod:
            raise HTTPException(404)
        if p.followed is not None:
            pod["followed"] = p.followed
        save_state()
    return {"ok": True}


@app.delete("/api/podcasts/{pid}")
def forget_podcast(pid: str):
    """Remove from the list (downloaded files stay on disk)."""
    with _lock:
        state["podcasts"].pop(pid, None)
        save_state()
    return {"ok": True}


def _find_episode(pid: str, eid: str) -> dict:
    for ep in get_feed(pid)["episodes"]:
        if ep["id"] == eid:
            return ep
    raise HTTPException(404, "Episode nicht im Feed gefunden")


class JobIn(BaseModel):
    podcast_id: str
    episode_id: str
    subs: bool = True


@app.post("/api/jobs")
def create_job(j: JobIn):
    for job in jobs:
        if (job.pid, job.episode["id"]) == (j.podcast_id, j.episode_id) and job.state in (
                "queued", "downloading", "converting", "transcribing"):
            job.subs = job.subs or j.subs
            return job.public()
    job = Job(j.podcast_id, _find_episode(j.podcast_id, j.episode_id), j.subs)
    jobs.append(job)
    job_queue.put(job)
    return job.public()


@app.get("/api/jobs")
def list_jobs():
    return [j.public() for j in jobs]


@app.post("/api/jobs/{jid}/cancel")
def cancel_job(jid: str):
    for job in jobs:
        if job.id == jid:
            job.cancel = True
            if job.state == "queued":
                job.state = "canceled"
            elif job.proc:
                job.proc.terminate()
    return {"ok": True}


@app.post("/api/jobs/clear")
def clear_jobs():
    jobs[:] = [j for j in jobs if j.state not in ("done", "error", "canceled")]
    return {"ok": True}


class EpisodeRef(BaseModel):
    podcast_id: str
    episode_id: str


@app.post("/api/episodes/done")
def toggle_done(e: EpisodeRef):
    key = f"{e.podcast_id}/{e.episode_id}"
    with _lock:
        if state["done"].pop(key, None) is None:
            state["done"][key] = True
        save_state()
    return {"done": key in state["done"]}


@app.post("/api/episodes/reveal")
def reveal(e: EpisodeRef):
    entry = scan_files(e.podcast_id).get(e.episode_id)
    if entry and entry["media"]:
        subprocess.run(["open", "-R", str(entry["media"])])
    else:
        folder = podcast_dir(e.podcast_id)
        folder.mkdir(parents=True, exist_ok=True)
        subprocess.run(["open", str(folder)])
    return {"ok": True}


@app.post("/api/episodes/delete")
def delete_files(e: EpisodeRef):
    entry = scan_files(e.podcast_id).get(e.episode_id)
    if entry:
        for f in [entry["media"], *entry["subs"]]:
            if f:
                f.unlink(missing_ok=True)
    return {"ok": True}


@app.get("/api/settings")
def get_settings():
    hub = Path.home() / ".cache/huggingface/hub"
    cached = []
    if hub.is_dir():
        for d in sorted(hub.glob("models--*")):
            name = d.name[len("models--"):].replace("--", "/")
            if "whisper" in name.lower():
                cached.append(name)
    return {**state["settings"], "cached_models": cached,
            "has_mlx": bool(shutil.which("mlx_whisper") or (Path.home() / ".local/bin/mlx_whisper").exists()),
            "has_openai": bool(shutil.which("whisper") or (Path.home() / ".local/bin/whisper").exists())}


class SettingsIn(BaseModel):
    engine: str
    model: str
    language: str = ""


@app.put("/api/settings")
def put_settings(s: SettingsIn):
    with _lock:
        state["settings"].update(engine=s.engine, model=s.model.strip(), language=s.language.strip())
        save_state()
    return state["settings"]


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("--no-browser", action="store_true", help="don't open the browser")
    parser.add_argument("--idle-exit", type=float, metavar="SECONDS",
                        help="quit after this long without an open tab and without running jobs")
    args = parser.parse_args()
    LIBRARY.mkdir(exist_ok=True)
    if not args.no_browser:
        threading.Timer(1.0, lambda: webbrowser.open(f"http://{HOST}:{PORT}")).start()
    if args.idle_exit:
        threading.Thread(target=idle_watchdog, args=(args.idle_exit,), daemon=True).start()
    uvicorn.run(app, host=HOST, port=PORT, log_level="warning")
