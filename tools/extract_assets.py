"""Extract Tauri-embedded (brotli) frontend assets from a Windows x64 PE.

Tauri codegen stores assets as a phf map of (&str key, &[u8] data); each entry is
four little-endian u64s: key_ptr, key_len, data_ptr, data_len (pointers as VAs).
"""
import struct, sys, os, subprocess

exe, out = sys.argv[1], sys.argv[2]
b = open(exe, 'rb').read()
pe = struct.unpack_from('<I', b, 0x3c)[0]
nsec = struct.unpack_from('<H', b, pe + 6)[0]
optsz = struct.unpack_from('<H', b, pe + 20)[0]
base = struct.unpack_from('<Q', b, pe + 24 + 24)[0]
secs = []
for i in range(nsec):
    o = pe + 24 + optsz + 40 * i
    name = b[o:o + 8].rstrip(b'\0').decode()
    vsz, va, rsz, raw = struct.unpack_from('<IIII', b, o + 8)
    secs.append((name, va, max(vsz, rsz), raw, rsz))


def va2off(v):
    rva = v - base
    for _, va, sz, raw, rsz in secs:
        if va <= rva < va + rsz:
            return raw + rva - va
    return None


rdata = next(s for s in secs if s[0] == '.rdata')
start, end = rdata[3], rdata[3] + rdata[4]
found = {}
for o in range(start, end - 32, 8):
    kp, kl, dp, dl = struct.unpack_from('<QQQQ', b, o)
    if not (2 <= kl < 200 and 0 < dl < 50_000_000):
        continue
    ko, do = va2off(kp), va2off(dp)
    if ko is None or do is None or b[ko:ko + 1] != b'/':
        continue
    try:
        key = b[ko:ko + kl].decode()
    except UnicodeDecodeError:
        continue
    if not key.isprintable() or ' ' in key:
        continue
    # Real assets have a file extension; 2-char hits like "/P" are pointer-table false positives.
    # Type declarations (.d.ts) are not needed at runtime.
    if '.' not in key.rsplit('/', 1)[-1] or key.endswith('.d.ts'):
        continue
    found[key] = (do, dl)

ok = bad = 0
for key, (do, dl) in sorted(found.items()):
    dst = os.path.join(out, key.lstrip('/'))
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    r = subprocess.run(['brotli', '-d', '-c'], input=b[do:do + dl], capture_output=True)
    if r.returncode == 0:
        open(dst, 'wb').write(r.stdout); ok += 1
    else:  # not compressed (or not brotli): keep raw bytes
        open(dst, 'wb').write(b[do:do + dl]); bad += 1
print(f'entries={len(found)} brotli_ok={ok} raw={bad}')
