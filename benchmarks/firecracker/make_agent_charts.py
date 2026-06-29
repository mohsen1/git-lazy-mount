#!/usr/bin/env python3
"""Render full-agent benchmark charts from bench_repo.sh metrics.

Input can be either:

  * a directory containing run/<repo>/metrics.json files, or
  * a JSON file previously written by this script.

The script writes agent_chartdata.json plus two SVG charts in the output
directory: agent-time.svg and agent-disk.svg.
"""

from __future__ import annotations

import json
import math
import os
import sys
import xml.dom.minidom
from pathlib import Path
from typing import Any


SOURCE = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("run")
OUTDIR = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(".")
OUTDIR.mkdir(parents=True, exist_ok=True)

FULL_SETUP = "#e4572e"
FULL_AGENT = "#f2a07f"
LAZY_SETUP = "#2a9d8f"
LAZY_AGENT = "#94d2bd"
FULL_DISK = "#e4572e"
LAZY_DISK = "#2a9d8f"
AXIS = "#54606b"
GRID = "#e7ebef"
INK = "#1d2329"
MUTE = "#6b7782"
FONT = ("-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,"
        "Arial,sans-serif")


def esc(value: Any) -> str:
    return (str(value).replace("&", "&amp;").replace("<", "&lt;")
            .replace(">", "&gt;").replace('"', "&quot;"))


def seconds(value: Any) -> float:
    try:
        return round(float(value), 1)
    except Exception:
        return 0.0


def mib(value: Any) -> int:
    try:
        return int(round(int(value) / 1048576))
    except Exception:
        return 0


def fmt_s(value: float) -> str:
    if abs(value - round(value)) < 0.05:
        return f"{int(round(value))} s"
    return f"{value:.1f} s"


def fmt_mb(value: float) -> str:
    if value >= 1024:
        return f"{value / 1024:.1f} GB"
    return f"{int(round(value))} MB"


def nice_step(max_value: float, target_ticks: int = 6) -> float:
    if max_value <= 0:
        return 1
    raw = max_value / target_ticks
    mag = 10 ** math.floor(math.log10(raw))
    for factor in (1, 2, 5, 10):
        step = factor * mag
        if raw <= step:
            return step
    return 10 * mag


def tick_values(max_value: float) -> list[float]:
    step = nice_step(max_value)
    limit = math.ceil(max_value / step) * step
    ticks = []
    cur = 0.0
    while cur <= limit + step / 2:
        ticks.append(cur)
        cur += step
    return ticks


def load_rows(source: Path) -> list[dict[str, Any]]:
    if source.is_file():
        with source.open() as f:
            data = json.load(f)
        if isinstance(data, dict):
            data = data.get("rows", [])
        return list(data)

    candidates = []
    if (source / "run").is_dir():
        candidates.extend((source / "run").glob("*/metrics.json"))
    candidates.extend(source.glob("*/metrics.json"))
    candidates.extend(source.glob("metrics.json"))

    seen = set()
    rows = []
    for path in sorted(candidates):
        resolved = path.resolve()
        if resolved in seen or path.parent.name == "results":
            continue
        seen.add(resolved)
        try:
            with path.open() as f:
                metric = json.load(f)
        except Exception:
            continue
        if not metric or "full" not in metric or "lazy" not in metric:
            continue
        full = metric.get("full") or {}
        lazy = metric.get("lazy") or {}
        full_setup_s = seconds(full.get("clone_s"))
        full_agent_s = seconds(full.get("agent_s"))
        lazy_setup_s = seconds(lazy.get("mount_s"))
        lazy_agent_s = seconds(lazy.get("agent_s"))
        full_disk_mb = mib(full.get("worktree_bytes")) + mib(full.get("dotgit_bytes"))
        row = {
            "repo": metric.get("repo") or path.parent.name,
            "clone": metric.get("clone") or "",
            "files": int(metric.get("files") or 0),
            "full_setup_s": full_setup_s,
            "full_agent_s": full_agent_s,
            "full_total_s": round(full_setup_s + full_agent_s, 1),
            "lazy_setup_s": lazy_setup_s,
            "lazy_agent_s": lazy_agent_s,
            "lazy_total_s": round(lazy_setup_s + lazy_agent_s, 1),
            "saved_s": round((full_setup_s + full_agent_s) - (lazy_setup_s + lazy_agent_s), 1),
            "full_worktree_mb": mib(full.get("worktree_bytes")),
            "full_git_mb": mib(full.get("dotgit_bytes")),
            "full_disk_mb": full_disk_mb,
            "lazy_initial_mb": mib(lazy.get("initial_bytes")),
            "lazy_final_mb": mib(lazy.get("final_bytes")),
            "lazy_cache_mb": mib(lazy.get("cache_bytes")),
            "lazy_git_mb": mib(lazy.get("git_bytes")),
            "lazy_overlay_mb": mib(lazy.get("overlay_bytes")),
        }
        rows.append(row)

    return rows


def legend(out: list[str], x: int, y: int, items: list[tuple[str, str]]) -> None:
    cursor = x
    for label, color in items:
        out.append(f'<rect x="{cursor}" y="{y-11}" width="15" height="15" '
                   f'rx="3" fill="{color}"/>')
        out.append(f'<text x="{cursor+21}" y="{y+1}" font-size="13.5" '
                   f'font-weight="600" fill="{INK}">{esc(label)}</text>')
        cursor += 34 + 7 * len(label)


def stacked_time_chart(rows: list[dict[str, Any]]) -> str:
    rows = sorted(rows, key=lambda r: r["full_total_s"], reverse=True)
    max_value = max(
        [r["full_total_s"] for r in rows] + [r["lazy_total_s"] for r in rows] + [1]
    )
    ticks = tick_values(max_value)
    axis_max = ticks[-1]

    width = 1080
    left = 138
    right = 150
    plot_w = width - left - right
    top = 136
    pitch = 42
    bar_h = 14
    gap = 4
    bottom = top + len(rows) * pitch
    height = bottom + 70

    def x(value: float) -> float:
        return left + (value / axis_max) * plot_w

    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" font-family="{FONT}">',
        f'<rect width="{width}" height="{height}" fill="#ffffff"/>',
        f'<text x="{left}" y="40" font-size="23" font-weight="700" '
        f'fill="{INK}">Agent task wall-clock - lower is better</text>',
        f'<text x="{left}" y="64" font-size="13.5" fill="{MUTE}">'
        'Fresh Firecracker microVM per repo; setup plus one Claude code-search/edit/commit task.</text>',
    ]
    legend(out, left, 92, [
        ("clone setup", FULL_SETUP),
        ("full agent", FULL_AGENT),
        ("lazy mount", LAZY_SETUP),
        ("lazy agent", LAZY_AGENT),
    ])

    for tick in ticks:
        tx = x(tick)
        out.append(f'<line x1="{tx:.1f}" y1="{top-10}" x2="{tx:.1f}" '
                   f'y2="{bottom}" stroke="{GRID}" stroke-width="1"/>')
        out.append(f'<text x="{tx:.1f}" y="{bottom+23}" font-size="12" '
                   f'fill="{MUTE}" text-anchor="middle">{int(tick)}</text>')
    out.append(f'<line x1="{left}" y1="{top-10}" x2="{left}" y2="{bottom}" '
               f'stroke="#cfd6dc" stroke-width="1"/>')

    for i, row in enumerate(rows):
        gy = top + i * pitch
        full_y = gy + 4
        lazy_y = full_y + bar_h + gap
        mid = gy + 4 + bar_h + gap / 2
        out.append(f'<text x="{left-12}" y="{mid+4:.1f}" font-size="13.5" '
                   f'font-weight="600" fill="{INK}" text-anchor="end">'
                   f'{esc(row["repo"])}</text>')

        for y, setup_key, agent_key, total_key, setup_color, agent_color in (
            (full_y, "full_setup_s", "full_agent_s", "full_total_s", FULL_SETUP, FULL_AGENT),
            (lazy_y, "lazy_setup_s", "lazy_agent_s", "lazy_total_s", LAZY_SETUP, LAZY_AGENT),
        ):
            setup_w = max(2.0, (row[setup_key] / axis_max) * plot_w)
            total_w = max(2.0, (row[total_key] / axis_max) * plot_w)
            agent_w = max(0.0, total_w - setup_w)
            out.append(f'<rect x="{left}" y="{y}" width="{setup_w:.1f}" '
                       f'height="{bar_h}" rx="2.5" fill="{setup_color}"/>')
            if agent_w > 0:
                out.append(f'<rect x="{left+setup_w:.1f}" y="{y}" '
                           f'width="{agent_w:.1f}" height="{bar_h}" rx="2.5" '
                           f'fill="{agent_color}"/>')
            tx = left + total_w
            label = fmt_s(row[total_key])
            if tx < left + plot_w - 72:
                out.append(f'<text x="{tx+7:.1f}" y="{y+bar_h-3}" '
                           f'font-size="12.5" font-weight="700" fill="{INK}">'
                           f'{esc(label)}</text>')
            else:
                out.append(f'<text x="{tx-7:.1f}" y="{y+bar_h-3}" '
                           f'font-size="12.5" font-weight="700" fill="#ffffff" '
                           f'text-anchor="end">{esc(label)}</text>')

    out.append(f'<text x="{left}" y="{bottom+45}" font-size="11.5" '
               f'fill="{MUTE}">x-axis in seconds; each pair is full clone+agent '
               f'above lazy mount+agent.</text>')
    out.append("</svg>")
    return "\n".join(out)


def disk_chart(rows: list[dict[str, Any]]) -> str:
    rows = sorted(rows, key=lambda r: r["full_disk_mb"], reverse=True)
    max_value = max(
        [r["full_disk_mb"] for r in rows] + [r["lazy_final_mb"] for r in rows] + [1]
    )
    ticks = tick_values(max_value)
    axis_max = ticks[-1]

    width = 1080
    left = 138
    right = 150
    plot_w = width - left - right
    top = 136
    pitch = 42
    bar_h = 14
    gap = 4
    bottom = top + len(rows) * pitch
    height = bottom + 70

    def xw(value: float) -> float:
        return max(2.0, (value / axis_max) * plot_w)

    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" font-family="{FONT}">',
        f'<rect width="{width}" height="{height}" fill="#ffffff"/>',
        f'<text x="{left}" y="40" font-size="23" font-weight="700" '
        f'fill="{INK}">Disk after the agent task - lower is better</text>',
        f'<text x="{left}" y="64" font-size="13.5" fill="{MUTE}">'
        'Full-history clone footprint vs lazy workspace after the same edit/commit task.</text>',
    ]
    legend(out, left, 92, [
        ("git clone", FULL_DISK),
        ("git lazy-mount", LAZY_DISK),
    ])

    for tick in ticks:
        tx = left + (tick / axis_max) * plot_w
        out.append(f'<line x1="{tx:.1f}" y1="{top-10}" x2="{tx:.1f}" '
                   f'y2="{bottom}" stroke="{GRID}" stroke-width="1"/>')
        out.append(f'<text x="{tx:.1f}" y="{bottom+23}" font-size="12" '
                   f'fill="{MUTE}" text-anchor="middle">{fmt_mb(tick)}</text>')
    out.append(f'<line x1="{left}" y1="{top-10}" x2="{left}" y2="{bottom}" '
               f'stroke="#cfd6dc" stroke-width="1"/>')

    for i, row in enumerate(rows):
        gy = top + i * pitch
        full_y = gy + 4
        lazy_y = full_y + bar_h + gap
        mid = gy + 4 + bar_h + gap / 2
        out.append(f'<text x="{left-12}" y="{mid+4:.1f}" font-size="13.5" '
                   f'font-weight="600" fill="{INK}" text-anchor="end">'
                   f'{esc(row["repo"])}</text>')
        for y, key, color in (
            (full_y, "full_disk_mb", FULL_DISK),
            (lazy_y, "lazy_final_mb", LAZY_DISK),
        ):
            width_px = xw(row[key])
            out.append(f'<rect x="{left}" y="{y}" width="{width_px:.1f}" '
                       f'height="{bar_h}" rx="2.5" fill="{color}"/>')
            tx = left + width_px
            label = fmt_mb(row[key])
            if tx < left + plot_w - 72:
                out.append(f'<text x="{tx+7:.1f}" y="{y+bar_h-3}" '
                           f'font-size="12.5" font-weight="700" fill="{color}">'
                           f'{esc(label)}</text>')
            else:
                out.append(f'<text x="{tx-7:.1f}" y="{y+bar_h-3}" '
                           f'font-size="12.5" font-weight="700" fill="#ffffff" '
                           f'text-anchor="end">{esc(label)}</text>')

    out.append(f'<text x="{left}" y="{bottom+45}" font-size="11.5" '
               f'fill="{MUTE}">x-axis shows MiB/GB on disk; lazy includes fetched '
               f'objects, cache, and overlay after commit.</text>')
    out.append("</svg>")
    return "\n".join(out)


def main() -> None:
    rows = load_rows(SOURCE)
    if not rows:
        raise SystemExit(f"no agent metrics found in {SOURCE}")
    rows = sorted(rows, key=lambda r: r["repo"])

    data_path = OUTDIR / "agent_chartdata.json"
    with data_path.open("w") as f:
        json.dump(rows, f, indent=2)
        f.write("\n")

    charts = {
        OUTDIR / "agent-time.svg": stacked_time_chart(rows),
        OUTDIR / "agent-disk.svg": disk_chart(rows),
    }
    for path, svg in charts.items():
        path.write_text(svg)
        xml.dom.minidom.parse(str(path))
        print(f"{path}: {path.stat().st_size} bytes (well-formed)")
    print(f"{data_path}: {len(rows)} rows")


if __name__ == "__main__":
    main()
