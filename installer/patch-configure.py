#!/usr/bin/env python3
"""Patches ffmpeg configure for a Chinese-locale MSVC (BuildTools zh-CN):
VSLANG=1033 is ignored by this cl.exe, so the banner never starts with
"Microsoft" and the msvc toolchain probe falls through to the gcc defaults
(`-c -o` breaks object naming, mslink never gets -out:). We match the
compiler/linker banners anywhere on the line instead of at line start.
Idempotent: safe to run on an already-patched configure.
"""
import io, sys

path = sys.argv[1]
s = io.open(path, encoding="utf-8", newline="").read()
lines = s.split("\n")
PAT = "'Microsoft \\(R\\) C/C\\+\\+|^Microsoft \\(R\\)'"
changed = 0
for i, line in enumerate(lines):
    if 'VSLANG=1033' not in line:
        continue
    if 'grep -q ^Microsoft' in line and '$_cc' in line:
        lines[i] = line.replace('grep -q ^Microsoft', 'grep -qE ' + PAT)
        changed += 1
    elif 'grep ^Microsoft' in line and '$_cc' in line:
        lines[i] = line.replace('grep ^Microsoft', 'grep -E ' + PAT)
        changed += 1
    elif '$ar' in line and 'grep -q ^Microsoft' in line:
        lines[i] = line.replace('grep -q ^Microsoft', 'grep -q "Microsoft (R)"')
        changed += 1
if changed == 0:
    print("configure already patched (or upstream fixed the locale probe)")
else:
    io.open(path, "w", encoding="utf-8", newline="").write("\n".join(lines))
    print(f"patched {changed} probe line(s)")
