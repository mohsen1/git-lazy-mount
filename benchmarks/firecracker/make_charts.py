#!/usr/bin/env python3
"""Render two grouped bar charts (disk + time) as dependency-free SVG from a
metrics JSON list. Each entry: {repo, files, clone_mb, lazy_mb, clone_task_s, lazy_task_s}."""
import json, sys, html

def bars(data, key_a, key_b, label_a, label_b, unit, title, fname, color_a="#c0504d", color_b="#4f81bd"):
    rows = sorted(data, key=lambda d: -d.get(key_a, 0))
    W, rowh, pad, labelw, maxw = 900, 26, 8, 150, 560
    H = 70 + len(rows) * rowh
    vmax = max(max(d.get(key_a, 0), d.get(key_b, 0)) for d in rows) or 1
    def x(v): return labelw + (v / vmax) * maxw
    s = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" font-family="-apple-system,Segoe UI,Helvetica,Arial,sans-serif" font-size="12">']
    s.append(f'<text x="{labelw}" y="22" font-size="15" font-weight="600">{html.escape(title)}</text>')
    s.append(f'<rect x="{labelw}" y="34" width="11" height="11" fill="{color_a}"/><text x="{labelw+16}" y="44">{html.escape(label_a)}</text>')
    s.append(f'<rect x="{labelw+200}" y="34" width="11" height="11" fill="{color_b}"/><text x="{labelw+216}" y="44">{html.escape(label_b)}</text>')
    y = 58
    for d in rows:
        a, b = d.get(key_a, 0), d.get(key_b, 0)
        s.append(f'<text x="{labelw-6}" y="{y+rowh/2-2}" text-anchor="end">{html.escape(d["repo"])}</text>')
        bh = (rowh - pad) / 2
        s.append(f'<rect x="{labelw}" y="{y}" width="{x(a)-labelw:.1f}" height="{bh:.1f}" fill="{color_a}"/>')
        s.append(f'<text x="{x(a)+4:.1f}" y="{y+bh-1:.1f}" fill="#555">{a:g}{unit}</text>')
        s.append(f'<rect x="{labelw}" y="{y+bh:.1f}" width="{x(b)-labelw:.1f}" height="{bh:.1f}" fill="{color_b}"/>')
        s.append(f'<text x="{x(b)+4:.1f}" y="{y+2*bh-1:.1f}" fill="#555">{b:g}{unit}</text>')
        y += rowh
    s.append('</svg>')
    open(fname, "w").write("\n".join(s))
    print(f"wrote {fname} ({len(rows)} repos)")

if __name__ == "__main__":
    data = json.load(open(sys.argv[1]))
    out = sys.argv[2] if len(sys.argv) > 2 else "."
    bars(data, "clone_mb", "lazy_mb", "git clone (full, with history)", "git lazy-mount", " MB",
         "Disk to a working copy (full git clone vs lazy-mount) — lower is better", f"{out}/disk.svg")
    bars(data, "clone_task_s", "lazy_task_s", "git clone --depth 1", "git lazy-mount", " s",
         "Time to a ready working copy (vs even a shallow clone) — lower is better", f"{out}/time.svg")
