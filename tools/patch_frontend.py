"""Apply the macOS host's frontend patches to an extracted web/ tree (idempotent).

Usage: python3 tools/patch_frontend.py web

Anchors are regular expressions that tolerate renamed minified identifiers, so a new Kuro build
usually still patches cleanly. A required patch that finds nothing stops the build.

1. listen: report `aux_listener_ready` once an event listener is registered, so the host can hold
   window-open events (e.g. `login-modal-open`) until the page can receive them. Tauri defines
   `__TAURI_INTERNALS__.invoke` as non-writable, so this cannot be done from an init script.
2. The overseas login page opens in Safari instead of the default browser: in Firefox the Kuro H5
   SDK iframe fails to initialize (page error 20003). The capability allows only this URL with Safari.
3. Protocol-relative Kuro SDK URLs (`//pro-cdn-sdk...`) become https: under `tauri://localhost` they
   resolve to `tauri://pro-cdn-sdk...`, the host answers with HTML and the login view logs "Script error.".
4. activityInfo (which supplies assets_host for CDN images and the remote dictionary) also runs while the
   startup network probe is still "reconnecting". It only runs at boot and after login, and the probe is
   always mid-flight at boot, so before login the dictionary retried forever and atlas images stayed empty.
   If the network really is down, the request fails and is logged as before.
"""
import glob, os, re, sys

PATCHES = [
    {
        'name': 'listen reports aux_listener_ready',
        'find': r'(invoke(?:\$\d+)?)\("plugin:event\|listen",\{event:(\w+),target:(\w+),handler:'
                r'(transformCallback(?:\$\d+)?)\((\w+)\)\}\)\.then\((\w+)=>async\(\)=>(_unlisten(?:\$\d+)?)\(\2,\6\)\)',
        'replace': r'\1("plugin:event|listen",{event:\2,target:\3,handler:\4(\5)}).then(\6=>'
                   r'(\1("aux_listener_ready",{event:\2}).catch(()=>{}),async()=>\7(\2,\6)))',
        'done': r'"aux_listener_ready",\{event:\w+\}',
        'required': True,
    },
    {
        'name': 'login page opens in Safari',
        'find': r'rememberPendingState\((\w+)\),await openUrl\((\w+)\)',
        'replace': r'rememberPendingState(\1),await openUrl(\2,"Safari")',
        'done': r'await openUrl\(\w+,"Safari"\)',
        'required': True,
    },
    {
        'name': 'Kuro SDK URLs use https',
        'find': r'="//pro-cdn-sdk\.kurogame\.com/',
        'replace': r'="https://pro-cdn-sdk.kurogame.com/',
        'done': r'="https://pro-cdn-sdk\.kurogame\.com/',
        'required': False,
    },
    {
        'name': 'activityInfo runs while the startup probe is reconnecting',
        'find': r'if\(([\w$]+)\.value\.status==="online"\|\|\1\.value\.status==="unknown"\)try\{',
        'replace': r'if(\1.value.status==="online"||\1.value.status==="unknown"||\1.value.status==="reconnecting")try{',
        'done': r'\.value\.status==="unknown"\|\|[\w$]+\.value\.status==="reconnecting"\)try\{',
        'required': True,
    },
]

web = sys.argv[1]
bundles = sorted(glob.glob(os.path.join(web, 'assets', '*.js')))
sources = {p: open(p, encoding='utf-8').read() for p in bundles}
failed = []
for patch in PATCHES:
    applied = already = 0
    for path, src in sources.items():
        new, n = re.subn(patch['find'], patch['replace'], src)
        if n:
            sources[path] = new
            applied += n
        elif re.search(patch['done'], src):
            already += 1
    if applied:
        print(f'patched ({applied}x): {patch["name"]}')
    elif already:
        print(f'already patched: {patch["name"]}')
    elif patch['required']:
        failed.append(patch['name'])
        print(f'NOT FOUND: {patch["name"]}')
    else:
        print(f'skipped (not present in this build): {patch["name"]}')
for path, src in sources.items():
    if open(path, encoding='utf-8').read() != src:
        open(path, 'w', encoding='utf-8').write(src)
if failed:
    sys.exit('frontend changed; update the anchors in tools/patch_frontend.py for: ' + ', '.join(failed))
