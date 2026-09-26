#!/bin/zsh
# Builds ~/Applications/Podcasts.app: runs the server (if not already running) and
# opens the browser. It quits by itself 10 minutes after the last browser tab is
# closed, unless a job is still running – or via "Server beenden" in the settings.
set -e
REPO="$(cd "$(dirname "$0")" && pwd)"
APP="$HOME/Applications/Podcasts.app"

# Python that has the dependencies: the project's .venv, else the python3 of this shell.
# /usr/bin/python3 is only a stub without the Xcode command line tools and would
# block on an install dialog, so it is never used in that case.
PY="$REPO/.venv/bin/python"
[[ -x "$PY" ]] || PY="$(command -v python3 || true)"
if [[ -z "$PY" || ( "$PY" == /usr/bin/python3 && ! -d "$(xcode-select -p 2>/dev/null)" ) ]]; then
  echo "Fehler: kein nutzbares Python gefunden – erst die Einrichtung aus der README ausführen (brew install python, .venv)." >&2
  exit 1
fi
echo "Python: $PY"
"$PY" -c "import fastapi, uvicorn, httpx" 2>/dev/null || {
  echo "Fehler: $PY findet fastapi/uvicorn/httpx nicht – erst die Einrichtung aus der README ausführen." >&2
  exit 1
}
echo "Baue $APP …"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cat > "$APP/Contents/MacOS/Podcasts" <<EOF
#!/bin/zsh
export PATH="/opt/homebrew/bin:/usr/local/bin:\$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
URL="http://127.0.0.1:8765"
# already running: just open another tab
if curl -sf -o /dev/null "\$URL/api/settings"; then
  exec open "\$URL"
fi
mkdir -p "\$HOME/Library/Logs"
cd "$REPO"
# the app process itself becomes the server: macOS kills leftover children of a finished app
exec "$PY" app.py --idle-exit 600 < /dev/null >> "\$HOME/Library/Logs/Podcasts.log" 2>&1
EOF
chmod +x "$APP/Contents/MacOS/Podcasts"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Podcasts</string>
  <key>CFBundleDisplayName</key><string>Podcasts</string>
  <key>CFBundleIdentifier</key><string>local.podcast-browser</string>
  <key>CFBundleExecutable</key><string>Podcasts</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
</dict></plist>
EOF

cp "$REPO/assets/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"

touch "$APP"
echo "Fertig: $APP"
echo "Zum Anheften: App im Finder öffnen und ins Dock ziehen (oder: open ~/Applications)."
