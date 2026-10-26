"""Run the document.cookie shim (src-tauri/src/lib.rs) against the frontend's own setCookie/getCookie in node.

Usage: python3 tools/check_cookie_shim.py   (from the repo root)
"""
import re, subprocess, tempfile

src = open('src-tauri/src/lib.rs').read()
shim = re.search(r'const COOKIE_SHIM_SCRIPT: &str = r#"(.*?)"#;', src, re.S).group(1)
app = next(open(p).read() for p in __import__('glob').glob('web/assets/index-*.js')
           if 'setCookie=(e,t,n)=>' in open(p).read())


def grab(start):
    i = app.find(start); j = app.find('{', i); depth = 0
    for k in range(j, len(app)):
        depth += app[k] == '{'; depth -= app[k] == '}'
        if depth == 0:
            return app[i:k + 1]


test = f'''
const store = {{}};
globalThis.localStorage = {{ getItem: k => k in store ? store[k] : null, setItem: (k, v) => {{ store[k] = String(v); }} }};
globalThis.document = {{}};
{shim}
const {grab('getCookie=e=>')};
const {grab('setCookie=(e,t,n)=>')};
const assert = (await import('node:assert')).default;
setCookie('infoToken', 'abc.def=x', 30); setCookie('lastLoginTime', '1793000000', 30);
assert.equal(getCookie('infoToken'), 'abc.def=x');
setCookie('infoToken', '', -1); assert.ok(!getCookie('infoToken'));
assert.equal(getCookie('lastLoginTime'), '1793000000');
document.cookie = 'mx=2; max-age=0'; assert.ok(!document.cookie.includes('mx='));
console.log('cookie shim OK');
'''
with tempfile.NamedTemporaryFile('w', suffix='.mjs', delete=False) as f:
    f.write(test)
subprocess.run(['node', f.name], check=True)
