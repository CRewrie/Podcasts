//! Local podcast browser – Rust port of app.py.
//!
//! Same HTTP API, same frontend (static/index.html, embedded at compile time) and by
//! default the same data (../state.json, ../library) as the Python version.
//!
//! Start:  cargo run --release   ->  http://127.0.0.1:8765

mod api;
mod app;
mod feed;
mod jobs;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

#[derive(Parser)]
#[command(version, about = "Lokaler Podcast-Browser mit Whisper-Untertiteln für Migaku")]
struct Args {
    /// Folder with state.json and library/ (default: the repository root, shared with app.py)
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/.."))]
    data_dir: PathBuf,
    #[arg(long, default_value_t = 8765)]
    port: u16,
    /// Don't open the browser
    #[arg(long)]
    no_browser: bool,
    /// Quit after this many seconds without an open tab and without running jobs
    #[arg(long, value_name = "SECONDS")]
    idle_exit: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let root = args.data_dir.canonicalize().with_context(|| format!("{} nicht gefunden", args.data_dir.display()))?;
    let (app, rx) = app::App::new(root.clone())?;
    tokio::spawn(jobs::worker(app.clone(), rx));
    if let Some(secs) = args.idle_exit {
        tokio::spawn(api::idle_watchdog(app.clone(), secs));
    }

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("Port {} ist belegt – läuft schon ein Server?", args.port))?;
    let url = format!("http://{addr}");
    println!("Podcasts (Rust) läuft auf {url}  ·  Daten: {}", root.display());
    if !args.no_browser {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    axum::serve(listener, api::router(app)).await?;
    Ok(())
}
