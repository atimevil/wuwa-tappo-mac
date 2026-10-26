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
  const send = (level, args) => {
    try {
      window.__TAURI_INTERNALS__.invoke('write_app_log', {
        level, message: '[console:' + location.hash + '] ' + args.map(fmt).join(' ').slice(0, 2000),
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
    update_detail: Mutex<Option<Value>>,
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
}

impl MenuState {
    fn label(&self, key: &str, fallback: &str) -> String {
        self.labels.get(key).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| fallback.into())
    }
}

/// Called from the input thread for every pointer event: converts to the frontend's physical
/// coordinates, toggles click-through, and finds the pet member under the cursor.
pub(crate) fn on_pointer(app: &AppHandle, px: f64, py: f64) -> (f64, f64, Option<Value>) {
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
    (px * scale, py * scale, member)
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

#[tauri::command]
fn open_update_window(app: AppHandle, detail: Option<Value>) -> Result<(), String> {
    *app.state::<Shared>().update_detail.lock().unwrap() = detail.clone();
    let aux = Aux { label: "update", route: "index.html#/update".into(), size: (960.0, 720.0) };
    open_aux(&app, aux, Some(("update-modal-open", detail.unwrap_or(Value::Null)))).map(|_| ())
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
    Ok(json!({ "insets": insets(&app)?, "autoHideEdge": null, "foregroundFullscreen": false }))
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

// ---------- misc ----------

const INPUT_MONITORING_PANE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";

/// Called from the input thread when Input Monitoring is still missing after the system request.
pub(crate) fn explain_input_monitoring(app: &AppHandle) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let korean = read_store(app)["locale"].as_str().map_or(true, |l| l == "kr");
    let (title, body, open, later) = if korean {
        (
            "입력 모니터링 권한이 필요합니다",
            "Wuwa Tappo가 키 입력과 클릭을 세려면 입력 모니터링 권한이 필요합니다.\n\n\
             '설정 열기'를 누른 뒤 목록에서 Wuwa Tappo를 켜 주세요. 목록에 없으면 아래 '+'를 눌러 \
             응용 프로그램 폴더의 Wuwa Tappo를 추가하면 됩니다. 이미 켜져 있는데도 이 창이 뜨면 \
             이전 빌드의 항목이니 '−'로 지우고 다시 추가하세요. 켜면 앱이 다시 시작됩니다.",
            "설정 열기",
            "나중에",
        )
    } else {
        (
            "Input Monitoring permission needed",
            "Wuwa Tappo needs Input Monitoring to count key presses and clicks.\n\n\
             Click 'Open Settings' and turn on Wuwa Tappo. If it is not listed, click '+' and add \
             Wuwa Tappo from the Applications folder. If it is already on and you still see this, \
             that entry belongs to an earlier build: remove it with '-' and add it again. \
             The app restarts after you turn it on.",
            "Open Settings",
            "Later",
        )
    };
    // A Dock-less (accessory) app is never frontmost, so its alert would open behind other windows.
    let _ = app.run_on_main_thread(|| {
        if let Some(mtm) = objc2::MainThreadMarker::new() {
            #[allow(deprecated)]
            objc2_app_kit::NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        }
    });
    let open_settings = app
        .dialog()
        .message(body)
        .title(title)
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(open.into(), later.into()))
        .blocking_show();
    if open_settings {
        let _ = std::process::Command::new("open").arg(INPUT_MONITORING_PANE).status();
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

/// Self-update is Windows-only (the published update feed ships Windows installers).
#[tauri::command]
fn check_app_update(endpoint: Option<String>) -> Option<Value> {
    let _ = endpoint;
    None
}

#[tauri::command]
fn get_app_update_detail(app: AppHandle) -> Option<Value> {
    app.state::<Shared>().update_detail.lock().unwrap().clone()
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
            input::spawn(handle);
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

    #[test]
    fn rect_hit_is_half_open() {
        let r = Rect { left: 10.0, top: 10.0, width: 5.0, height: 5.0, member_id: None };
        assert!(r.contains(10.0, 14.9));
        assert!(!r.contains(15.0, 12.0));
    }
}
