//! macOS host for the WuwaTappo frontend extracted from the Windows 1.0.0 build.
//! The frontend (web/) is unchanged; this crate re-implements the Rust commands and
//! events it calls, matching the contract read from the bundled JS.

mod input;
mod ws_proxy;

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, PhysicalPosition, PhysicalSize,
    WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

const MAIN: &str = "main";
const PET_SCALES: [u32; 6] = [50, 75, 100, 125, 150, 200];
const OPACITIES: [u32; 4] = [25, 50, 75, 100];

/// The main window covers the whole monitor and never sees real clicks: the frontend
/// synthesizes pointer events from our native `mouse-*` events. Trusted DOM mouse events
/// would double-handle clicks whenever we make the window clickable over a pet.
const MAIN_INIT_SCRIPT: &str = r#"
(() => {
  const types = ['pointerdown','pointerup','pointermove','pointerover','pointerout','pointerenter',
    'pointerleave','mousedown','mouseup','mousemove','mouseover','mouseout','mouseenter','mouseleave',
    'click','dblclick','auxclick','contextmenu'];
  for (const t of types) {
    window.addEventListener(t, (e) => {
      if (!e.isTrusted) return;
      e.stopImmediatePropagation();
      if (e.cancelable) e.preventDefault();
    }, { capture: true });
  }
})();
"#;

/// WebView console warnings/errors and uncaught errors go to the app log (they are otherwise invisible
/// in a release build). The Windows build logs through `write_app_log` the same way.
const CONSOLE_BRIDGE_SCRIPT: &str = r#"
(() => {
  const fmt = (a) => {
    if (typeof a === 'string') return a;
    if (a instanceof Error) return a.stack || a.message;
    try { return JSON.stringify(a); } catch { return String(a); }
  };
  // Kuro's analytics names a property '#device_id' and its own SDK warns about the '#' on every event
  // (the event is still sent). Same on Windows; keep the pair out of the app log, which it floods.
  let skipFormatWarning = false;
  const isNoise = (raw) => {
    const msg = raw.replace(/^"|"$/g, ''); // the SDK logs String objects, which fmt JSON-quotes
    if (msg === 'invalid key: #device_id') return (skipFormatWarning = true);
    const follow = skipFormatWarning && msg.startsWith('Please check the parameter format, the key of properties');
    skipFormatWarning = false;
    return follow;
  };
  const send = (level, args) => {
    try {
      const msg = args.map(fmt).join(' ');
      if (isNoise(msg)) return;
      window.__TAURI_INTERNALS__.invoke('write_app_log', {
        level, message: '[console:' + location.hash + '] ' + msg.slice(0, 2000),
      });
    } catch {}
  };
  for (const level of ['warn', 'error']) {
    const orig = console[level].bind(console);
    console[level] = (...args) => { orig(...args); send(level, args); };
  }
  window.addEventListener('error', (e) => send('error', [e.message, e.filename + ':' + e.lineno]));
  window.addEventListener('unhandledrejection', (e) => send('error', ['unhandledrejection', e.reason]));
})();
"#;

/// WebKit keeps no cookies for the custom `tauri://` scheme, but the frontend stores its login token
/// with `document.cookie` (Windows serves the page from http://tauri.localhost, where that works).
/// Back `document.cookie` with localStorage, which every window of this origin shares.
const COOKIE_SHIM_SCRIPT: &str = r#"
(() => {
  const KEY = '__wuwa_cookie_jar__';
  const load = () => { try { return JSON.parse(localStorage.getItem(KEY) || '{}'); } catch { return {}; } };
  const save = (jar) => { try { localStorage.setItem(KEY, JSON.stringify(jar)); } catch {} };
  Object.defineProperty(document, 'cookie', {
    configurable: true,
    get() {
      const jar = load(), now = Date.now(), out = [];
      let expired = false;
      for (const [name, c] of Object.entries(jar)) {
        if (c.e !== null && c.e <= now) { delete jar[name]; expired = true; continue; }
        out.push(name + '=' + c.v);
      }
      if (expired) save(jar);
      return out.join('; ');
    },
    set(raw) {
      const [pair, ...attrs] = String(raw).split(';');
      const eq = pair.indexOf('=');
      if (eq < 0) return;
      const name = pair.slice(0, eq).trim(), value = pair.slice(eq + 1).trim();
      let expires = null;
      for (const attr of attrs) {
        const [k, ...rest] = attr.split('=');
        const key = k.trim().toLowerCase(), val = rest.join('=').trim();
        if (key === 'max-age' && val !== '' && !isNaN(Number(val))) expires = Date.now() + Number(val) * 1000;
        else if (key === 'expires' && expires === null && !isNaN(Date.parse(val))) expires = Date.parse(val);
      }
      const jar = load();
      if (expires !== null && expires <= Date.now()) delete jar[name];
      else jar[name] = { v: value, e: expires };
      save(jar);
    },
  });
})();
"#;

#[derive(Deserialize, Clone, Debug)]
struct Rect {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    #[serde(rename = "memberId")]
    member_id: Option<Value>,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.left && x < self.left + self.width && y >= self.top && y < self.top + self.height
    }
}

#[derive(Default)]
struct Hit {
    regions: Vec<Rect>,
    input_regions: Vec<Rect>,
    window_passthrough: bool,
    swallow: bool,
    /// Main window inner origin in points and its scale factor.
    origin: (f64, f64),
    scale: f64,
    ignoring: Option<bool>,
}

#[derive(Default)]
struct Shared {
    hit: Mutex<Hit>,
    menu: Mutex<MenuState>,
    maintenance: Mutex<Option<Value>>,
    store: Mutex<()>,
    aux_events: Mutex<AuxEvents>,
}

/// Open events for aux windows are only delivered once the page has registered its listener;
/// emitting right after creating a window loses the event and leaves e.g. login invisible.
#[derive(Default)]
struct AuxEvents {
    ready: std::collections::HashSet<(String, String)>,
    pending: HashMap<String, (String, Value)>,
}

#[derive(Default, Clone)]
struct MenuState {
    labels: HashMap<String, String>,
    pet_dock_visible: bool,
    pet_scale: u32,
    window_opacity: u32,
    window_passthrough: bool,
    game_mode: bool,
}

impl MenuState {
    fn label(&self, key: &str, fallback: &str) -> String {
        self.labels.get(key).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| fallback.into())
    }
}

/// Called from the input thread for every pointer event: converts to the frontend's physical
/// coordinates, toggles click-through, and finds the pet member under the cursor.
/// Returns physical coordinates, the pet member under the cursor, and whether the main window
/// is taking this click (cursor over a pet region).
pub(crate) fn on_pointer(app: &AppHandle, px: f64, py: f64) -> (f64, f64, Option<Value>, bool) {
    let st = app.state::<Shared>();
    let mut hit = st.hit.lock().unwrap();
    let scale = if hit.scale > 0.0 { hit.scale } else { 1.0 };
    let (lx, ly) = (px - hit.origin.0, py - hit.origin.1);
    let input_hit = hit.input_regions.iter().find(|r| r.contains(lx, ly));
    let member = input_hit.and_then(|r| r.member_id.clone());
    let inside = input_hit.is_some() || hit.regions.iter().any(|r| r.contains(lx, ly));
    let capture = hit.swallow || (inside && !hit.window_passthrough);
    let changed = hit.ignoring != Some(!capture);
    hit.ignoring = Some(!capture);
    // Release the lock before touching the window: window calls hop to the main thread, where
    // commands like set_main_hit_region may be waiting for this same lock.
    drop(hit);
    if changed {
        if let Some(w) = app.get_webview_window(MAIN) {
            let _ = w.set_ignore_cursor_events(!capture);
        }
    }
    (px * scale, py * scale, member, capture)
}

fn refresh_geometry(app: &AppHandle) {
    let Some(w) = app.get_webview_window(MAIN) else { return };
    let (Ok(scale), Ok(pos)) = (w.scale_factor(), w.inner_position()) else { return };
    let st = app.state::<Shared>();
    let mut hit = st.hit.lock().unwrap();
    hit.scale = scale;
    hit.origin = (pos.x as f64 / scale, pos.y as f64 / scale);
}

fn reapply_ignore(app: &AppHandle) {
    let st = app.state::<Shared>();
    st.hit.lock().unwrap().ignoring = None;
}

fn main_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window(MAIN).ok_or_else(|| "main window missing".into())
}

fn main_monitor(app: &AppHandle) -> Result<tauri::Monitor, String> {
    let w = main_window(app)?;
    w.current_monitor()
        .ok()
        .flatten()
        .or_else(|| w.primary_monitor().ok().flatten())
        .ok_or_else(|| "no monitor".into())
}

fn cover_monitor(w: &WebviewWindow, m: &tauri::Monitor) -> tauri::Result<()> {
    w.set_position(PhysicalPosition::new(m.position().x, m.position().y))?;
    w.set_size(PhysicalSize::new(m.size().width, m.size().height))
}

// ---------- windows ----------

struct Aux {
    label: &'static str,
    route: String,
    size: (f64, f64),
}

/// Called by the patched frontend `listen` (tools/patch_frontend.py) after each listener registers.
#[tauri::command]
fn aux_listener_ready(app: AppHandle, window: tauri::Window, event: String) {
    let label = window.label().to_string();
    let st = app.state::<Shared>();
    let mut aux = st.aux_events.lock().unwrap();
    aux.ready.insert((label.clone(), event.clone()));
    if aux.pending.get(&label).is_some_and(|(ev, _)| *ev == event) {
        let (ev, payload) = aux.pending.remove(&label).unwrap();
        log::info!("[aux] delivering {ev} to {label} after its listener registered");
        let _ = app.emit_to(label.as_str(), ev.as_str(), payload);
    }
}

/// Show an aux window (creating it on first use) and send it its open event.
fn open_aux(app: &AppHandle, aux: Aux, event: Option<(&str, Value)>) -> Result<WebviewWindow, String> {
    let w = match app.get_webview_window(aux.label) {
        Some(w) => w,
        None => {
            let w = WebviewWindowBuilder::new(app, aux.label, WebviewUrl::App(aux.route.into()))
                .title("Wuwa Tappo")
                .initialization_script(CONSOLE_BRIDGE_SCRIPT)
                .initialization_script(COOKIE_SHIM_SCRIPT)
                .inner_size(aux.size.0, aux.size.1)
                .decorations(false)
                .accept_first_mouse(true)
                .transparent(true)
                .shadow(false)
                .resizable(false)
                .always_on_top(true)
                .visible(false)
                .center()
                .build()
                .map_err(|e| e.to_string())?;
            let (handle, label) = (app.clone(), aux.label.to_string());
            w.on_window_event(move |e| {
                if matches!(e, WindowEvent::Destroyed) {
                    let st = handle.state::<Shared>();
                    st.aux_events.lock().unwrap().ready.retain(|(l, _)| *l != label);
                }
            });
            w
        }
    };
    log::info!("[window] show {}", aux.label);
    w.show().map_err(|e| e.to_string())?;
    AUX_SHOWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _ = w.unminimize();
    let _ = w.set_focus();
    if let Some((name, payload)) = event {
        let st = app.state::<Shared>();
        let mut ev = st.aux_events.lock().unwrap();
        if ev.ready.contains(&(aux.label.to_string(), name.to_string())) {
            let _ = app.emit_to(aux.label, name, payload);
        } else {
            ev.pending.insert(aux.label.to_string(), (name.to_string(), payload));
        }
    }
    Ok(w)
}

fn hide_window(app: &AppHandle, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.hide();
    }
}

#[tauri::command]
fn open_about_window(app: AppHandle) -> Result<(), String> {
    let aux = Aux { label: "about", route: "index.html#/about".into(), size: (880.0, 720.0) };
    open_aux(&app, aux, Some(("about-open", Value::Null))).map(|_| ())
}

#[tauri::command]
fn close_about_window(app: AppHandle) {
    hide_window(&app, "about");
}

#[tauri::command]
fn open_oss_notice_window(app: AppHandle) -> Result<(), String> {
    let aux = Aux { label: "oss-notice", route: "index.html#/oss-notice".into(), size: (880.0, 720.0) };
    open_aux(&app, aux, Some(("oss-notice-open", Value::Null))).map(|_| ())
}

#[tauri::command]
fn close_oss_notice_window(app: AppHandle) {
    hide_window(&app, "oss-notice");
}

#[tauri::command]
fn open_login_window(app: AppHandle, intent: Option<String>, compact: Option<bool>) -> Result<(), String> {
    let compact = compact.unwrap_or(false);
    let size = if compact { (498.0, 396.0) } else { (900.0, 760.0) };
    let aux = Aux { label: "login", route: "index.html#/login".into(), size };
    let payload = json!({ "intent": intent.unwrap_or_else(|| "login".into()), "compact": compact });
    open_aux(&app, aux, Some(("login-modal-open", payload))).map(|_| ())
}

#[tauri::command]
fn close_login_window(app: AppHandle) {
    hide_window(&app, "login");
}

#[derive(Deserialize)]
struct SizePayload {
    width: f64,
    height: f64,
}

#[tauri::command]
fn resize_login_window_to_content(app: AppHandle, payload: SizePayload) -> Result<(), String> {
    let w = app.get_webview_window("login").ok_or("login window missing")?;
    w.set_size(LogicalSize::new(payload.width, payload.height)).map_err(|e| e.to_string())?;
    w.center().map_err(|e| e.to_string())
}

/// Kuro's update window offers "update now", which runs the Windows updater; on macOS show the
/// rebuild notice instead. The frontend calls this from the settings "check for updates" button
/// and from its silent check (at startup and every 30 minutes, once per version per session).
#[tauri::command]
fn open_update_window(app: AppHandle, detail: Option<Value>) {
    let d = detail.unwrap_or(Value::Null);
    let latest = d["newVersion"].as_str().unwrap_or("?").to_string();
    let current = d["currentVersion"].as_str().map_or_else(|| app.package_info().version.to_string(), str::to_string);
    static OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    // The silent check and the settings button can both ask while the notice is still up.
    if OPEN.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        show_rebuild_notice(&app, &latest, &current);
        OPEN.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

#[tauri::command]
fn close_update_window(app: AppHandle) {
    hide_window(&app, "update");
}

#[tauri::command]
fn open_maintenance_notice(app: AppHandle, payload: Option<Value>) -> Result<(), String> {
    *app.state::<Shared>().maintenance.lock().unwrap() = payload.clone();
    let aux = Aux { label: "update", route: "index.html#/update".into(), size: (960.0, 720.0) };
    open_aux(&app, aux, Some(("maintenance-notice-open", payload.unwrap_or(Value::Null)))).map(|_| ())
}

#[tauri::command]
fn get_maintenance_notice_payload(app: AppHandle) -> Option<Value> {
    app.state::<Shared>().maintenance.lock().unwrap().clone()
}

#[tauri::command]
fn has_maintenance_notice(app: AppHandle) -> bool {
    app.state::<Shared>().maintenance.lock().unwrap().is_some()
}

#[tauri::command]
fn hide_main_after_maintenance_notice_ready(app: AppHandle) {
    hide_window(&app, MAIN);
}

#[derive(Deserialize)]
struct PanelPayload {
    tab: Option<String>,
}

#[tauri::command]
fn open_panel_window(app: AppHandle, payload: PanelPayload) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("panel") {
        w.show().map_err(|e| e.to_string())?;
        AUX_SHOWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = w.set_focus();
        if let Some(tab) = payload.tab {
            let _ = app.emit_to("panel", "panel-open-tab", tab);
        }
        return Ok(());
    }
    // A new panel reads its first tab from the route query.
    let route = match &payload.tab {
        Some(tab) => format!("index.html#/panel?tab={tab}"),
        None => "index.html#/panel".into(),
    };
    open_aux(&app, Aux { label: "panel", route, size: (1100.0, 760.0) }, None).map(|_| ())
}

/// Hide rather than destroy: the settings "login" button closes the panel and then opens login
/// from the panel's own script, which a destroyed window would never get to run.
#[tauri::command]
fn close_panel_window(app: AppHandle) {
    hide_window(&app, "panel");
}

#[tauri::command]
fn raise_panel_window_above_pets(app: AppHandle) {
    if let Some(w) = app.get_webview_window("panel") {
        let _ = w.set_focus();
    }
}

#[derive(Deserialize)]
struct CollapsedPayload {
    collapsed: bool,
}

#[tauri::command]
fn set_panel_window_collapsed(app: AppHandle, payload: CollapsedPayload) -> Result<(), String> {
    let w = app.get_webview_window("panel").ok_or("panel window missing")?;
    // shortcut: collapsed width = CSS tab rail (101px) + margin; the Windows value is unverified.
    let width = if payload.collapsed { 112.0 } else { 1100.0 };
    w.set_size(LogicalSize::new(width, 760.0)).map_err(|e| e.to_string())
}

#[tauri::command]
fn show_toast_overlay(app: AppHandle) -> Result<(), String> {
    let w = app.get_webview_window("toast").ok_or("toast window missing")?;
    if let Ok(m) = main_monitor(&app) {
        let _ = cover_monitor(&w, &m);
    }
    let _ = w.set_ignore_cursor_events(true);
    w.show().map_err(|e| e.to_string())
}

#[tauri::command]
fn sync_aux_windows_to_main_monitor(app: AppHandle) -> Result<(), String> {
    if let (Some(w), Ok(m)) = (app.get_webview_window("toast"), main_monitor(&app)) {
        cover_monitor(&w, &m).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------- main window placement ----------

#[derive(Deserialize)]
struct PositionPayload {
    #[serde(rename = "alwaysOnTop")]
    always_on_top: Option<bool>,
}

#[tauri::command]
fn apply_main_window_position(app: AppHandle, payload: PositionPayload) -> Result<(), String> {
    main_window(&app)?
        .set_always_on_top(payload.always_on_top.unwrap_or(true))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_main_window_fullscreen_shell(app: AppHandle) -> Result<(), String> {
    let w = main_window(&app)?;
    cover_monitor(&w, &main_monitor(&app)?).map_err(|e| e.to_string())?;
    refresh_geometry(&app);
    Ok(())
}

#[derive(Deserialize)]
struct BoundsPayload {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[tauri::command]
fn set_main_window_monitor_bounds(app: AppHandle, payload: BoundsPayload) -> Result<(), String> {
    let w = main_window(&app)?;
    w.set_position(PhysicalPosition::new(payload.x, payload.y)).map_err(|e| e.to_string())?;
    w.set_size(PhysicalSize::new(payload.width, payload.height)).map_err(|e| e.to_string())?;
    refresh_geometry(&app);
    Ok(())
}

/// Only used by the non-fullscreen layout; the frontend returns early in fullscreen-shell mode.
#[tauri::command]
fn set_main_window_content_bounds(payload: Value) {
    let _ = payload;
}

#[tauri::command]
fn sync_main_window_to_current_monitor(app: AppHandle) {
    refresh_geometry(&app);
    reapply_ignore(&app);
}

#[tauri::command]
fn finish_main_window_monitor_switch(app: AppHandle) {
    refresh_geometry(&app);
    reapply_ignore(&app);
}

#[tauri::command]
fn ensure_main_taskbar_icon() {}

/// "Show taskbar icon" setting (1.0.3+). shortcut: macOS has no taskbar and this app never shows a Dock
/// icon, so the setting has no effect; map it to a Dock icon (activation policy Regular) if users ask.
#[tauri::command]
fn set_main_skip_taskbar(skip: bool) {
    let _ = skip;
}

// ---------- app shortcuts (1.0.3+) ----------
// The Windows host registers these as global hotkeys and emits `app-shortcut` with the action name.
// Bindings use Windows virtual-key codes and modifier bits Alt 1, Ctrl 2, Shift 4, Win 8 (Cmd here).

static SHORTCUTS: Mutex<Option<Value>> = Mutex::new(None);
static SHORTCUT_CAPTURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn shortcuts(app: &AppHandle) -> Value {
    let mut cache = SHORTCUTS.lock().unwrap();
    cache
        .get_or_insert_with(|| {
            // Defaults as in the frontend's DEFAULT_APP_SHORTCUTS: F7, F8, F9.
            let mut v = json!({
                "gameMode": { "modifiers": 0, "key": 118 },
                "switchScreen": { "modifiers": 0, "key": 119 },
                "togglePet": { "modifiers": 0, "key": 120 },
            });
            if let Some(saved) = read_store(app)["shortcuts"].as_object() {
                for (k, b) in saved {
                    if v.get(k).is_some() && valid_binding(b) {
                        v[k] = b.clone();
                    }
                }
            }
            v
        })
        .clone()
}

fn binding(b: &Value) -> (u64, u64) {
    (b["modifiers"].as_u64().unwrap_or(0), b["key"].as_u64().unwrap_or(0))
}

fn valid_binding(b: &Value) -> bool {
    matches!((b["modifiers"].as_u64(), b["key"].as_u64()), (Some(m), Some(k)) if m <= 15 && k <= 255)
}

#[tauri::command]
fn get_app_shortcuts(app: AppHandle) -> Value {
    shortcuts(&app)
}

#[tauri::command]
fn set_app_shortcuts(app: AppHandle, config: Value) -> Result<(), String> {
    // Keep only the known actions, each a {modifiers, key} pair (key 0 = unbound).
    let current = shortcuts(&app);
    let mut clean = serde_json::Map::new();
    let mut seen = std::collections::HashSet::new();
    for action in current.as_object().into_iter().flat_map(|m| m.keys()) {
        let b = config.get(action).filter(|b| valid_binding(b)).ok_or("unavailable")?;
        let (m, k) = binding(b);
        if k != 0 && !seen.insert((m, k)) {
            return Err("duplicate".into());
        }
        clean.insert(action.clone(), json!({ "modifiers": m, "key": k }));
    }
    update_store(&app, |v| v["shortcuts"] = Value::Object(clean))?;
    *SHORTCUTS.lock().unwrap() = None;
    let _ = app.emit("app-shortcuts-changed", shortcuts(&app));
    Ok(())
}

/// The settings page pauses shortcuts while it records a new one.
#[tauri::command]
fn set_shortcut_capture(active: bool) {
    SHORTCUT_CAPTURE.store(active, std::sync::atomic::Ordering::Relaxed);
}

/// Called from the input thread on every key press (not auto-repeat).
pub(crate) fn trigger_shortcut(app: &AppHandle, modifiers: u64, key: u64) {
    if SHORTCUT_CAPTURE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    if key == 0 {
        return;
    }
    if SHORTCUTS.lock().unwrap().is_none() {
        shortcuts(app); // first key press after launch: load once from the store
    }
    // Look up under the lock without cloning: this runs inside the event tap callback.
    let action = SHORTCUTS.lock().unwrap().as_ref().and_then(Value::as_object).and_then(|m| {
        m.iter().find(|(_, b)| binding(b) == (modifiers, key)).map(|(a, _)| a.clone())
    });
    if let Some(action) = action {
        let _ = app.emit("app-shortcut", action);
    }
}

fn insets(app: &AppHandle) -> Result<Value, String> {
    let m = main_monitor(app)?;
    let s = m.scale_factor();
    let (mp, ms, wa) = (m.position(), m.size(), m.work_area());
    let left = (wa.position.x - mp.x).max(0) as f64 / s;
    let top = (wa.position.y - mp.y).max(0) as f64 / s;
    let right = ((mp.x + ms.width as i32) - (wa.position.x + wa.size.width as i32)).max(0) as f64 / s;
    let bottom = ((mp.y + ms.height as i32) - (wa.position.y + wa.size.height as i32)).max(0) as f64 / s;
    Ok(json!({ "left": left, "top": top, "right": right, "bottom": bottom }))
}

#[tauri::command]
fn get_main_desktop_safe_area(app: AppHandle) -> Result<Value, String> {
    Ok(json!({
        "insets": insets(&app)?,
        "autoHideEdge": null,
        "foregroundFullscreen": false,
        // 1.0.3+: drives "auto game mode in full screen".
        "foregroundApplicationFullscreen": foreground_app_fullscreen(),
    }))
}

/// True when the frontmost other app has a normal-level window that exactly covers one display
/// (a full-screen game or video), on any display.
fn covers(w: &core_graphics::geometry::CGRect, d: &core_graphics::geometry::CGRect) -> bool {
    let near = |a: f64, b: f64| (a - b).abs() < 1.0;
    near(w.origin.x, d.origin.x)
        && near(w.origin.y, d.origin.y)
        && w.size.width >= d.size.width - 1.0
        && w.size.height >= d.size.height - 1.0
}

fn foreground_app_fullscreen() -> bool {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::display::CGDisplay;
    use core_graphics::geometry::CGRect;
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
    };
    let Some(front) = objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication() else { return false };
    let pid = front.processIdentifier() as i64;
    if pid == std::process::id() as i64 {
        return false;
    }
    let screens: Vec<CGRect> =
        CGDisplay::active_displays().unwrap_or_default().into_iter().map(|d| CGDisplay::new(d).bounds()).collect();
    let Some(list) = copy_window_info(kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID) else {
        return false;
    };
    list.iter().any(|w| {
        let d: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_get_rule(*w as _) };
        let num = |k: &'static str| d.find(CFString::from_static_string(k)).and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_i64());
        if num("kCGWindowOwnerPID") != Some(pid) || num("kCGWindowLayer") != Some(0) {
            return false;
        }
        let bounds = d.find(CFString::from_static_string("kCGWindowBounds")).and_then(|v| v.downcast::<CFDictionary>());
        bounds.and_then(|b| CGRect::from_dict_representation(&b)).is_some_and(|r| screens.iter().any(|d| covers(&r, d)))
    })
}

#[tauri::command]
fn get_main_taskbar_insets(app: AppHandle) -> Result<Value, String> {
    insets(&app)
}

// ---------- click-through ----------

#[derive(Deserialize)]
struct HitPayload {
    regions: Vec<Rect>,
    #[serde(rename = "inputRegions", default)]
    input_regions: Vec<Rect>,
    #[serde(rename = "updateNativeInput", default)]
    update_native_input: bool,
    #[serde(rename = "windowPassthrough", default)]
    window_passthrough: bool,
}

#[tauri::command]
fn set_main_hit_region(app: AppHandle, payload: HitPayload) {
    let st = app.state::<Shared>();
    let mut hit = st.hit.lock().unwrap();
    hit.regions = payload.regions;
    // An empty list without updateNativeInput means "unchanged", not "none".
    if payload.update_native_input {
        hit.input_regions = payload.input_regions;
    }
    hit.window_passthrough = payload.window_passthrough;
    hit.ignoring = None;
}

#[tauri::command]
fn set_main_swallow_mouse(app: AppHandle, enabled: bool) {
    let st = app.state::<Shared>();
    let mut hit = st.hit.lock().unwrap();
    hit.swallow = enabled;
    hit.ignoring = None;
}

/// The Windows build keeps the main WebView cursor-ignored and routes input natively;
/// here click-through is decided per pointer event in `on_pointer`, so nothing to do.
#[tauri::command]
fn set_main_force_cursor_ignore(enabled: bool) {
    let _ = enabled;
}

fn set_cursor(app: &AppHandle, kind: &str) {
    use tauri::CursorIcon::*;
    let icon = match kind {
        "pointer" => Hand,
        "grab" => Grab,
        "grabbing" => Grabbing,
        "move" => Move,
        "text" => Text,
        "not-allowed" => NotAllowed,
        _ => Default,
    };
    if let Some(w) = app.get_webview_window(MAIN) {
        let _ = w.set_cursor_icon(icon);
    }
}

#[tauri::command]
fn set_native_input_cursor(app: AppHandle, kind: String) {
    set_cursor(&app, &kind);
}

#[tauri::command]
fn set_overlay_cursor(app: AppHandle, kind: String) {
    set_cursor(&app, &kind);
}

// ---------- menus ----------

fn build_menu(app: &AppHandle, prefix: &str, s: &MenuState) -> tauri::Result<Menu<tauri::Wry>> {
    let id = |k: &str| format!("{prefix}_{k}");
    let item = |k: &str, label: String| MenuItem::with_id(app, id(k), label, true, None::<&str>);
    let scale = Submenu::with_id(app, id("window_size"), s.label("windowSize", "Pet Size"), true)?;
    for v in PET_SCALES {
        let it = CheckMenuItem::with_id(app, id(&format!("pet_scale_{v}")), format!("{v}%"), true, s.pet_scale == v, None::<&str>)?;
        scale.append(&it)?;
    }
    let opacity = Submenu::with_id(app, id("window_opacity"), s.label("windowOpacity", "Opacity"), true)?;
    for v in OPACITIES {
        let it = CheckMenuItem::with_id(app, id(&format!("window_opacity_{v}")), format!("{v}%"), true, s.window_opacity == v, None::<&str>)?;
        opacity.append(&it)?;
    }
    let toggle_pet = if s.pet_dock_visible { s.label("hidePet", "Hide Pet") } else { s.label("showPet", "Show Pet") };
    let settings_key = if prefix == "tray" { "trayOpenSettings" } else { "dockSettings" };
    let passthrough = CheckMenuItem::with_id(app, id("window_passthrough"), s.label("windowPassthrough", "Click-through"), true, s.window_passthrough, None::<&str>)?;
    let game_mode = CheckMenuItem::with_id(app, id("game_mode"), s.label("gameMode", "Game Mode"), true, s.game_mode, None::<&str>)?;
    let sep = || PredefinedMenuItem::separator(app);
    Menu::with_items(
        app,
        &[
            &item("bag", s.label("openBag", "Bag"))?,
            &item("atlas", s.label("openAtlas", "Atlas"))?,
            &item("exchange", s.label("openExchange", "Exchange"))?,
            &item("room", s.label("openRoom", "Room"))?,
            &sep()?,
            &item("toggle_pet", toggle_pet)?,
            &item("settings", s.label(settings_key, "Settings"))?,
            &scale,
            &opacity,
            &passthrough,
            &game_mode,
            &sep()?,
            &item("restart", s.label("restart", "Restart"))?,
            &item("quit", s.label("quit", "Quit"))?,
        ],
    )
}

fn on_menu(app: &AppHandle, id: &str) {
    let Some(key) = id.strip_prefix("dock_").or_else(|| id.strip_prefix("tray_")) else { return };
    let emit = |ev: &str, payload: Value| {
        let _ = app.emit_to(MAIN, ev, payload);
    };
    match key {
        "bag" | "atlas" | "exchange" | "room" => emit("dock-open-panel", json!(key)),
        "settings" => emit("dock-open-panel", json!("setting")),
        "toggle_pet" => emit("tray-toggle-pet-visibility", Value::Null),
        "window_passthrough" => emit("tray-toggle-window-passthrough", Value::Null),
        "game_mode" => emit("tray-toggle-game-mode", Value::Null),
        "restart" => emit("tray-restart", Value::Null),
        "quit" => emit("tray-quit", Value::Null),
        _ => {
            if let Some(v) = key.strip_prefix("pet_scale_").and_then(|v| v.parse::<u32>().ok()) {
                emit("tray-set-pet-scale", json!(v));
            } else if let Some(v) = key.strip_prefix("window_opacity_").and_then(|v| v.parse::<u32>().ok()) {
                emit("tray-set-window-opacity", json!(v));
            }
        }
    }
}

#[tauri::command]
fn sync_native_menu(
    app: AppHandle,
    labels: HashMap<String, Value>,
    pet_dock_visible: Option<bool>,
    pet_scale: Option<f64>,
    window_opacity: Option<f64>,
    window_passthrough: Option<bool>,
    game_mode: Option<bool>,
) -> Result<(), String> {
    let st = app.state::<Shared>();
    let snapshot = {
        let mut m = st.menu.lock().unwrap();
        m.labels = labels
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect();
        m.pet_dock_visible = pet_dock_visible.unwrap_or(true);
        m.pet_scale = pet_scale.unwrap_or(100.0).round() as u32;
        m.window_opacity = window_opacity.unwrap_or(100.0).round() as u32;
        m.window_passthrough = window_passthrough.unwrap_or(false);
        m.game_mode = game_mode.unwrap_or(false);
        m.clone()
    };
    if let Some(tray) = app.tray_by_id("main-tray") {
        let menu = build_menu(&app, "tray", &snapshot).map_err(|e| e.to_string())?;
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
        let _ = tray.set_tooltip(Some(snapshot.label("trayTooltip", "Wuwa Tappo")));
    }
    Ok(())
}

#[tauri::command]
fn popup_main_dock_context_menu(app: AppHandle, x: f64, y: f64) -> Result<(), String> {
    let snapshot = app.state::<Shared>().menu.lock().unwrap().clone();
    let menu = build_menu(&app, "dock", &snapshot).map_err(|e| e.to_string())?;
    main_window(&app)?
        .popup_menu_at(&menu, LogicalPosition::new(x, y))
        .map_err(|e| e.to_string())
}

// ---------- persistence ----------

fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("local-state.json"))
}

fn read_store(app: &AppHandle) -> Value {
    store_path(app)
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}))
}

/// Read-modify-write under a lock; writes via temp file + rename so a crash can't truncate counts.
fn update_store(app: &AppHandle, f: impl FnOnce(&mut Value)) -> Result<(), String> {
    let st = app.state::<Shared>();
    let _guard = st.store.lock().unwrap();
    let mut v = read_store(app);
    if !v.is_object() {
        v = json!({});
    }
    f(&mut v);
    let path = store_path(app)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&v).unwrap()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

#[tauri::command]
fn load_key_press_count(app: AppHandle, environment: String) -> Value {
    read_store(&app)["key_press"][&environment].clone().as_u64().map(Value::from).unwrap_or(json!(0))
}

#[tauri::command]
fn save_key_press_count(app: AppHandle, count: u64, environment: String) -> Result<(), String> {
    update_store(&app, |v| v["key_press"][&environment] = json!(count))
}

#[tauri::command]
fn load_cumulative_click_count(app: AppHandle, environment: String) -> Value {
    let e = &read_store(&app)["cumulative_click"][&environment];
    if e.is_null() {
        return json!({ "count": 0, "cuid": "", "environment": "" });
    }
    json!({ "count": e["count"].as_u64().unwrap_or(0), "cuid": e["cuid"].as_str().unwrap_or(""), "environment": environment })
}

#[tauri::command]
fn save_cumulative_click_count(app: AppHandle, count: u64, cuid: String, environment: String) -> Result<(), String> {
    update_store(&app, |v| v["cumulative_click"][&environment] = json!({ "count": count, "cuid": cuid }))
}

#[tauri::command]
fn save_app_ui_locale(app: AppHandle, locale: String) -> Result<(), String> {
    update_store(&app, |v| v["locale"] = json!(locale))
}

/// The app that was frontmost when a click on the pet began (0 = none).
static FOCUS_OWNER_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
/// Counts aux windows (panel, login, ...) shown, so a pet click that opened one keeps the focus there.
static AUX_SHOWN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AUX_SHOWN_AT_PRESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Called on mouse-down over the pet, before macOS activates this app for the click.
pub(crate) fn remember_focus_owner() {
    let front = objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication();
    let pid = front.map_or(0, |app| app.processIdentifier());
    let own = std::process::id() as i32;
    FOCUS_OWNER_PID.store(if pid == own { 0 } else { pid }, std::sync::atomic::Ordering::Relaxed);
    AUX_SHOWN_AT_PRESS.store(AUX_SHOWN.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed);
}

/// Clicking a window of a background app makes macOS activate that app, so after a click on the pet
/// the keyboard focus would stay here and the next keystrokes typed into the game or editor would be
/// lost. Pet clicks are handled from the global event tap anyway, so hand focus back to the app that
/// had it. macOS 14+ ignores a plain `deactivate`; activation must be yielded to a named app.
/// A click that opened the panel or login window keeps the focus there. The hand-back waits a moment
/// so that the frontend's window-open call (made on the same mouse release) lands first.
pub(crate) fn yield_focus_after_pet_click(app: &AppHandle) {
    let pid = FOCUS_OWNER_PID.swap(0, std::sync::atomic::Ordering::Relaxed);
    if pid == 0 {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let _ = handle.run_on_main_thread(move || {
            use objc2::runtime::NSObjectProtocol;
            use objc2_app_kit::{NSApplication, NSApplicationActivationOptions, NSRunningApplication};
            let ord = std::sync::atomic::Ordering::Relaxed;
            if AUX_SHOWN.load(ord) != AUX_SHOWN_AT_PRESS.load(ord) {
                return;
            }
            let (Some(mtm), Some(owner)) = (
                objc2::MainThreadMarker::new(),
                NSRunningApplication::runningApplicationWithProcessIdentifier(pid),
            ) else {
                return;
            };
            let ns_app = NSApplication::sharedApplication(mtm);
            if ns_app.isActive() {
                // yieldActivationToApplication: exists from macOS 14; earlier versions let the active
                // app activate another one directly.
                if ns_app.respondsToSelector(objc2::sel!(yieldActivationToApplication:)) {
                    ns_app.yieldActivationToApplication(&owner);
                }
                owner.activateWithOptions(NSApplicationActivationOptions::empty());
            }
        });
    });
}

// ---------- misc ----------

const INPUT_MONITORING_PANE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";

/// Called from the input thread when Input Monitoring is still missing after the system request.
pub(crate) fn explain_input_monitoring(app: &AppHandle) {
    let [title, body, open, later] = permission_text(&ui_locale(app));
    if front_dialog(app, title, body, open, later) {
        let _ = std::process::Command::new("open").arg(INPUT_MONITORING_PANE).status();
    }
}

/// The language chosen in the app (frontend codes: en tw jp kr fr de es pt th); before the frontend
/// has saved one (the permission dialog can come first), the system language.
fn ui_locale(app: &AppHandle) -> String {
    if let Some(l) = read_store(app)["locale"].as_str() {
        return l.to_string();
    }
    let out = std::process::Command::new("defaults").args(["read", "-g", "AppleLanguages"]).output();
    let langs = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    // Output looks like `(\n    "ko-KR",\n    en\n)`: the first entry, quoted or not.
    let first = langs.lines().nth(1).unwrap_or("").trim().trim_end_matches(',').trim_matches('"');
    locale_from_system(first).to_string()
}

fn locale_from_system(lang: &str) -> &'static str {
    match lang.split(['-', '_']).next().unwrap_or("") {
        "ko" => "kr",
        "ja" => "jp",
        "zh" => "tw",
        "fr" => "fr",
        "de" => "de",
        "es" => "es",
        "pt" => "pt",
        "th" => "th",
        _ => "en",
    }
}

/// [title, body, open button, later button]
fn permission_text(locale: &str) -> [&'static str; 4] {
    match locale {
        "kr" => [
            "입력 모니터링 권한이 필요합니다",
            "Wuwa Tappo가 키 입력과 클릭을 세려면 입력 모니터링 권한이 필요합니다.\n\n\
             '설정 열기'를 누른 뒤 목록에서 Wuwa Tappo를 켜 주세요. 목록에 없으면 아래 '+'를 눌러 \
             응용 프로그램 폴더의 Wuwa Tappo를 추가하면 됩니다. 이미 켜져 있는데도 이 창이 뜨면 \
             이전 빌드의 항목이니 '−'로 지우고 다시 추가하세요. 켜면 앱이 다시 시작됩니다.",
            "설정 열기",
            "나중에",
        ],
        "jp" => [
            "入力監視の許可が必要です",
            "Wuwa Tappo がキー入力とクリックを数えるには、入力監視の許可が必要です。\n\n\
             「設定を開く」を押して、一覧の Wuwa Tappo をオンにしてください。一覧にない場合は、下の「+」を押して\
             アプリケーションフォルダの Wuwa Tappo を追加してください。すでにオンなのにこの画面が出る場合は\
             以前のビルドの項目なので、「−」で削除してからもう一度追加してください。オンにするとアプリが再起動します。",
            "設定を開く",
            "後で",
        ],
        "tw" => [
            "需要「輸入監控」權限",
            "Wuwa Tappo 需要「輸入監控」權限才能計算按鍵與點擊次數。\n\n\
             請按「打開設定」，然後在列表中開啟 Wuwa Tappo。若列表中沒有，請按下方的「+」，從「應用程式」\
             資料夾加入 Wuwa Tappo。若已開啟卻仍看到此視窗，表示那是舊版建置的項目，請用「−」移除後再重新加入。\
             開啟後 App 會重新啟動。",
            "打開設定",
            "稍後",
        ],
        "fr" => [
            "Autorisation « Surveillance de l'entrée » requise",
            "Wuwa Tappo a besoin de la surveillance de l'entrée pour compter les frappes et les clics.\n\n\
             Cliquez sur « Ouvrir les réglages » et activez Wuwa Tappo. S'il n'apparaît pas, cliquez sur « + » \
             et ajoutez Wuwa Tappo depuis le dossier Applications. S'il est déjà activé et que ce message \
             s'affiche encore, l'entrée vient d'une version précédente : supprimez-la avec « − » puis \
             ajoutez-la de nouveau. L'app redémarre une fois l'autorisation activée.",
            "Ouvrir les réglages",
            "Plus tard",
        ],
        "de" => [
            "Berechtigung „Eingabeüberwachung“ erforderlich",
            "Wuwa Tappo benötigt die Eingabeüberwachung, um Tastenanschläge und Klicks zu zählen.\n\n\
             Klicke auf „Einstellungen öffnen“ und aktiviere Wuwa Tappo. Falls es nicht aufgeführt ist, klicke \
             auf „+“ und füge Wuwa Tappo aus dem Ordner „Programme“ hinzu. Wenn es bereits aktiviert ist und du \
             diese Meldung trotzdem siehst, gehört der Eintrag zu einem früheren Build: Entferne ihn mit „−“ \
             und füge ihn erneut hinzu. Nach dem Aktivieren startet die App neu.",
            "Einstellungen öffnen",
            "Später",
        ],
        "es" => [
            "Se necesita el permiso de Monitorización de entrada",
            "Wuwa Tappo necesita la Monitorización de entrada para contar las pulsaciones de teclas y los clics.\n\n\
             Haz clic en «Abrir Ajustes» y activa Wuwa Tappo. Si no aparece en la lista, haz clic en «+» y añade \
             Wuwa Tappo desde la carpeta Aplicaciones. Si ya está activado y sigues viendo este aviso, esa \
             entrada es de una compilación anterior: elimínala con «−» y vuelve a añadirla. La app se reinicia \
             al activarlo.",
            "Abrir Ajustes",
            "Más tarde",
        ],
        "pt" => [
            "Permissão de Monitoramento de Entrada necessária",
            "O Wuwa Tappo precisa do Monitoramento de Entrada para contar as teclas pressionadas e os cliques.\n\n\
             Clique em \"Abrir Ajustes\" e ative o Wuwa Tappo. Se ele não estiver na lista, clique em \"+\" e \
             adicione o Wuwa Tappo da pasta Aplicativos. Se já estiver ativado e esta mensagem continuar \
             aparecendo, o item é de uma versão anterior: remova-o com \"−\" e adicione-o novamente. O app \
             reinicia depois que você ativar.",
            "Abrir Ajustes",
            "Mais tarde",
        ],
        "th" => [
            "ต้องการสิทธิ์การตรวจสอบการป้อนข้อมูล",
            "Wuwa Tappo ต้องใช้สิทธิ์การตรวจสอบการป้อนข้อมูลเพื่อนับการกดแป้นและการคลิก\n\n\
             คลิก \"เปิดการตั้งค่า\" แล้วเปิด Wuwa Tappo ในรายการ หากไม่มีในรายการ ให้คลิก \"+\" แล้วเพิ่ม \
             Wuwa Tappo จากโฟลเดอร์แอปพลิเคชัน หากเปิดอยู่แล้วแต่ยังเห็นหน้าต่างนี้ แสดงว่าเป็นรายการของบิลด์ก่อนหน้า \
             ให้ลบด้วย \"−\" แล้วเพิ่มใหม่ แอปจะรีสตาร์ทหลังจากเปิดสิทธิ์",
            "เปิดการตั้งค่า",
            "ภายหลัง",
        ],
        _ => [
            "Input Monitoring permission needed",
            "Wuwa Tappo needs Input Monitoring to count key presses and clicks.\n\n\
             Click 'Open Settings' and turn on Wuwa Tappo. If it is not listed, click '+' and add \
             Wuwa Tappo from the Applications folder. If it is already on and you still see this, \
             that entry belongs to an earlier build: remove it with '-' and add it again. \
             The app restarts after you turn it on.",
            "Open Settings",
            "Later",
        ],
    }
}

/// [body with {latest}/{current}, button] when the app can rebuild itself from its clone.
fn update_now_text(locale: &str) -> [&'static str; 2] {
    match locale {
        "kr" => [
            "Wuwa Tappo {latest}이(가) 나왔습니다. 지금 앱은 {current}입니다.\n\n\
             '지금 업데이트'를 누르면 터미널 창에서 새 버전을 받아 다시 빌드하고 설치합니다. \
             몇 분 걸리며, 끝나면 앱이 다시 켜집니다.",
            "지금 업데이트",
        ],
        "jp" => [
            "Wuwa Tappo {latest} が公開されました。現在のアプリは {current} です。\n\n\
             「今すぐ更新」を押すと、ターミナルで新しいバージョンをダウンロードしてビルドし直し、インストールします。\
             数分かかり、終わるとアプリが再起動します。",
            "今すぐ更新",
        ],
        "tw" => [
            "Wuwa Tappo {latest} 已推出，目前的 App 為 {current}。\n\n\
             按「立即更新」後，會在終端機視窗中下載並重新建置、安裝新版本。需要幾分鐘，完成後 App 會重新啟動。",
            "立即更新",
        ],
        "fr" => [
            "Wuwa Tappo {latest} est sorti. Cette app est en version {current}.\n\n\
             Cliquez sur « Mettre à jour » pour télécharger, recompiler et installer la nouvelle version \
             dans une fenêtre du Terminal. Cela prend quelques minutes, puis l'app redémarre.",
            "Mettre à jour",
        ],
        "de" => [
            "Wuwa Tappo {latest} ist erschienen. Diese App ist Version {current}.\n\n\
             Klicke auf „Jetzt aktualisieren“, um die neue Version in einem Terminal-Fenster herunterzuladen, \
             neu zu bauen und zu installieren. Das dauert einige Minuten; danach startet die App neu.",
            "Jetzt aktualisieren",
        ],
        "es" => [
            "Ya está disponible Wuwa Tappo {latest}. Esta app es la versión {current}.\n\n\
             Haz clic en «Actualizar ahora» para descargar, compilar e instalar la nueva versión en una \
             ventana de Terminal. Tarda unos minutos y la app se reinicia al terminar.",
            "Actualizar ahora",
        ],
        "pt" => [
            "O Wuwa Tappo {latest} foi lançado. Este app está na versão {current}.\n\n\
             Clique em \"Atualizar agora\" para baixar, compilar e instalar a nova versão em uma janela do \
             Terminal. Leva alguns minutos, e o app reinicia ao terminar.",
            "Atualizar agora",
        ],
        "th" => [
            "Wuwa Tappo {latest} เปิดตัวแล้ว แอปนี้เป็นเวอร์ชัน {current}\n\n\
             คลิก \"อัปเดตเลย\" เพื่อดาวน์โหลด บิลด์ และติดตั้งเวอร์ชันใหม่ในหน้าต่าง Terminal \
             ใช้เวลาสักครู่ และแอปจะรีสตาร์ทเมื่อเสร็จ",
            "อัปเดตเลย",
        ],
        _ => [
            "Wuwa Tappo {latest} has been released. This app is {current}.\n\n\
             Click 'Update Now' to download, rebuild and install the new version in a Terminal window. \
             It takes a few minutes, and the app restarts when it is done.",
            "Update Now",
        ],
    }
}

/// [title, body with {latest}/{current}, open button, close button]
fn update_text(locale: &str) -> [&'static str; 4] {
    match locale {
        "kr" => [
            "새 버전이 나왔습니다",
            "Wuwa Tappo {latest}이(가) 나왔습니다. 지금 앱은 {current}입니다.\n\n\
             macOS용은 자동으로 업데이트되지 않습니다. 저장소 폴더에서 tools/build.sh를 \
             다시 실행하면 새 버전을 받아 다시 빌드합니다.",
            "GitHub 열기",
            "닫기",
        ],
        "jp" => [
            "新しいバージョンがあります",
            "Wuwa Tappo {latest} が公開されました。現在のアプリは {current} です。\n\n\
             macOS 版は自動では更新されません。リポジトリのフォルダで tools/build.sh をもう一度実行すると、\
             新しいバージョンをダウンロードしてビルドし直します。",
            "GitHub を開く",
            "閉じる",
        ],
        "tw" => [
            "有新版本",
            "Wuwa Tappo {latest} 已推出，目前的 App 為 {current}。\n\n\
             macOS 版不會自動更新。請在儲存庫資料夾中再次執行 tools/build.sh，即可下載並重新建置新版本。",
            "開啟 GitHub",
            "關閉",
        ],
        "fr" => [
            "Une nouvelle version est disponible",
            "Wuwa Tappo {latest} est sorti. Cette app est en version {current}.\n\n\
             La version macOS ne se met pas à jour toute seule. Relancez tools/build.sh dans le dossier \
             du dépôt pour télécharger et recompiler la nouvelle version.",
            "Ouvrir GitHub",
            "Fermer",
        ],
        "de" => [
            "Eine neue Version ist verfügbar",
            "Wuwa Tappo {latest} ist erschienen. Diese App ist Version {current}.\n\n\
             Die macOS-Version aktualisiert sich nicht selbst. Führe tools/build.sh im Repository-Ordner \
             erneut aus, um die neue Version herunterzuladen und neu zu bauen.",
            "GitHub öffnen",
            "Schließen",
        ],
        "es" => [
            "Hay una nueva versión disponible",
            "Ya está disponible Wuwa Tappo {latest}. Esta app es la versión {current}.\n\n\
             La versión para macOS no se actualiza sola. Vuelve a ejecutar tools/build.sh en la carpeta \
             del repositorio para descargar y compilar la nueva versión.",
            "Abrir GitHub",
            "Cerrar",
        ],
        "pt" => [
            "Uma nova versão está disponível",
            "O Wuwa Tappo {latest} foi lançado. Este app está na versão {current}.\n\n\
             A versão para macOS não se atualiza sozinha. Execute tools/build.sh novamente na pasta do \
             repositório para baixar e compilar a nova versão.",
            "Abrir GitHub",
            "Fechar",
        ],
        "th" => [
            "มีเวอร์ชันใหม่",
            "Wuwa Tappo {latest} เปิดตัวแล้ว แอปนี้เป็นเวอร์ชัน {current}\n\n\
             เวอร์ชัน macOS ไม่อัปเดตเอง ให้เรียกใช้ tools/build.sh อีกครั้งในโฟลเดอร์ของรีโพสิทอรี\
             เพื่อดาวน์โหลดและบิลด์เวอร์ชันใหม่",
            "เปิด GitHub",
            "ปิด",
        ],
        _ => [
            "A new version is available",
            "Wuwa Tappo {latest} has been released. This app is {current}.\n\n\
             The macOS build does not update itself. Run tools/build.sh again in the \
             repository folder to download and rebuild the new version.",
            "Open GitHub",
            "Close",
        ],
    }
}

/// Two-button alert brought to the front; true when the first button is chosen. Blocks, so call
/// it off the main thread.
fn front_dialog(app: &AppHandle, title: &str, body: &str, ok: &str, cancel: &str) -> bool {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    // A Dock-less (accessory) app is never frontmost, so its alert would open behind other windows.
    let _ = app.run_on_main_thread(|| {
        if let Some(mtm) = objc2::MainThreadMarker::new() {
            #[allow(deprecated)]
            objc2_app_kit::NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        }
    });
    app.dialog()
        .message(body)
        .title(title)
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(ok.into(), cancel.into()))
        .blocking_show()
}

// ---------- update notice ----------

/// Kuro's release manifest (what the official app's updater reads). The Windows updater cannot
/// install on macOS, so a newer version only produces a notice to rebuild with tools/build.sh.
const RELEASE_HOST_JSON: &str =
    "https://wutheringwaves-wuwatappo.kurogames-global.com/app/json-host/latest-global_host.json";
const KURO_ASSETS_PREFIX: &str = "https://wutheringwaves-wuwatappo.kurogames-global.com/";
const REPO_URL: &str = "https://github.com/atimevil/wuwa-tappo-mac";

fn fetch_json(url: &str) -> Option<Value> {
    let out = std::process::Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", "15", url])
        .output()
        .ok()?;
    out.status.success().then(|| serde_json::from_slice(&out.stdout).ok()).flatten()
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.trim().split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

fn latest_manifest() -> Result<Value, String> {
    let host = fetch_json(RELEASE_HOST_JSON).ok_or("could not reach Kuro's release host")?;
    let manifest_url =
        host["url"].as_str().filter(|u| u.starts_with(KURO_ASSETS_PREFIX)).ok_or("unexpected release host data")?;
    fetch_json(manifest_url).ok_or_else(|| "could not read Kuro's release manifest".to_string())
}

/// The clone this app was built from (set by tools/build.sh), if it is still there.
fn local_repo() -> Option<PathBuf> {
    option_env!("WUWA_REPO_DIR").map(PathBuf::from).filter(|p| p.join("tools/build.sh").is_file())
}

/// Terminal script for "Update now": pull the port (rebasing any local commits on top, so a clone
/// with its own changes keeps them), then build.sh downloads Kuro's latest
/// installer, rebuilds, reinstalls and relaunches the app (it quits this one first).
fn update_script(repo: &std::path::Path) -> String {
    let quoted = format!("'{}'", repo.display().to_string().replace('\'', r"'\''"));
    format!(
        r#"#!/bin/zsh -l
cd {quoted} || exit 1
export PATH="/opt/homebrew/opt/rustup/bin:/usr/local/opt/rustup/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
echo "==> updating the macOS port (git pull)"
git pull --rebase --autostash || {{ git rebase --abort 2>/dev/null; echo "git pull failed; building with the current checkout"; }}
tools/build.sh
rc=$?
echo
if [ $rc = 0 ]; then echo "Update finished. You can close this window."; else echo "Update failed (exit $rc). See the messages above."; fi
"#
    )
}

fn start_update(repo: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join("wuwa-tappo-update.command");
    std::fs::write(&path, update_script(repo))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    // `open` hands a .command file to Terminal, so the build runs outside this (soon replaced) app.
    std::process::Command::new("open").arg(&path).status().map(|_| ())
}

fn show_rebuild_notice(app: &AppHandle, latest: &str, current: &str) {
    let locale = ui_locale(app);
    let [title, mut body, mut open, close] = update_text(&locale);
    let repo = local_repo();
    if repo.is_some() {
        [body, open] = update_now_text(&locale);
    }
    let body = body.replace("{latest}", latest).replace("{current}", current);
    if !front_dialog(app, title, &body, open, close) {
        return;
    }
    match repo {
        Some(repo) => {
            if let Err(e) = start_update(&repo) {
                log::error!("[update] could not start the update: {e}");
                let _ = std::process::Command::new("open").arg(REPO_URL).status();
            }
        }
        None => {
            let _ = std::process::Command::new("open").arg(REPO_URL).status();
        }
    }
}

#[tauri::command]
fn write_app_log(level: String, message: String) {
    match level.as_str() {
        "error" => log::error!("{message}"),
        "warn" => log::warn!("{message}"),
        "debug" => log::debug!("{message}"),
        _ => log::info!("{message}"),
    }
}

#[tauri::command]
fn get_track_os_info() -> Value {
    let ver = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    json!({ "os": "macOS", "osVersion": ver })
}

/// Answers the frontend's updater check from Kuro's release manifest. The frontend then opens the
/// update window, which on macOS is the rebuild notice (see open_update_window); `rid` is never used
/// because nothing is downloaded. The endpoint from the page is ignored: only Kuro's fixed URL is fetched.
#[tauri::command]
async fn check_app_update(app: AppHandle, endpoint: Option<String>) -> Result<Option<Value>, String> {
    let _ = endpoint;
    // When Kuro cannot be reached this is an error, not "no update": the settings button then reports
    // a failed check instead of "up to date" (the silent check treats errors as up to date).
    let manifest = tauri::async_runtime::spawn_blocking(latest_manifest).await.map_err(|e| e.to_string())??;
    let latest = manifest["version"].as_str().ok_or("manifest has no version")?.to_string();
    let current = app.package_info().version.to_string();
    if !is_newer(&latest, &current) {
        return Ok(None);
    }
    log::info!("[update] Kuro released {latest} (this build: {current})");
    Ok(Some(json!({
        "rid": 0,
        "currentVersion": current,
        "version": latest,
        "date": manifest["pub_date"],
        "body": manifest["notes"],
        "rawJson": manifest,
    })))
}

/// The update window is never opened on macOS (see open_update_window), so there is no detail.
#[tauri::command]
fn get_app_update_detail() -> Option<Value> {
    None
}

#[tauri::command]
fn is_force_update_locked() -> bool {
    false
}

/// The frontend only calls these on Windows (it uses the autostart plugin elsewhere).
#[tauri::command]
fn is_elevated_autostart_enabled() -> bool {
    false
}

#[tauri::command]
fn set_elevated_autostart(enabled: bool) {
    let _ = enabled;
}

#[tauri::command]
fn exit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn is_internal_testing_build() -> bool {
    false
}

#[tauri::command]
fn is_devtools_hotkeys_build() -> bool {
    false
}

#[tauri::command]
fn get_boot_diagnostics() -> Option<Value> {
    None
}

// ---------- setup ----------

fn create_main(app: &AppHandle) -> tauri::Result<()> {
    let builder = WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
        .title("Wuwa Tappo")
                .initialization_script(CONSOLE_BRIDGE_SCRIPT)
                .initialization_script(COOKIE_SHIM_SCRIPT)
        .decorations(false)
                .accept_first_mouse(true)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .visible_on_all_workspaces(true)
        .initialization_script(MAIN_INIT_SCRIPT);
    let builder = match ws_proxy::start() {
        Ok(port) => builder.initialization_script(&ws_proxy::rewrite_script(port)),
        Err(e) => {
            log::error!("[ws-proxy] disabled: {e}");
            builder
        }
    };
    // Debug builds only: tools/qa_invoke.js exercises every host command (see README).
    #[cfg(debug_assertions)]
    let builder = match std::env::var("WUWA_QA_SCRIPT").ok().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(qa) => builder.initialization_script(&qa),
        None => builder,
    };
    let w = builder.build()?;
    if let Ok(m) = main_monitor(app) {
        cover_monitor(&w, &m)?;
    }
    w.set_ignore_cursor_events(true)?;
    let handle = app.clone();
    w.on_window_event(move |e| {
        if matches!(e, WindowEvent::Moved(_) | WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }) {
            refresh_geometry(&handle);
        }
    });
    refresh_geometry(app);
    Ok(())
}

fn create_toast(app: &AppHandle) -> tauri::Result<()> {
    let w = WebviewWindowBuilder::new(app, "toast", WebviewUrl::App("index.html#/toast".into()))
        .title("Wuwa Tappo")
                .initialization_script(CONSOLE_BRIDGE_SCRIPT)
                .initialization_script(COOKIE_SHIM_SCRIPT)
        .decorations(false)
                .accept_first_mouse(true)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .visible(false)
        .build()?;
    w.set_ignore_cursor_events(true)?;
    if let Ok(m) = main_monitor(app) {
        cover_monitor(&w, &m)?;
    }
    Ok(())
}

pub fn run() {
    let builder = tauri::Builder::default();
    // Release only: a debug binary would register its own target/ path as the login item.
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_autostart::init(
        tauri_plugin_autostart::MacosLauncher::LaunchAgent,
        None,
    ));
    builder
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .max_file_size(5_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(3))
                .build(),
        )
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(Shared::default())
        .setup(|app| {
            // Desktop pet: no Dock icon or app menu, tray only (like the Windows tray app).
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let handle = app.handle().clone();
            create_main(&handle)?;
            create_toast(&handle)?;
            let menu = build_menu(&handle, "tray", &MenuState { pet_dock_visible: true, pet_scale: 100, window_opacity: 100, ..Default::default() })?;
            TrayIconBuilder::with_id("main-tray")
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .tooltip("Wuwa Tappo")
                .menu(&menu)
                .build(app)?;
            app.on_menu_event(|app, event| on_menu(app, event.id().as_ref()));
            input::spawn(handle.clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            aux_listener_ready,
            open_about_window,
            close_about_window,
            open_oss_notice_window,
            close_oss_notice_window,
            open_login_window,
            close_login_window,
            resize_login_window_to_content,
            open_update_window,
            close_update_window,
            open_maintenance_notice,
            get_maintenance_notice_payload,
            has_maintenance_notice,
            hide_main_after_maintenance_notice_ready,
            open_panel_window,
            close_panel_window,
            raise_panel_window_above_pets,
            set_panel_window_collapsed,
            show_toast_overlay,
            sync_aux_windows_to_main_monitor,
            apply_main_window_position,
            set_main_window_fullscreen_shell,
            set_main_window_monitor_bounds,
            set_main_window_content_bounds,
            sync_main_window_to_current_monitor,
            finish_main_window_monitor_switch,
            ensure_main_taskbar_icon,
            set_main_skip_taskbar,
            get_app_shortcuts,
            set_app_shortcuts,
            set_shortcut_capture,
            get_main_desktop_safe_area,
            get_main_taskbar_insets,
            set_main_hit_region,
            set_main_swallow_mouse,
            set_main_force_cursor_ignore,
            set_native_input_cursor,
            set_overlay_cursor,
            sync_native_menu,
            popup_main_dock_context_menu,
            load_key_press_count,
            save_key_press_count,
            load_cumulative_click_count,
            save_cumulative_click_count,
            save_app_ui_locale,
            write_app_log,
            get_track_os_info,
            check_app_update,
            get_app_update_detail,
            is_force_update_locked,
            is_elevated_autostart_enabled,
            set_elevated_autostart,
            exit_app,
            is_internal_testing_build,
            is_devtools_hotkeys_build,
            get_boot_diagnostics,
        ])
        .run(tauri::generate_context!())
        .expect("error while running WuwaTappo");
}

#[cfg(test)]
mod tests {
    use super::Rect;

    /// Manual: `cargo test fullscreen_probe -- --ignored --nocapture` with and without a full-screen app in front.
    #[test]
    #[ignore]
    fn fullscreen_probe() {
        println!("foreground_app_fullscreen = {}", super::foreground_app_fullscreen());
    }

    #[test]
    fn update_script_quotes_the_repo_path() {
        let s = super::update_script(std::path::Path::new("/Users/a b/it's/wuwa"));
        assert!(s.contains(r"cd '/Users/a b/it'\''s/wuwa' || exit 1"), "{s}");
        assert!(s.contains("\ntools/build.sh\n"));
        // Same quoting as the shell would need: run it through zsh to be sure.
        let out = std::process::Command::new("/bin/zsh").arg("-c").arg(format!("print -r -- {}", r"'/Users/a b/it'\''s/wuwa'")).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "/Users/a b/it's/wuwa");
    }

    #[test]
    fn fullscreen_means_covering_a_display_exactly() {
        use core_graphics::geometry::{CGPoint, CGRect, CGSize};
        let r = |x, y, w, h| CGRect::new(&CGPoint::new(x, y), &CGSize::new(w, h));
        let main = r(0.0, 0.0, 1512.0, 982.0);
        let external = r(1512.0, -400.0, 3840.0, 2160.0);
        assert!(super::covers(&r(0.0, 0.0, 1512.0, 982.0), &main));
        assert!(super::covers(&r(1512.0, -400.0, 3840.0, 2160.0), &external));
        // A big maximized window on the external display is not full screen anywhere.
        assert!(!super::covers(&r(1512.0, -375.0, 3840.0, 2100.0), &main));
        assert!(!super::covers(&r(1512.0, -375.0, 3840.0, 2100.0), &external));
    }

    #[test]
    fn shortcut_bindings_are_validated() {
        assert!(super::valid_binding(&serde_json::json!({"modifiers": 8, "key": 120})));
        assert!(!super::valid_binding(&serde_json::Value::Null));
        assert!(!super::valid_binding(&serde_json::json!({"modifiers": 99, "key": 120})));
        assert!(!super::valid_binding(&serde_json::json!({"key": 120})));
    }

    #[test]
    fn update_script_runs_and_reports_the_result() {
        // A fake clone (not a git repo, so the pull fails and the build still runs) whose build.sh exits 3.
        let dir = std::env::temp_dir().join(format!("wuwa-update-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join("tools/build.sh"), "#!/bin/sh\necho fake-build\nexit 3\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("tools/build.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = dir.join("update.command");
        std::fs::write(&script, super::update_script(&dir)).unwrap();
        let out = std::process::Command::new("/bin/zsh").arg(&script).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(text.contains("fake-build") && text.contains("Update failed (exit 3)"), "{text}");
    }

    #[test]
    fn dialogs_cover_every_app_language() {
        // The frontend's GLOBAL_LOCALE_OPTIONS.
        for l in ["tw", "jp", "kr", "fr", "de", "es", "pt", "th"] {
            assert_ne!(super::permission_text(l), super::permission_text("en"), "{l}");
            assert_ne!(super::update_text(l), super::update_text("en"), "{l}");
            assert!(super::update_text(l)[1].contains("{latest}") && super::update_text(l)[1].contains("{current}"), "{l}");
            assert_ne!(super::update_now_text(l), super::update_now_text("en"), "{l}");
            assert!(super::update_now_text(l)[0].contains("{latest}") && super::update_now_text(l)[0].contains("{current}"), "{l}");
            assert!(super::update_now_text(l)[0].contains(super::update_now_text(l)[1]), "{l}: body names its button");
        }
        assert_eq!(super::locale_from_system("ko-KR"), "kr");
        assert_eq!(super::locale_from_system("zh-Hant-TW"), "tw");
        assert_eq!(super::locale_from_system("pt-BR"), "pt");
        assert_eq!(super::locale_from_system("it"), "en");
    }

    #[test]
    fn version_comparison() {
        assert!(super::is_newer("1.0.1", "1.0.0"));
        assert!(super::is_newer("1.10.0", "1.9.9"));
        assert!(!super::is_newer("1.0.0", "1.0.0"));
        assert!(!super::is_newer("0.9.0", "1.0.0"));
        assert!(!super::is_newer("garbage", "1.0.0"));
    }

    #[test]
    fn rect_hit_is_half_open() {
        let r = Rect { left: 10.0, top: 10.0, width: 5.0, height: 5.0, member_id: None };
        assert!(r.contains(10.0, 14.9));
        assert!(!r.contains(15.0, 12.0));
    }
}
