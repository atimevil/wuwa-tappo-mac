#!/usr/bin/env bash
# Build "Wuwa Tappo.app" for macOS from the official Windows installer.
#
#   tools/build.sh [--no-install]                  download the latest installer from Kuro and verify it
#   tools/build.sh /path/to/setup.exe [--no-install]  use an installer you already downloaded
#
# Extracts the frontend from the installer, applies the macOS patches, checks the host/frontend
# contract, builds the Tauri app, signs it, and (unless --no-install) installs it to /Applications.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXE=""
INSTALL=1
for arg in "$@"; do
  case "$arg" in
    --no-install) INSTALL=0 ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    *) EXE="$arg" ;;
  esac
done
if [ -n "$EXE" ]; then
  [ -f "$EXE" ] || { echo "no such file: $EXE" >&2; exit 1; }
  EXE="$(cd "$(dirname "$EXE")" && pwd)/$(basename "$EXE")"
fi

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing '$1': $2" >&2; exit 1; }; }
need python3 "install the Xcode Command Line Tools (xcode-select --install)"
need 7zz "brew install sevenzip"
need brotli "brew install brotli"
need cargo "brew install rustup && rustup default stable (then add rustup's bin to PATH)"
cargo tauri --version >/dev/null 2>&1 || { echo "missing tauri CLI: cargo install tauri-cli --version '^2' --locked" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$ROOT"

if [ -z "$EXE" ]; then
  # Kuro's own release manifest (what the official app's updater reads) and the public key of its
  # Tauri updater (minisign key 2DB584868D18BB49, embedded in the official app). The download comes
  # straight from Kuro and is rejected unless Kuro's signature verifies.
  need minisign "brew install minisign"
  HOST_JSON="https://wutheringwaves-wuwatappo.kurogames-global.com/app/json-host/latest-global_host.json"
  KURO_PUBKEY="RWRJuxiNhoS1LUN1e77aq5H5vNJqFBr18S8RvgGXw3ewFVhkmWSM7Tbd"
  echo "==> looking up the latest official installer"
  MANIFEST_URL="$(curl -fsSL "$HOST_JSON" | python3 -I -c 'import sys, json; print(json.load(sys.stdin)["url"])')"
  curl -fsSL "$MANIFEST_URL" -o "$WORK/latest.json"
  python3 -I - "$WORK" <<'PY'
import base64, json, sys, urllib.parse
work = sys.argv[1]
m = json.load(open(f'{work}/latest.json'))
p = m['platforms']['windows-x86_64']
url = urllib.parse.urlparse(p['url'])
if url.scheme != 'https' or not url.hostname.endswith('.kurogames-global.com'):
    sys.exit(f'refusing installer URL outside kurogames-global.com: {p["url"]}')
open(f'{work}/setup.url', 'w').write(p['url'])
open(f'{work}/setup.exe.sig', 'wb').write(base64.b64decode(p['signature']))
print(f'version {m["version"]} ({m.get("pub_date", "?")})')
PY
  echo "==> downloading from Kuro"
  curl -fL --progress-bar "$(cat "$WORK/setup.url")" -o "$WORK/setup.exe"
  minisign -V -q -P "$KURO_PUBKEY" -m "$WORK/setup.exe" -x "$WORK/setup.exe.sig" \
    || { echo "Kuro's signature did not verify; refusing to use this download" >&2; exit 1; }
  echo "    signature verified (Kuro updater key 2DB584868D18BB49)"
  EXE="$WORK/setup.exe"
fi

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

BUNDLE_ID=io.github.atimevil.wuwatappo-mac
IDENTITY="WuwaTappo Local Signing"
if security find-identity -p codesigning 2>/dev/null | grep -q "$IDENTITY"; then
  echo "==> signing with '$IDENTITY' (Input Monitoring stays granted across rebuilds)"
  codesign --force --deep --identifier "$BUNDLE_ID" -s "$IDENTITY" "$APP"
  ADHOC=0
else
  echo "==> ad-hoc signing (no certificate needed; Input Monitoring is asked again after each rebuild)"
  codesign --force --deep --identifier "$BUNDLE_ID" -s - "$APP"
  ADHOC=1
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
  if [ "$ADHOC" = 1 ]; then
    # An ad-hoc grant belongs to the previous build's hash; clear it so Settings shows no stale,
    # already-enabled entry that silently does nothing. The app then asks for the permission.
    tccutil reset ListenEvent "$BUNDLE_ID" >/dev/null 2>&1 || true
  fi
  open "/Applications/Wuwa Tappo.app"
  echo "done: /Applications/Wuwa Tappo.app"
else
  echo "done: $APP"
fi
