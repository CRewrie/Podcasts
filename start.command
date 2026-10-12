#!/bin/zsh
# Starts the podcast browser: the Rust version by default (compiled on first start),
# the Python version (app.py) only if Rust is not installed.
cd "$(dirname "$0")"
export PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"
if command -v cargo >/dev/null; then
  exec cargo run --release --quiet --manifest-path podcasts-rs/Cargo.toml -- --data-dir "$PWD"
elif [[ -x podcasts-rs/target/release/podcasts ]]; then
  exec podcasts-rs/target/release/podcasts --data-dir "$PWD"
elif [[ -x .venv/bin/python ]]; then
  exec .venv/bin/python app.py
else
  exec python3 app.py
fi
