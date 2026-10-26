"""Pull the inner NSIS installer out of the official WuwaTappo setup .exe.

Usage: python3 tools/extract_payload.py WuwaTappo-global_<ver>_x64-setup.exe out/inner.exe

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
