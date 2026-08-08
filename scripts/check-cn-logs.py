#!/usr/bin/env python3
"""Find remaining user-visible Chinese log messages (open-source check)."""
import re
import glob

pat = re.compile(r'[\u4e00-\u9fa5]')
files = glob.glob('crates/core/src/*.rs') + glob.glob('crates/daemon/src/*.rs')
found = 0
for f in files:
    for i, line in enumerate(open(f, encoding='utf-8'), 1):
        s = line.strip()
        if (s.startswith('eprintln') or s.startswith('println')) and pat.search(line):
            print(f'{f}:{i}: {line.strip()[:110]}')
            found += 1
print(f'total: {found}')
