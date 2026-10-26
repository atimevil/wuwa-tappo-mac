#!/usr/bin/env bash
# Build "Wuwa Tappo.app" for macOS from the official Windows installer.
#
#   tools/build.sh /path/to/WuwaTappo-global_<version>_x64-setup.exe [--no-install]
#
# Extracts the frontend from the installer, applies the macOS patches, checks the host/frontend
# contract, builds the Tauri app, signs it, and (unless --no-install) installs it to /Applications.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXE="${1:-}"
INSTALL=1
[ "${2:-}" = "--no-install" ] && INSTALL=0
if [ -z "$EXE" ] || [ ! -f "$EXE" ]; then
  echo "usage: tools/build.sh /path/to/WuwaTappo-global_<version>_x64-setup.exe [--no-install]" >&2
  exit 1
fi
EXE="$(cd "$(dirname "$EXE")" && pwd)/$(basename "$EXE")"

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing '$1': $2" >&2; exit 1; }; }
need python3 "install the Xcode Command Line Tools (xcode-select --install)"
need 7zz "brew install sevenzip"
need brotli "brew install brotli"
need cargo "brew install rustup && rustup default stable (then add rustup's bin to PATH)"
cargo tauri --version >/dev/null 2>&1 || { echo "missing tauri CLI: cargo install tauri-cli --version '^2' --locked" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$ROOT"

echo "==> extracting frontend from $(basename "$EXE")"
python3 -I tools/extract_payload.py "$EXE" "$WORK/inner.exe"
7zz x -y -o"$WORK/nsis" "$WORK/inner.exe" WuwaTappo.exe >/dev/null
rm -rf web && mkdir web
python3 -I tools/extract_assets.py "$WORK/nsis/WuwaTappo.exe" web

echo "==> patching and checking"
python3 -I tools/patch_frontend.py web
python3 -I tools/check_contract.py

echo "==> icons"
ICONS=src-tauri/icons SRC=web/app-icon.png
mkdir -p "$ICONS" "$WORK/icon.iconset"
sips -z 32 32 "$SRC" --out "$ICONS/32x32.png" >/dev/null
sips -z 128 128 "$SRC" --out "$ICONS/128x128.png" >/dev/null
sips -z 256 256 "$SRC" --out "$ICONS/128x128@2x.png" >/dev/null
sips -z 64 64 "$SRC" --out "$ICONS/tray.png" >/dev/null
for s in 16 32 128 256 512; do
  sips -z $s $s "$SRC" --out "$WORK/icon.iconset/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) "$SRC" --out "$WORK/icon.iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$WORK/icon.iconset" -o "$ICONS/icon.icns"

echo "==> building"
(cd src-tauri && cargo tauri build --bundles app)
APP="src-tauri/target/release/bundle/macos/Wuwa Tappo.app"

IDENTITY="WuwaTappo Local Signing"
if security find-identity -p codesigning 2>/dev/null | grep -q "$IDENTITY"; then
  echo "==> signing with '$IDENTITY'"
  codesign --force --deep --identifier io.github.atimevil.wuwatappo-mac -s "$IDENTITY" "$APP"
else
  echo "==> no '$IDENTITY' identity: keeping the ad-hoc signature."
  echo "    Input Monitoring must be granted again after every rebuild; run tools/make_signing_identity.sh once to avoid that."
fi

if [ "$INSTALL" = 1 ]; then
  echo "==> installing to /Applications"
  pkill -f "Wuwa Tappo.app/Contents/MacOS" 2>/dev/null || true
  sleep 1
  rm -rf "/Applications/Wuwa Tappo.app"
  ditto "$APP" "/Applications/Wuwa Tappo.app"
  LSR=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
  "$LSR" -f "/Applications/Wuwa Tappo.app"
  "$LSR" -u "$ROOT/$APP" 2>/dev/null || true  # so wuwa-buddy:// login callbacks open the installed copy
  open "/Applications/Wuwa Tappo.app"
  echo "done: /Applications/Wuwa Tappo.app"
else
  echo "done: $APP"
fi
