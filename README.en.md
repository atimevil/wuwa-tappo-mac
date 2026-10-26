# Wuwa Tappo for macOS (unofficial)

[한국어](README.md) | **English**

An unofficial host that runs **Wuwa Tappo**, the Windows-only desktop pet for Wuthering Waves, on macOS.

This repository contains **no** Kuro Games frontend code or art. `tools/build.sh` downloads the official Windows installer from Kuro's server and verifies Kuro's signature. It then extracts the frontend from the installer and builds it into a macOS app (`Wuwa Tappo.app`). The repository itself contains a Tauri host that re-implements, for macOS, everything the frontend calls (windows, global input, tray, storage, and so on), plus the extraction, patching, and checking tools.

## Requirements

- An Apple Silicon or Intel Mac running macOS 11 or later
- Xcode Command Line Tools: `xcode-select --install`
- Homebrew packages: `brew install sevenzip brotli minisign rustup`
- Rust and the Tauri CLI:
  ```sh
  rustup default stable
  export PATH="$(brew --prefix rustup)/bin:$HOME/.cargo/bin:$PATH"
  cargo install tauri-cli --version '^2' --locked
  ```

## Build and install

```sh
git clone https://github.com/atimevil/wuwa-tappo-mac.git
cd wuwa-tappo-mac
tools/build.sh
```

`build.sh` runs these steps in order:

1. Downloads the latest installer listed in Kuro's release manifest (the file the official app's updater reads). It verifies the download with Kuro's updater public key from the official app (minisign `2DB584868D18BB49`) and stops if verification fails.
2. Extracts the frontend from the installer.
3. Applies the macOS patches.
4. Checks that the host and the frontend agree on every command they exchange.
5. Generates the icons, builds and signs the app, and installs it to `/Applications`.

To use an installer you already downloaded, pass its path, for example `tools/build.sh ~/Downloads/WuwaTappo-global_1.0.0_x64-setup.exe`. In that case the signature is not verified. To build without installing, add `--no-install`. The build output goes to `src-tauri/target/release/bundle/macos/`.

## Permissions

To count key presses and clicks, Wuwa Tappo needs **System Settings → Privacy & Security → Input Monitoring**. When you turn it on, macOS quits the app once; open it again.

macOS shows its permission prompt only once per app, and if you miss it the app may not appear in the Settings list at all. So whenever the permission is missing, the app shows its own dialog at startup. Click "Open Settings" in that dialog and turn on Wuwa Tappo in the list. If it is not listed, click "+" below the list and add Wuwa Tappo from the Applications folder.

### Why the permission is requested again after a rebuild (the certificate is optional)

macOS ties this permission to the app's code signature. By default the build is signed ad hoc, without a certificate, so after a rebuild macOS sees a different app and asks for the permission again. You only rebuild when a new version comes out, so allowing it once each time is usually fine. `build.sh` clears the previous build's permission record before installing, so no stale entry stays in the list looking enabled while doing nothing.

If you rebuild often (while developing or modifying the app), run `tools/make_signing_identity.sh` once. It creates a self-signed certificate (`WuwaTappo Local Signing`) in your login keychain. `build.sh` then signs with it, and the permission survives rebuilds. The first signing asks for your keychain password; choose "Always Allow". You can delete the certificate in Keychain Access at any time.

## Login

- **Kuro account:** the official login page opens in Safari, and after you sign in it returns to the app through a `wuwa-buddy://` link. The page's login SDK fails to initialize in Firefox (error 20003), so it always opens in Safari regardless of your default browser.
- **Game UID:** you sign in inside the app with a verification code.

## How it works

The Windows app is a Tauri 2 app. Its frontend (Vue) is compressed inside the executable, and its Rust side handles windows, global input, the tray, and storage. This repository re-implements that Rust side for macOS.

| Part | Files |
|---|---|
| The 51 commands the frontend calls, windows, menus, storage | `src-tauri/src/lib.rs` |
| Global keyboard and mouse (listen-only CGEventTap) | `src-tauri/src/input.rs` |
| WebSocket relay for the realtime server | `src-tauri/src/ws_proxy.rs` |
| Installer unpacking and asset extraction | `tools/extract_payload.py`, `tools/extract_assets.py` |
| Frontend patches (Windows/macOS differences) | `tools/patch_frontend.py` |

### Differences from Windows

- **Cookies:** the frontend is served from `tauri://localhost`, where WebKit keeps no cookies. The login token lives in a cookie, so every window replaces `document.cookie` with a localStorage-backed store.
- **Realtime server (socket.io):** the server accepts WebSocket connections only from the `http(s)://tauri.localhost` origin or with no Origin header. WebSockets to that server therefore go through a loopback relay (127.0.0.1) inside the app. The relay changes only the Host and Origin of the upgrade request and passes the TLS connection through unchanged.
- **Window-open events:** some windows, such as the login window, render only after they receive an event. For those windows the host sends the event after the page has registered its listener (patch 1).
- **Click-through:** a transparent window covers the whole screen and accepts clicks only over the pet regions that the frontend reports.
- **Other:**
  - Auto-update is turned off, because the update files are Windows installers.
  - The collapsed panel width (112px) is an estimate taken from the CSS.
  - With many menu bar icons, the tray icon can be hidden behind the MacBook notch. The same menu also opens when you right-click the pet.

## When a new version comes out

Run `tools/build.sh` again to rebuild with the latest installer. If a patch or the contract check fails, the frontend code has changed. Update the anchor strings in `tools/patch_frontend.py` and the command list in `src-tauri/src/lib.rs` to match the new version.

## Checks

- `python3 tools/check_contract.py`: compares every command and argument name the frontend sends with the Rust implementation, and checks that each plugin command is covered by a permission.
- `python3 tools/check_cookie_shim.py`: tests the cookie replacement against the frontend's own cookie functions. Requires Node.
- Runtime command check (debug builds only): after `tools/build.sh` has created `web/` once, run:
  ```sh
  (cd src-tauri && cargo build)
  WUWA_QA_SCRIPT=tools/qa_invoke.js src-tauri/target/debug/wuwatappo-mac
  ```
  Look for `[QA]` lines in the app log (`~/Library/Logs/io.github.atimevil.wuwatappo-mac/`). The run should end with `done pass=… fail=0`.
- `cd src-tauri && cargo test`

## License and notice

The code in this repository is MIT-licensed (`LICENSE`). Wuwa Tappo, Wuthering Waves, and the related names, art, and frontend code belong to Kuro Games. This project is not affiliated with or endorsed by Kuro Games. The app you build and the assets it extracts contain Kuro's copyrighted material, so do not distribute them. Anyone else can clone this repository and build their own copy.
