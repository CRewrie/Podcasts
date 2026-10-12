# podcasts-rs

Rust-Server des Podcast-Browsers (Standard). Port der Python-Version `../app.py`.

- **Gleiche API** wie `app.py`, daher läuft das Frontend `../static/index.html`
  unverändert (es wird beim Kompilieren ins Binary eingebettet).
- **Gleiche Daten:** standardmäßig `../state.json` und `../library/`. Python- und
  Rust-Version lassen sich abwechselnd benutzen (nicht gleichzeitig – gleicher Port).
- Ein einzelnes Binary, keine Python-Abhängigkeiten für den Server. Für Downloads
  und Untertitel werden weiterhin `ffmpeg` und `mlx_whisper` (bzw. `whisper`) gebraucht.

## Bauen & starten

```sh
brew install rust          # einmalig
cargo run --release        # öffnet http://127.0.0.1:8765
```

Optionen (`cargo run --release -- --help`):

| Option | Bedeutung |
| --- | --- |
| `--data-dir DIR` | Ordner mit `state.json` und `library/` (Standard: Repo-Hauptordner) |
| `--port N` | Port (Standard 8765) |
| `--no-browser` | Browser nicht öffnen |
| `--idle-exit SEK` | Beenden, wenn so lange kein Tab offen ist und kein Auftrag läuft |

Nach Änderungen an `static/index.html` neu kompilieren.

## Aufbau

| Datei | Inhalt |
| --- | --- |
| `src/main.rs` | CLI, Server-Start |
| `src/api.rs` | HTTP-Routen (axum), gleiche JSON-Formate wie `app.py` |
| `src/app.rs` | `state.json`, Scan des `library/`-Ordners, Feed-Cache |
| `src/feed.rs` | RSS-Parser (roxmltree) |
| `src/jobs.rs` | Warteschlange: Download → MP4 (ffmpeg) → Untertitel (Whisper), abbrechbar per `CancellationToken` |

`cargo test` führt die Unit-Tests aus.
