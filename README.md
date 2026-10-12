# Podcasts

Lokales Web-Frontend zum Durchstöbern von Podcasts: Podcast suchen, Episoden
durchsuchen/sortieren, Episoden als Migaku-kompatible MP4 herunterladen und
optional mit Whisper Untertitel (`.srt`) erzeugen.

Der Server ist in Rust geschrieben ([podcasts-rs/](podcasts-rs/), Standard).
Die ursprüngliche Python-Version (`app.py`) mit identischer API, gleichem Frontend
und gleichen Daten gibt es weiterhin, siehe unten.

## Einrichtung (macOS, Apple Silicon)

Einmalig [Homebrew](https://brew.sh) installieren, dann:

```sh
brew install rust ffmpeg pipx
pipx ensurepath                 # danach Terminal neu öffnen
pipx install mlx-whisper        # Whisper für Apple Silicon

git clone https://github.com/CRewrie/Podcasts.git
cd Podcasts
```

Auf Intel-Macs statt `mlx-whisper` `pipx install openai-whisper` nehmen und in
den Einstellungen die Engine umstellen (deutlich langsamer).

## Starten

```sh
./start.command     # oder Doppelklick im Finder
```

Der erste Start kompiliert den Server (ca. 1 Minute), danach startet er sofort.
Öffnet http://127.0.0.1:8765. Downloads landen in `library/<Podcast>/`,
Einstellungen (Whisper-Modell, Sprache) über das ⚙︎-Symbol. Beim ersten
Transkribieren wird das Whisper-Modell (~1,6 GB) automatisch geladen.

## Als App im Dock

```sh
./make_app.sh
```

Legt `~/Applications/Podcasts.app` an. Im Finder öffnen (`open ~/Applications`)
und ins Dock ziehen. Ein Klick startet den Server und öffnet den Browser; läuft er
schon, öffnet sich nur ein neuer Tab. Die App beendet sich 10 Minuten nach dem
Schließen des letzten Tabs von selbst (nicht während eines laufenden Auftrags),
oder sofort über ⚙︎ → „Quit server“. Log: `~/Library/Logs/Podcasts.log`.

Das kompilierte Programm wird in die App kopiert – zum Ausführen ist Rust nicht
nötig, nur zum Bauen. Nach Änderungen am Code oder an `static/index.html` und nach
dem Verschieben des Projektordners `./make_app.sh` erneut ausführen.

## Python-Version (Alternative)

```sh
brew install python
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python app.py          # oder als Dock-App: ./make_app.sh --python
```

`start.command` nimmt die Python-Version automatisch, wenn Rust nicht installiert ist.
