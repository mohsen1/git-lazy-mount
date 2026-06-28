#!/usr/bin/env python3
"""Option 4 - LINEAR grouped bars done right via a BROKEN (split) x-axis.

The disk numbers span ~1000x (4 MB .. 4160 MB) and time ~130x (1 s .. 131 s).
On a single linear scale the small "lazy-mount" bars vanish.  Instead of a log
axis (which distorts how *big* the saving feels), we keep a LINEAR feel by
splitting the x-axis into two linear panels joined by a break marker:

  * a wide, fine-grained DETAIL panel for the small range (where every
    lazy-mount bar and the small clones live), and
  * a compressed HIGH panel for the long clone bars.

Both panels are linear; only the *scale* changes at the break, which is clearly
marked.  Small values get real width and bold labels; large bars stay long
enough to read "this is the expensive one".  Pure stdlib, hand-written SVG.
"""

import json
import os
import sys
import xml.dom.minidom

DATA = sys.argv[1] if len(sys.argv) > 1 else "chartdata.json"
OUTDIR = sys.argv[2] if len(sys.argv) > 2 else "."

CLONE_COLOR = "#e4572e"   # shallow clone baseline
LAZY_COLOR = "#2a9d8f"    # git lazy-mount (the win)
AXIS = "#54606b"
GRID = "#e7ebef"
INK = "#1d2329"
MUTE = "#6b7782"
FONT = ("-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,"
        "Arial,sans-serif")


def esc(s):
    return (str(s).replace("&", "&amp;").replace("<", "&lt;")
            .replace(">", "&gt;").replace('"', "&quot;"))


def fmt(v, unit):
    if unit == "MB":
        return f"{int(round(v))} MB"
    # seconds: one decimal, but drop a trailing .0
    if abs(v - round(v)) < 0.05:
        return f"{int(round(v))} s"
    return f"{v:.1f} s"


def chart(rows, clone_key, lazy_key, title, subtitle, unit,
          vbreak, fine_ticks, comp_ticks, vmax, clone_label, lazy_label):
    # ---- geometry -----------------------------------------------------
    W = 960
    left = 132          # repo-name gutter
    right = 28
    plot_x0 = left
    plot_x1 = W - right
    plotW = plot_x1 - plot_x0

    fine_frac = 0.54
    gapW = 26                       # visual break gap between the two panels
    usable = plotW - gapW
    Wf = usable * fine_frac
    Wc = usable - Wf
    x_break_a = plot_x0 + Wf        # end of detail panel
    x_break_b = x_break_a + gapW    # start of compressed panel

    top = 132            # first row baseline area (title+legend live above)
    pitch = 40           # vertical distance between repo groups
    bh = 13              # individual bar height
    bgap = 3             # gap between the two bars in a group
    n = len(rows)
    plot_bottom = top + n * pitch
    H = plot_bottom + 64

    def xmap(v):
        if v <= vbreak:
            return plot_x0 + (v / vbreak) * Wf
        return x_break_b + ((v - vbreak) / (vmax - vbreak)) * Wc

    MIN_BAR = 3.0  # px: guarantee even the tiniest value is a visible sliver

    out = []
    out.append(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
        f'viewBox="0 0 {W} {H}" font-family="{FONT}">')
    out.append(f'<rect width="{W}" height="{H}" fill="#ffffff"/>')

    # ---- title + subtitle --------------------------------------------
    out.append(f'<text x="{plot_x0}" y="40" font-size="23" font-weight="700" '
               f'fill="{INK}">{esc(title)}</text>')
    out.append(f'<text x="{plot_x0}" y="64" font-size="13.5" '
               f'fill="{MUTE}">{esc(subtitle)}</text>')

    # ---- legend ------------------------------------------------------
    ly = 90
    lx = plot_x0
    out.append(f'<rect x="{lx}" y="{ly-11}" width="15" height="15" rx="3" '
               f'fill="{CLONE_COLOR}"/>')
    out.append(f'<text x="{lx+21}" y="{ly+1}" font-size="14" '
               f'font-weight="600" fill="{INK}">{esc(clone_label)}</text>')
    lx2 = lx + 26 + 8 * len(clone_label) + 34
    out.append(f'<rect x="{lx2}" y="{ly-11}" width="15" height="15" rx="3" '
               f'fill="{LAZY_COLOR}"/>')
    out.append(f'<text x="{lx2+21}" y="{ly+1}" font-size="14" '
               f'font-weight="600" fill="{INK}">{esc(lazy_label)}</text>')

    # ---- gridlines + x tick labels (both panels) ---------------------
    def vline(x, color, dash=None, wid=1):
        d = f' stroke-dasharray="{dash}"' if dash else ""
        out.append(f'<line x1="{x:.1f}" y1="{top-10}" x2="{x:.1f}" '
                   f'y2="{plot_bottom}" stroke="{color}" '
                   f'stroke-width="{wid}"{d}/>')

    def tick_label(x, v):
        out.append(f'<text x="{x:.1f}" y="{plot_bottom+22}" font-size="12" '
                   f'fill="{MUTE}" text-anchor="middle">{int(v)}</text>')

    for v in fine_ticks:
        x = xmap(v)
        vline(x, GRID)
        tick_label(x, v)
    for v in comp_ticks:
        x = xmap(v)
        vline(x, GRID)
        tick_label(x, v)

    # baseline (x=0) a touch darker
    vline(plot_x0, "#cfd6dc")

    # ---- the break marker (zig-zag band) -----------------------------
    bx = (x_break_a + x_break_b) / 2
    out.append(f'<rect x="{x_break_a:.1f}" y="{top-10}" width="{gapW}" '
               f'height="{plot_bottom-(top-10)}" fill="#ffffff"/>')
    # two diagonal slashes top & bottom to signal the scale break
    for yy in (top - 10, plot_bottom):
        out.append(
            f'<path d="M{x_break_a+5:.1f} {yy-6} l8 12 M{x_break_a+13:.1f} '
            f'{yy-6} l8 12" stroke="{AXIS}" stroke-width="1.6" '
            f'fill="none"/>')
    out.append(f'<text x="{bx:.1f}" y="{top-16}" font-size="11" '
               f'fill="{MUTE}" text-anchor="middle">break</text>')

    # note that the right panel is compressed
    out.append(
        f'<text x="{x_break_b+4:.1f}" y="{plot_bottom+40}" font-size="11.5" '
        f'fill="{MUTE}">scale compressed above {int(vbreak)} {esc(unit)}</text>')
    out.append(
        f'<text x="{plot_x0}" y="{plot_bottom+40}" font-size="11.5" '
        f'fill="{MUTE}">detail scale (0–{int(vbreak)} {esc(unit)})</text>')

    # ---- rows ---------------------------------------------------------
    for i, r in enumerate(rows):
        gy = top + i * pitch          # top of this group's row band
        cy = gy + 6                   # clone bar top
        ly_ = cy + bh + bgap          # lazy bar top
        mid = gy + 6 + bh + bgap / 2  # vertical centre of the pair

        # repo name (right-aligned in the gutter)
        out.append(f'<text x="{plot_x0-12}" y="{mid+4:.1f}" font-size="13.5" '
                   f'font-weight="600" fill="{INK}" text-anchor="end">'
                   f'{esc(r["repo"])}</text>')

        for key, color, ytop in ((clone_key, CLONE_COLOR, cy),
                                 (lazy_key, LAZY_COLOR, ly_)):
            v = r[key]
            xend = xmap(v)
            blen = max(xend - plot_x0, MIN_BAR)
            xend = plot_x0 + blen
            out.append(f'<rect x="{plot_x0}" y="{ytop}" width="{blen:.1f}" '
                       f'height="{bh}" rx="2.5" fill="{color}"/>')
            label = fmt(v, unit)
            ty = ytop + bh - 3
            if blen > 64:
                # inside, right-aligned, white
                out.append(
                    f'<text x="{xend-7:.1f}" y="{ty:.1f}" font-size="12.5" '
                    f'font-weight="700" fill="#ffffff" text-anchor="end">'
                    f'{esc(label)}</text>')
            else:
                out.append(
                    f'<text x="{xend+7:.1f}" y="{ty:.1f}" font-size="12.5" '
                    f'font-weight="700" fill="{color}" text-anchor="start">'
                    f'{esc(label)}</text>')

    out.append('</svg>')
    return "\n".join(out)


def main():
    with open(DATA) as f:
        data = json.load(f)

    # ---- DISK chart (sorted by full-clone size, largest first) -------
    disk_rows = sorted(data, key=lambda r: r["clone_mb"], reverse=True)
    disk_svg = chart(
        disk_rows, "clone_mb", "lazy_mb",
        title="Disk to a working copy — lower is better",
        subtitle="Bytes written for a shallow checkout vs a lazy full-history "
                 "mount, per repository.",
        unit="MB", vbreak=300,
        fine_ticks=[0, 100, 200, 300],
        comp_ticks=[1000, 2000, 3000, 4000],
        vmax=4250,
        clone_label="git clone --depth 1",
        lazy_label="git lazy-mount")

    # ---- TIME chart (sorted by clone time, largest first) ------------
    time_rows = sorted(data, key=lambda r: r["clone_task_s"], reverse=True)
    time_svg = chart(
        time_rows, "clone_task_s", "lazy_task_s",
        title="Time to a ready working copy — lower is better",
        subtitle="Wall-clock seconds until the checkout is usable, per "
                 "repository (20 large open-source repos).",
        unit="s", vbreak=30,
        fine_ticks=[0, 10, 20, 30],
        comp_ticks=[60, 90, 120],
        vmax=137,
        clone_label="git clone --depth 1",
        lazy_label="git lazy-mount")

    disk_path = os.path.join(OUTDIR, "disk.svg")
    time_path = os.path.join(OUTDIR, "time.svg")
    with open(disk_path, "w") as f:
        f.write(disk_svg)
    with open(time_path, "w") as f:
        f.write(time_svg)

    for p in (disk_path, time_path):
        xml.dom.minidom.parse(p)  # raises if malformed
        print(f"{p}: {os.path.getsize(p)} bytes  (well-formed)")


if __name__ == "__main__":
    main()
