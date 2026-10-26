"""Apply the macOS host's frontend patches to an extracted web/ tree (idempotent).

Usage: python3 tools/patch_frontend.py web

1. listen$1 reports `aux_listener_ready` once an event listener is registered, so the host can hold
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
import glob, os, sys

PATCHES = [
    ('.then(r=>async()=>_unlisten$1(e,r))}async function once',
     '.then(r=>(invoke$1("aux_listener_ready",{event:e}).catch(()=>{}),'
     'async()=>_unlisten$1(e,r)))}async function once'),
    ('rememberPendingState(t),await openUrl(i),{state:t}',
     'rememberPendingState(t),await openUrl(i,"Safari"),{state:t}'),
    ('isdkSrc="//pro-cdn-sdk.kurogame.com/', 'isdkSrc="https://pro-cdn-sdk.kurogame.com/'),
    ('isdkCss="//pro-cdn-sdk.kurogame.com/', 'isdkCss="https://pro-cdn-sdk.kurogame.com/'),
    ('if(nt.value.status==="online"||nt.value.status==="unknown")try{const l=gt(await um())',
     'if(nt.value.status==="online"||nt.value.status==="unknown"||nt.value.status==="reconnecting")try{const l=gt(await um())'),
]

web = sys.argv[1]
bundles = glob.glob(os.path.join(web, 'assets', 'index-*.js'))
for orig, new in PATCHES:
    hits = 0
    for path in bundles:
        s = open(path, encoding='utf-8').read()
        if new in s:
            print(f'already patched: {path}'); hits += 1
        elif s.count(orig) == 1:
            open(path, 'w', encoding='utf-8').write(s.replace(orig, new))
            print(f'patched: {path}'); hits += 1
    if hits != 1:
        sys.exit(f'expected exactly one bundle containing {orig[:40]!r}, found {hits}; inspect the new build by hand')
