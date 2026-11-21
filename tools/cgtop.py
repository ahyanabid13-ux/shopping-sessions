#!/usr/bin/env python3
"""Ranks source lines of a cg_annotate output by instruction count (Ir).
Usage: cgtop.py ANNOTATE_TXT [N]"""
import re, sys
text = open(sys.argv[1]).read().split("\n")
n = int(sys.argv[2]) if len(sys.argv) > 2 else 40
cur = "?"
rows = []
for line in text:
    m = re.match(r"-- (?:Annotated source file|Auto-annotated source): (.*)", line)
    if m:
        cur = m.group(1).split("/")[-1]
        continue
    m = re.match(r"^\s*([0-9,]+) \([ 0-9.]+%\)(.*)$", line)
    if m and cur != "?":
        ir = int(m.group(1).replace(",", ""))
        rest = m.group(2)
        # source text follows the last column group: take the text after the final '%)' or numbers
        src = re.split(r"\)\s{2,}|\s{3,}", rest)[-1]
        rows.append((ir, cur, src.strip()[:110]))
rows.sort(reverse=True)
total = sum(r[0] for r in rows)
for ir, f, src in rows[:n]:
    print(f"{ir/1e6:8.1f}M  {f:<12} {src}")
