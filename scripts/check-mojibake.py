#!/usr/bin/env python3
"""Detect mojibake (UTF-8 mis-decoded as latin-1) left by non-UTF8 perl edits."""
import re
import glob

# Common mojibake markers: sequences like è­¦ (U+00E8 + combining), å, ï¼
pat = re.compile(
    r'[\u00c0-\u00ff][\u0080-\u00ff]'
    r'|\u00e8[\u00ad-\u00af]'
    r'|\u00e7|\u00e5\u2019|\u00ef\u00bc'
)
files = glob.glob('crates/core/src/*.rs') + glob.glob('crates/daemon/src/*.rs')
found = 0
for f in files:
    for i, line in enumerate(open(f, encoding='utf-8'), 1):
        if pat.search(line):
            print(f'{f}:{i}: {line.strip()[:110]}')
            found += 1
print(f'total: {found}')
