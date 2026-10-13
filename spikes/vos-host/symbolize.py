#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Aggregate a vos-host profile by function.
usage: symbolize.py <runtime.elf> <profile.tsv> <resolver.log>
resolver.log: output of VOS's resolve_observed_standard_program_pcs
(`observed_pc pc=.. mapped_pc=.. riscv=0x..`)."""
import struct, sys, re, bisect, collections
elf = open(sys.argv[1], 'rb').read()
shoff, = struct.unpack_from('<Q', elf, 40)
shentsize, shnum, shstrndx = struct.unpack_from('<HHH', elf, 58)
secs = [struct.unpack_from('<IIQQQQIIQQ', elf, shoff + i * shentsize) for i in range(shnum)]
syms = []
for s in secs:
    if s[1] == 2:  # SHT_SYMTAB
        strtab = secs[s[6]]
        for off in range(s[4], s[4] + s[5], 24):
            name, info, other, shndx, value, size = struct.unpack_from('<IBBHQQ', elf, off)
            if info & 0xf == 2 and value:  # STT_FUNC
                n = elf[strtab[4] + name: elf.index(b'\0', strtab[4] + name)].decode()
                syms.append((value, size, n))
syms.sort()
addrs = [s[0] for s in syms]
def sym(a):
    i = bisect.bisect_right(addrs, a) - 1
    if i < 0: return '?'
    v, sz, n = syms[i]
    return n if a < v + max(sz, 1) else '?+' + n
pcmap = {}
for line in open(sys.argv[3]):
    m = re.search(r'observed_pc pc=(\d+) mapped_pc=\d+ riscv=(0x[0-9a-f]+)', line)
    if m: pcmap[int(m.group(1))] = int(m.group(2), 16)
by = collections.Counter(); total = 0; unresolved = 0
for line in open(sys.argv[2]):
    pc, g = map(int, line.split())
    total += g
    if pc in pcmap: by[sym(pcmap[pc])] += g
    else: unresolved += g
def demangle(n):
    # legacy Rust mangling: _ZN...E -> path
    if n.startswith('_ZN'):
        out, i = [], 3
        while n[i].isdigit():
            j = i
            while n[j].isdigit(): j += 1
            l = int(n[i:j]); out.append(n[j:j+l]); i = j + l
        if out and re.fullmatch(r'h[0-9a-f]{16}', out[-1]): out.pop()
        return '::'.join(out).replace('$LT$','<').replace('$GT$','>').replace('$u20$',' ').replace('..','::')
    return n
print(f'total {total}  unresolved {unresolved}')
for n, g in by.most_common(40):
    print(f'{g:>14,} {100*g/total:5.1f}%  {demangle(n)[:150]}')
