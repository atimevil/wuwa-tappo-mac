"""Static QA: compare every command the frontend invokes with what the macOS host provides.

Usage: python3 tools/check_contract.py   (from the repo root)

Checks:
1. Every app command invoked by the JS exists in `generate_handler!` (else "command not found").
2. Argument keys sent by each call site match the Rust parameter names (Tauri expects camelCase).
3. Every `plugin:<p>|<cmd>` the JS can invoke is covered by a capability permission.
"""
import glob, json, os, re, sys

WEB = 'web/assets'
LIB = 'src-tauri/src/lib.rs'
CAPS = 'src-tauri/capabilities/default.json'

js = {os.path.basename(p): open(p, encoding='utf-8').read() for p in glob.glob(f'{WEB}/*.js')}
main_name = next(n for n, s in js.items() if 'async function invoke$1(' in s)
main = js[main_name]

# Names under which each module can call invoke$1.
export_list = re.findall(r'export\{([^}]*)\}', main)[-1]
exported_as = {m.group(2): m.group(1) for m in re.finditer(r'([\w$]+) as ([\w$]+)', export_list)}
aliases = {main_name: {'invoke$1'}}
inv_exports = {alias for alias, local in exported_as.items() if local == 'invoke$1'}
for name, src in js.items():
    if name == main_name:
        continue
    for imp in re.finditer(r'import\{([^}]*)\}from"\./' + re.escape(main_name) + '"', src):
        for m in re.finditer(r'([\w$]+) as ([\w$]+)', imp.group(1)):
            if m.group(1) in inv_exports:
                aliases.setdefault(name, set()).add(m.group(2))
# Modules re-wrapping invoke (e.g. `async function _(e,t){return n(e,t)}`) are found by following one level.
for name, src in js.items():
    for a in list(aliases.get(name, ())):
        for m in re.finditer(r'(?:async )?function ([\w$]+)\((\w),(\w)\)\{return (?:await )?' + re.escape(a) + r'\(\2,\3\)\}', src):
            aliases[name].add(m.group(1))
        for m in re.finditer(r'const ([\w$]+)=\((\w),(\w)\)=>' + re.escape(a) + r'\(\2,\3\)', src):
            aliases[name].add(m.group(1))


def top_keys(obj):
    if not obj:
        return []
    depth, cur, out = 0, '', []
    for ch in obj[1:-1]:
        if ch in '{[(':
            depth += 1
        elif ch in '}])':
            depth -= 1
        if ch == ',' and depth == 0:
            out.append(cur); cur = ''
        else:
            cur += ch
    if cur.strip():
        out.append(cur)
    return sorted({t.split(':')[0].strip() for t in out if t.strip()})


calls = {}
plugin_cmds = set()
for name, src in js.items():
    for a in aliases.get(name, ()):
        for m in re.finditer(r'(?<![\w$.])' + re.escape(a) + r'\(\s*("([^"]+)"|([\w$]+))\s*(,\s*\{)?', src):
            cmd = m.group(2)
            if cmd is None:  # variable command name: resolve `const d="..."` nearby
                v = re.search(r'const ' + re.escape(m.group(3)) + r'="([^"]+)"', src[max(0, m.start() - 300):m.start()])
                if not v:
                    continue
                cmd = v.group(1)
            if cmd.startswith('plugin:'):
                plugin_cmds.add(cmd)
                continue
            obj = None
            if m.group(4):  # balanced-brace scan: args may contain calls like getNativeMenuLabels()
                start = m.end() - 1; depth = 0
                for k in range(start, len(src)):
                    depth += src[k] == '{'; depth -= src[k] == '}'
                    if depth == 0:
                        obj = src[start:k + 1]; break
            calls.setdefault(cmd, set()).add(tuple(top_keys(obj)))
    plugin_cmds |= set(re.findall(r'"(plugin:[a-z-]+\|[a-z_]+)"', src))

lib = open(LIB).read()
handlers = set(re.findall(r'^\s+([a-z_]+),$', lib.split('generate_handler![', 1)[1].split(']', 1)[0], re.M))


def camel(n):
    head, *rest = n.split('_')
    return head + ''.join(w.capitalize() for w in rest)


params = {}
for m in re.finditer(r'#\[tauri::command\]\s*(?:async )?fn ([a-z_]+)\(([^)]*)\)', lib):
    ps = []
    for p in m.group(2).split(','):
        p = p.strip()
        if not p or ':' not in p:
            continue
        pname, ptype = [x.strip() for x in p.split(':', 1)]
        if ptype.startswith(('AppHandle', 'tauri::Window', 'tauri::State', 'WebviewWindow', 'tauri::AppHandle')):
            continue
        ps.append((camel(pname), ptype.startswith('Option<')))
    params[m.group(1)] = ps

problems = []
for cmd, keysets in sorted(calls.items()):
    if cmd not in handlers:
        problems.append(f'MISSING command: {cmd} (JS sends {sorted(keysets)})')
        continue
    want = params.get(cmd, [])
    required = {n for n, opt in want if not opt}
    allowed = {n for n, _ in want}
    for keys in keysets:
        ks = set(keys)
        if not required <= ks:
            problems.append(f'ARGS {cmd}: JS sends {sorted(ks)} but Rust requires {sorted(required)}')
        extra = ks - allowed
        if extra:
            problems.append(f'ARGS {cmd}: JS sends unused {sorted(extra)} (Rust takes {sorted(allowed)})')
unused = sorted(handlers - set(calls) - {'aux_listener_ready'})

caps = json.load(open(CAPS))['permissions']
cap_ids = {c if isinstance(c, str) else c['identifier'] for c in caps}
core_defaults = {'window', 'webview', 'event', 'app', 'image', 'menu', 'tray', 'path', 'resources'}
uncovered = []
for pc in sorted(plugin_cmds):
    plugin, cmd = pc[len('plugin:'):].split('|')
    perm = cmd.replace('_', '-')
    if plugin in core_defaults:
        ok = f'core:{plugin}:allow-{perm}' in cap_ids or f'core:{plugin}:default' in cap_ids and plugin not in ('window', 'webview')
        # core:window:default / core:webview:default only cover getters; setters need explicit allow-*.
        if plugin in ('window', 'webview') and not ok:
            getter = cmd.startswith(('is_', 'inner_', 'outer_', 'scale_', 'current_', 'primary_', 'available_', 'cursor_', 'monitor_', 'title', 'theme', 'get_', 'webview_position', 'webview_size'))
            ok = getter and f'core:{plugin}:default' in cap_ids
    else:
        ok = f'{plugin}:allow-{perm}' in cap_ids or f'{plugin}:default' in cap_ids
    if not ok:
        uncovered.append(pc)

print(f'app commands called by JS: {len(calls)}; registered: {len(handlers)}')
print(f'plugin commands referenced by JS: {len(plugin_cmds)}')
for p in problems:
    print(p)
for u in uncovered:
    print(f'PERMISSION? {u}')
if unused:
    print(f'registered but never called from JS: {unused}')
if problems:
    sys.exit(1)
print('contract OK' if not uncovered else 'contract OK (review PERMISSION? lines: plugin defaults may not include them)')
