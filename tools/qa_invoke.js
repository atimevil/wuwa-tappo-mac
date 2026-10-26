// Runtime QA: injected into the main window of a debug build (WUWA_QA_SCRIPT=tools/qa_invoke.js).
// Calls every host command with the argument shapes the frontend uses and logs `[QA] PASS|FAIL`.
// Skipped on purpose: exit_app, hide_main_after_maintenance_notice_ready, open_maintenance_notice
// (they quit or hide the pet) and popup_main_dock_context_menu (a modal menu; covered by the UI pass).
(() => {
  if (window.__TAURI_INTERNALS__?.metadata?.currentWindow?.label !== 'main') return;
  const inv = (cmd, args = {}) => window.__TAURI_INTERNALS__.invoke(cmd, args);
  const log = (msg) => inv('write_app_log', { level: 'info', message: '[QA] ' + msg }).catch(() => {});
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const env = 'global-global-prod';
  const isInsets = (v) => v && ['left', 'top', 'right', 'bottom'].every((k) => Number.isFinite(v[k]) && v[k] >= 0);
  let pass = 0, fail = 0;
  const check = async (name, fn, expect = () => true) => {
    try {
      const v = await fn();
      if (expect(v)) { pass++; log(`PASS ${name} -> ${JSON.stringify(v)?.slice(0, 160)}`); }
      else { fail++; log(`FAIL ${name}: unexpected ${JSON.stringify(v)?.slice(0, 300)}`); }
      return v;
    } catch (e) {
      fail++; log(`FAIL ${name}: ${String(e?.message ?? e).slice(0, 300)}`);
    }
  };

  setTimeout(async () => {
    log('start');
    await check('get_track_os_info', () => inv('get_track_os_info'), (v) => v.os === 'macOS' && typeof v.osVersion === 'string' && v.osVersion.length > 0);
    await check('get_main_desktop_safe_area', () => inv('get_main_desktop_safe_area'), (v) => isInsets(v.insets) && v.autoHideEdge === null && v.foregroundFullscreen === false);
    await check('get_main_taskbar_insets', () => inv('get_main_taskbar_insets'), isInsets);
    const before = await check('load_key_press_count', () => inv('load_key_press_count', { environment: env }), Number.isFinite);
    await check('save_key_press_count', () => inv('save_key_press_count', { count: before, environment: env }));
    await check('load_key_press_count (round trip)', () => inv('load_key_press_count', { environment: env }), (v) => v === before);
    await check('load_key_press_count (other env)', () => inv('load_key_press_count', { environment: 'qa-unused-env' }), (v) => v === 0);
    const cum = await check('load_cumulative_click_count', () => inv('load_cumulative_click_count', { environment: env }), (v) => Number.isFinite(v.count) && typeof v.cuid === 'string');
    await check('load_cumulative_click_count (missing env)', () => inv('load_cumulative_click_count', { environment: 'qa-unused-env' }), (v) => v.count === 0 && v.environment === '');
    if (cum && cum.cuid) {
      await check('save_cumulative_click_count', () => inv('save_cumulative_click_count', { count: cum.count, cuid: cum.cuid, environment: env }));
    }
    await check('save_app_ui_locale', () => inv('save_app_ui_locale', { locale: 'kr' }));
    await check('write_app_log', () => inv('write_app_log', { level: 'warn', message: '[QA] warn-level log line' }));
    await check('is_force_update_locked', () => inv('is_force_update_locked'), (v) => v === false);
    await check('check_app_update', () => inv('check_app_update', { endpoint: 'https://example.invalid/latest.json' }), (v) => v === null);
    await check('has_maintenance_notice', () => inv('has_maintenance_notice'), (v) => v === false);
    await check('get_maintenance_notice_payload', () => inv('get_maintenance_notice_payload'), (v) => v === null);
    await check('is_elevated_autostart_enabled', () => inv('is_elevated_autostart_enabled'), (v) => v === false);
    await check('set_elevated_autostart', () => inv('set_elevated_autostart', { enabled: false }));
    await check('is_internal_testing_build', () => inv('is_internal_testing_build'), (v) => v === false);
    await check('is_devtools_hotkeys_build', () => inv('is_devtools_hotkeys_build'), (v) => v === false);
    await check('get_boot_diagnostics', () => inv('get_boot_diagnostics'), (v) => v === null);
    await check('ensure_main_taskbar_icon', () => inv('ensure_main_taskbar_icon'));
    await check('apply_main_window_position', () => inv('apply_main_window_position', { payload: { alwaysOnTop: true, position: 'bottomRight', applyPosition: false } }));
    await check('set_main_window_fullscreen_shell', () => inv('set_main_window_fullscreen_shell'));
    const mon = await check('plugin:window|current_monitor', () => inv('plugin:window|current_monitor', { label: 'main' }), (v) => v && v.size.width > 0);
    if (mon) {
      await check('set_main_window_monitor_bounds', () => inv('set_main_window_monitor_bounds', { payload: { x: mon.position.x, y: mon.position.y, width: mon.size.width, height: mon.size.height } }));
      await sleep(300);
      await check('main covers monitor', () => inv('plugin:window|outer_size', { label: 'main' }), (v) => v.width === mon.size.width && v.height === mon.size.height);
    }
    await check('sync_main_window_to_current_monitor', () => inv('sync_main_window_to_current_monitor'));
    await check('finish_main_window_monitor_switch', () => inv('finish_main_window_monitor_switch'));
    await check('sync_aux_windows_to_main_monitor', () => inv('sync_aux_windows_to_main_monitor'));
    await check('set_main_window_content_bounds', () => inv('set_main_window_content_bounds', { payload: { width: 200, height: 200, skipPassthroughRefresh: true } }));
    await check('set_main_hit_region', () => inv('set_main_hit_region', { payload: { regions: [{ left: 1, top: 1, width: 2, height: 2 }], inputRegions: [], updateNativeInput: false, windowPassthrough: false, nativeInput: true } }));
    await check('set_main_swallow_mouse', () => inv('set_main_swallow_mouse', { enabled: false }));
    await check('set_main_force_cursor_ignore', () => inv('set_main_force_cursor_ignore', { enabled: true }));
    await check('set_native_input_cursor', () => inv('set_native_input_cursor', { kind: 'pointer' }));
    await check('set_overlay_cursor', () => inv('set_overlay_cursor', { kind: 'default' }));
    await check('sync_native_menu', () => inv('sync_native_menu', { labels: { openBag: 'Bag', quit: 'Quit', trayTooltip: 'Wuwa Tappo' }, petDockVisible: true, petScale: 100, windowOpacity: 100, windowPassthrough: false }));
    await check('show_toast_overlay', () => inv('show_toast_overlay'));
    await check('toast visible', () => inv('plugin:window|is_visible', { label: 'toast' }), (v) => v === true);

    const auxCycle = async (label, open, close, extra) => {
      await check(`${open}`, () => inv(open.split(' ')[0], extra?.openArgs ?? {}));
      await sleep(1500);
      await check(`${label} visible`, () => inv('plugin:window|is_visible', { label }), (v) => v === true);
      if (extra?.during) await extra.during();
      await check(`${close}`, () => inv(close, extra?.closeArgs ?? {}));
      await sleep(400);
      await check(`${label} hidden`, () => inv('plugin:window|is_visible', { label }), (v) => v === false);
    };
    await auxCycle('about', 'open_about_window', 'close_about_window');
    await auxCycle('oss-notice', 'open_oss_notice_window', 'close_oss_notice_window');
    await auxCycle('update', 'open_update_window', 'close_update_window', {
      openArgs: { detail: { newVersion: '9.9.9', body: 'qa' } },
      during: () => check('get_app_update_detail', () => inv('get_app_update_detail'), (v) => v?.newVersion === '9.9.9'),
    });
    await auxCycle('panel', 'open_panel_window', 'close_panel_window', {
      openArgs: { payload: { tab: 'setting' } },
      during: async () => {
        await check('set_panel_window_collapsed true', () => inv('set_panel_window_collapsed', { payload: { collapsed: true } }));
        await check('set_panel_window_collapsed false', () => inv('set_panel_window_collapsed', { payload: { collapsed: false } }));
        await check('raise_panel_window_above_pets', () => inv('raise_panel_window_above_pets'));
        await check('open_panel_window again (tab event)', () => inv('open_panel_window', { payload: { tab: 'bag' } }));
      },
    });
    await auxCycle('login', 'open_login_window', 'close_login_window', {
      openArgs: { intent: 'show', compact: false },
      during: () => check('resize_login_window_to_content', () => inv('resize_login_window_to_content', { payload: { width: 500, height: 378 } })),
    });
    log(`done pass=${pass} fail=${fail}`);
  }, 9000);
})();
