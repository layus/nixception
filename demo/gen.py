#!/usr/bin/env python3
# Emit a C file with a huge *designated* aggregate initializer:
#
#   const struct row slow_table[N] = { [0]={...}, [1]={...}, ... };
#
# GCC's handling of `[i]={...}` element-designators in a large brace-enclosed
# initializer is super-linear — each designator is resolved against the growing
# constructor — so a few hundred thousand rows turn one translation unit into a
# multi-second compile. That is exactly the case where offloading the compile to
# nixception (built once, then served from the Nix store) pays off.
#
# slow_table.c in this directory is the checked-in output of this script at
# the default row count — it is NOT regenerated at build time (see the
# Makefile). Run this script again only to change the row count:
#
#   python3 gen.py 550000 > slow_table.c
#
# Usage: gen.py [ROWS] > slow_table.c      (default ROWS tuned for ~5s at -O2)

import sys

rows = int(sys.argv[1]) if len(sys.argv) > 1 else 550000

out = sys.stdout
out.write("#include <stddef.h>\n")
out.write("struct row { long a, b, c, d; };\n")
out.write(f"const struct row slow_table[{rows}] = {{\n")
for i in range(rows):
    a = (i * 2654435761) & 0xFFFFFFFF
    b = (i * 40503 + 12345) & 0xFFFFFFFF
    c = (i ^ 0x5BD1E995) & 0xFFFFFFFF
    d = (i * 2246822519) & 0xFFFFFFFF
    out.write(f"[{i}]={{.a={a}L,.b={b}L,.c={c}L,.d={d}L}},\n")
out.write("};\n")
out.write(f"size_t slow_table_rows(void){{return {rows};}}\n")
