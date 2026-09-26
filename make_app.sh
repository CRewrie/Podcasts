#!/bin/zsh
# Builds ~/Applications/Podcasts.app: runs the server (if not already running) and
# opens the browser. It quits by itself 10 minutes after the last browser tab is
# closed, unless a job is still running – or via "Server beenden" in the settings.
set -e
REPO="$(cd "$(dirname "$0")" && pwd)"
APP="$HOME/Applications/Podcasts.app"
BUILD="$(mktemp -d)"
# Python that has the dependencies: the project's .venv, else the python3 of this shell
PY="$REPO/.venv/bin/python"
[[ -x "$PY" ]] || PY="$(command -v python3)"
"$PY" -c "import fastapi, uvicorn, httpx" 2>/dev/null || {
  echo "Fehler: $PY findet fastapi/uvicorn/httpx nicht – erst die Einrichtung aus der README ausführen." >&2
  exit 1
}
trap 'rm -rf "$BUILD"' EXIT

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

# Icon: headphones emoji on a rounded gradient square
cat > "$BUILD/icon.swift" <<'EOF'
import AppKit
let size: CGFloat = 1024
let img = NSImage(size: NSSize(width: size, height: size))
img.lockFocus()
let rect = NSRect(x: 100, y: 100, width: size - 200, height: size - 200)
let path = NSBezierPath(roundedRect: rect, xRadius: 185, yRadius: 185)
NSGradient(starting: NSColor(red: 0.45, green: 0.35, blue: 0.95, alpha: 1),
           ending: NSColor(red: 0.18, green: 0.44, blue: 0.86, alpha: 1))!.draw(in: path, angle: -90)
let emoji = NSAttributedString(string: "🎧", attributes: [.font: NSFont.systemFont(ofSize: 520)])
let s = emoji.size()
emoji.draw(at: NSPoint(x: (size - s.width) / 2, y: (size - s.height) / 2))
img.unlockFocus()
let rep = NSBitmapImageRep(data: img.tiffRepresentation!)!
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
EOF
if swift "$BUILD/icon.swift" "$BUILD/icon.png" 2>/dev/null; then
  mkdir "$BUILD/AppIcon.iconset"
  for s in 16 32 128 256 512; do
    sips -z $s $s "$BUILD/icon.png" --out "$BUILD/AppIcon.iconset/icon_${s}x${s}.png" >/dev/null
    sips -z $((s*2)) $((s*2)) "$BUILD/icon.png" --out "$BUILD/AppIcon.iconset/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$BUILD/AppIcon.iconset" -o "$APP/Contents/Resources/AppIcon.icns"
fi

touch "$APP"
echo "Fertig: $APP"
echo "Zum Anheften: App im Finder öffnen und ins Dock ziehen (oder: open ~/Applications)."
