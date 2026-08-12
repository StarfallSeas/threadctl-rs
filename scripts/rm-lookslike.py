#!/usr/bin/env python3
s = open('crates/core/src/config.rs', encoding='utf-8').read()
old = '''fn looks_like_cpu_range(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == ',' || c == ' ' || c == '\t')
        && s.chars().any(|c| c.is_ascii_digit())
}

'''
assert old in s, "looks_like_cpu_range 未命中"
s = s.replace(old, '')
open('crates/core/src/config.rs', 'w', encoding='utf-8').write(s)
print('removed')
