"""Pull the inner NSIS installer out of the official WuwaTappo setup .exe.

Usage: python3 tools/extract_payload.py WuwaTappo-global_<ver>_x64-setup.exe out/inner.exe [out/version.txt]

The setup is a Tauri "installer shell" that appends the real NSIS installer after an
`ENDWUWAPAYLOAD` marker; `7zz x` can then unpack `WuwaTappo.exe` from that installer.
"""
import sys

src, dst = sys.argv[1], sys.argv[2]
data = open(src, 'rb').read()
marker = b'ENDWUWAPAYLOAD'
i = data.find(marker)
if i < 0:
    sys.exit('payload marker not found: is this the official WuwaTappo setup .exe?')
payload = data[i + len(marker):]
if payload[:2] != b'MZ':
    sys.exit('payload after the marker is not a Windows executable')
open(dst, 'wb').write(payload)
print(f'inner installer: {len(payload)} bytes -> {dst}')

# The shell also carries `WUWAPAYLOAD:{json}:` with the installer's version; build.sh stamps it on the app.
import json, re
meta = re.search(rb'WUWAPAYLOAD:(\{.*?\}):ENDWUWAPAYLOAD', data[max(0, i - 4096):i + len(marker)])
if meta and len(sys.argv) > 3:
    version = json.loads(meta.group(1)).get('version', '')
    if re.fullmatch(r'\d+\.\d+\.\d+', version):
        open(sys.argv[3], 'w').write(version)
        print(f'installer version: {version}')
