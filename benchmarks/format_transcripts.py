#!/usr/bin/env python3
"""Summarize Claude stream-json transcript TSVs from benchmark runs.

Input transcript lines are written by ts_prepend.py as:

    <seconds-since-start>\t<stream-json event>

This script intentionally avoids copying large tool payloads such as full file
contents into the generated summaries.  It extracts timing, tool-call counts,
long calls, searches, final answers, and permission denials.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


MAX_CELL = 220


@dataclass
class ToolCall:
    id: str
    name: str
    start: float
    summary: str
    end: float | None = None
    is_error: bool = False
    output: str = ""

    @property
    def duration(self) -> float:
        if self.end is None:
            return 0.0
        return max(0.0, self.end - self.start)


@dataclass
class Transcript:
    repo: str
    mode: str
    path: Path
    events: int = 0
    last_offset: float = 0.0
    result_s: float = 0.0
    api_s: float = 0.0
    turns: int = 0
    cost_usd: float = 0.0
    final_answer: str = ""
    terminal_reason: str = ""
    permission_denials: list[str] = field(default_factory=list)
    calls: list[ToolCall] = field(default_factory=list)
    sgrep_events: list[dict[str, Any]] = field(default_factory=list)

    @property
    def session_s(self) -> float:
        return self.result_s or self.last_offset

    @property
    def tool_s(self) -> float:
        return sum(call.duration for call in self.calls)

    @property
    def non_tool_s(self) -> float:
        return max(0.0, self.session_s - self.tool_s)

    @property
    def sidecar_sgrep_s(self) -> float:
        return sum(float(event.get("duration_s") or 0.0) for event in self.sgrep_events)

    @property
    def seed_sgrep_s(self) -> float:
        return sum(
            float(event.get("duration_s") or 0.0)
            for event in self.sgrep_events
            if event.get("phase") == "seed"
        )

    @property
    def sgrep_timeouts(self) -> int:
        return sum(1 for event in self.sgrep_events if int(event.get("rc") or 0) in {124, 137})

    @property
    def sgrep_cache_hits(self) -> int:
        return sum(1 for event in self.sgrep_events if event.get("cache") == "hit")

    @property
    def sgrep_cache_misses(self) -> int:
        return sum(1 for event in self.sgrep_events if event.get("cache") == "miss")


def clip(value: Any, limit: int = MAX_CELL) -> str:
    text = str(value).replace("\n", "\\n")
    if len(text) <= limit:
        return text
    return text[: limit - 1] + "…"


def tool_summary(name: str, input_obj: Any) -> str:
    if not isinstance(input_obj, dict):
        return clip(input_obj)
    if name == "Bash":
        return clip(input_obj.get("command", ""))
    if name in {"Read", "Edit", "Write"}:
        path = input_obj.get("file_path") or input_obj.get("path") or ""
        if name == "Edit":
            old = input_obj.get("old_string", "")
            return clip(f"{path} :: {old[:90]!r}")
        return clip(path)
    if name == "Glob":
        return clip(input_obj.get("pattern", input_obj))
    return clip(input_obj)


def output_summary(event: dict[str, Any]) -> str:
    result = event.get("tool_use_result")
    if isinstance(result, dict):
        pieces = []
        for key in ("stdout", "stderr", "filePath", "filenames", "numFiles"):
            value = result.get(key)
            if value:
                pieces.append(f"{key}={clip(value, 120)}")
        if pieces:
            return clip("; ".join(pieces))
    message = event.get("message") or {}
    content = message.get("content") if isinstance(message, dict) else None
    if isinstance(content, list):
        for item in content:
            if isinstance(item, dict) and item.get("type") == "tool_result":
                return clip(item.get("content", ""))
    return ""


def parse_tsv(path: Path) -> Transcript:
    repo = path.parent.name
    mode = path.name.split(".", 1)[0]
    transcript = Transcript(repo=repo, mode=mode, path=path)
    pending: dict[str, ToolCall] = {}

    with path.open(errors="replace") as f:
        for raw in f:
            raw = raw.rstrip("\n")
            if not raw:
                continue
            try:
                off_s, payload = raw.split("\t", 1)
                offset = float(off_s)
                event = json.loads(payload)
            except Exception:
                continue

            transcript.events += 1
            transcript.last_offset = max(transcript.last_offset, offset)

            etype = event.get("type")
            message = event.get("message")
            if isinstance(message, dict):
                for item in message.get("content", []) or []:
                    if not isinstance(item, dict):
                        continue
                    if item.get("type") == "tool_use":
                        call = ToolCall(
                            id=item.get("id", ""),
                            name=item.get("name", ""),
                            start=offset,
                            summary=tool_summary(item.get("name", ""), item.get("input")),
                        )
                        pending[call.id] = call
                        transcript.calls.append(call)
                    elif item.get("type") == "tool_result":
                        call_id = item.get("tool_use_id", "")
                        call = pending.pop(call_id, None)
                        if call:
                            call.end = offset
                            call.is_error = bool(item.get("is_error"))
                            call.output = output_summary(event)

            if etype == "result":
                transcript.result_s = float(event.get("duration_ms") or 0) / 1000.0
                transcript.api_s = float(event.get("duration_api_ms") or 0) / 1000.0
                transcript.turns = int(event.get("num_turns") or 0)
                transcript.cost_usd = float(event.get("total_cost_usd") or 0.0)
                transcript.final_answer = event.get("result") or ""
                transcript.terminal_reason = event.get("terminal_reason") or ""
                denials = event.get("permission_denials") or []
                transcript.permission_denials = [clip(x, 180) for x in denials]

    parse_sgrep_sidecar(transcript)
    return transcript


def parse_sgrep_sidecar(transcript: Transcript) -> None:
    path = transcript.path.with_name(f"{transcript.mode}.sgrep.tsv")
    if not path.exists():
        return
    with path.open(errors="replace") as f:
        for raw in f:
            raw = raw.rstrip("\n")
            if not raw:
                continue
            parts = raw.split("\t", 9)
            if len(parts) < 10:
                continue
            phase, start, duration, rc, limit, count, has_file, cache, hits, command = parts
            try:
                event = {
                    "phase": phase,
                    "start": float(start),
                    "duration_s": float(duration),
                    "rc": int(rc),
                    "limit_s": float(limit),
                    "count": int(count),
                    "has_file": has_file == "1",
                    "cache": cache,
                    "hits": int(hits) if hits else None,
                    "command": command,
                }
            except ValueError:
                continue
            transcript.sgrep_events.append(event)


def call_group(call: ToolCall) -> str:
    if call.name != "Bash":
        return call.name
    cmd = call.summary.strip()
    first = cmd.split(None, 1)[0] if cmd else ""
    if first == "sgrep":
        return "Bash:sgrep"
    if first == "git":
        return "Bash:git"
    if first in {"ls", "cat", "head", "sed", "mkdir"}:
        return f"Bash:{first}"
    return "Bash:other"


def tool_totals(transcript: Transcript) -> list[tuple[str, int, float]]:
    counts: dict[str, int] = defaultdict(int)
    seconds: dict[str, float] = defaultdict(float)
    for call in transcript.calls:
        group = call_group(call)
        counts[group] += 1
        seconds[group] += call.duration
    return sorted(
        ((group, counts[group], seconds[group]) for group in counts),
        key=lambda row: row[2],
        reverse=True,
    )


def write_transcript_markdown(transcript: Transcript, out: Path) -> None:
    totals = tool_totals(transcript)
    longest = sorted(transcript.calls, key=lambda c: c.duration, reverse=True)[:10]
    searches = [
        c for c in transcript.calls
        if c.name == "Bash" and c.summary.strip().split(None, 1)[:1] == ["sgrep"]
    ][:20]

    lines: list[str] = [
        f"# {transcript.repo} {transcript.mode}",
        "",
        f"- Source: `{transcript.path}`",
        f"- Session: {transcript.session_s:.1f}s, API: {transcript.api_s:.1f}s, "
        f"tool: {transcript.tool_s:.1f}s, non-tool: {transcript.non_tool_s:.1f}s",
        f"- Turns: {transcript.turns}, events: {transcript.events}, "
        f"cost: ${transcript.cost_usd:.4f}, terminal: `{transcript.terminal_reason}`",
        f"- Sgrep sidecar: {transcript.sidecar_sgrep_s:.1f}s total, "
        f"{transcript.seed_sgrep_s:.1f}s seed, {transcript.sgrep_timeouts} timeouts, "
        f"{transcript.sgrep_cache_hits} cache hits, {transcript.sgrep_cache_misses} cache misses",
        "",
        "## Tool Time",
        "",
        "| group | calls | seconds |",
        "|---|---:|---:|",
    ]
    for group, count, seconds in totals:
        lines.append(f"| `{group}` | {count} | {seconds:.1f} |")

    lines += [
        "",
        "## Longest Calls",
        "",
        "| start | duration | tool | input | outcome |",
        "|---:|---:|---|---|---|",
    ]
    for call in longest:
        lines.append(
            f"| {call.start:.1f} | {call.duration:.1f} | `{call_group(call)}` | "
            f"{clip(call.summary)} | {clip(call.output)} |"
        )

    if searches:
        lines += [
            "",
            "## Searches",
            "",
            "| start | duration | command |",
            "|---:|---:|---|",
        ]
        for call in searches:
            lines.append(f"| {call.start:.1f} | {call.duration:.1f} | `{clip(call.summary)}` |")

    if transcript.sgrep_events:
        lines += [
            "",
            "## Sgrep Sidecar",
            "",
            "| phase | duration | rc | limit | count | file | cache | hits | command |",
            "|---|---:|---:|---:|---:|---|---|---:|---|",
        ]
        for event in transcript.sgrep_events[:40]:
            lines.append(
                f"| {event['phase']} | {event['duration_s']:.1f} | {event['rc']} | "
                f"{event['limit_s']:.0f} | {event['count']} | {event['has_file']} | "
                f"{event['cache']} | {event['hits'] if event['hits'] is not None else ''} | "
                f"{clip(event['command'])} |"
            )

    if transcript.permission_denials:
        lines += ["", "## Permission Denials", ""]
        for denial in transcript.permission_denials:
            lines.append(f"- {denial}")

    lines += [
        "",
        "## Final Answer",
        "",
        clip(transcript.final_answer, 1000) or "_No result event found._",
        "",
    ]
    out.write_text("\n".join(lines))


def write_summary(transcripts: list[Transcript], out_dir: Path) -> None:
    rows = sorted(transcripts, key=lambda t: (t.repo, t.mode))
    csv_path = out_dir / "summary.csv"
    with csv_path.open("w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow([
            "repo", "mode", "session_s", "api_s", "tool_s", "non_tool_s",
            "turns", "events", "cost_usd", "sgrep_calls", "sgrep_s",
            "sidecar_sgrep_s", "seed_sgrep_s", "sgrep_timeouts",
            "sgrep_cache_hits", "sgrep_cache_misses",
            "git_s", "read_s", "edit_s", "terminal_reason", "final_answer",
        ])
        for t in rows:
            totals = {group: seconds for group, _count, seconds in tool_totals(t)}
            counts = {group: count for group, count, _seconds in tool_totals(t)}
            writer.writerow([
                t.repo,
                t.mode,
                f"{t.session_s:.1f}",
                f"{t.api_s:.1f}",
                f"{t.tool_s:.1f}",
                f"{t.non_tool_s:.1f}",
                t.turns,
                t.events,
                f"{t.cost_usd:.6f}",
                counts.get("Bash:sgrep", 0),
                f"{totals.get('Bash:sgrep', 0.0):.1f}",
                f"{t.sidecar_sgrep_s:.1f}",
                f"{t.seed_sgrep_s:.1f}",
                t.sgrep_timeouts,
                t.sgrep_cache_hits,
                t.sgrep_cache_misses,
                f"{totals.get('Bash:git', 0.0):.1f}",
                f"{totals.get('Read', 0.0):.1f}",
                f"{totals.get('Edit', 0.0):.1f}",
                t.terminal_reason,
                clip(t.final_answer, 300),
            ])

    pairs: dict[str, dict[str, Transcript]] = defaultdict(dict)
    for t in transcripts:
        pairs[t.repo][t.mode] = t

    md: list[str] = [
        "# Transcript Summary",
        "",
        f"- Transcripts: {len(transcripts)}",
        f"- Repos with pairs: {sum(1 for modes in pairs.values() if {'full', 'lazy'} <= set(modes))}",
        "",
        "## Full vs Lazy",
        "",
        "| repo | full s | lazy s | delta s | full sgrep | lazy sgrep | full turns | lazy turns |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for repo in sorted(pairs):
        full = pairs[repo].get("full")
        lazy = pairs[repo].get("lazy")
        if not full or not lazy:
            continue
        ft = {group: seconds for group, _count, seconds in tool_totals(full)}
        lt = {group: seconds for group, _count, seconds in tool_totals(lazy)}
        md.append(
            f"| {repo} | {full.session_s:.1f} | {lazy.session_s:.1f} | "
            f"{lazy.session_s - full.session_s:.1f} | "
            f"{ft.get('Bash:sgrep', 0.0):.1f} | {lt.get('Bash:sgrep', 0.0):.1f} | "
            f"{full.turns} | {lazy.turns} |"
        )

    md += [
        "",
        "## Slowest Transcripts",
        "",
        "| repo | mode | session s | api s | tool s | non-tool s | turns |",
        "|---|---|---:|---:|---:|---:|---:|",
    ]
    for t in sorted(transcripts, key=lambda x: x.session_s, reverse=True)[:20]:
        md.append(
            f"| {t.repo} | {t.mode} | {t.session_s:.1f} | {t.api_s:.1f} | "
            f"{t.tool_s:.1f} | {t.non_tool_s:.1f} | {t.turns} |"
        )

    md += ["", "## Slowest Tool Calls", "", "| repo | mode | start | duration | tool | input |", "|---|---|---:|---:|---|---|"]
    all_calls = []
    for t in transcripts:
        for call in t.calls:
            all_calls.append((t, call))
    for t, call in sorted(all_calls, key=lambda item: item[1].duration, reverse=True)[:25]:
        md.append(
            f"| {t.repo} | {t.mode} | {call.start:.1f} | {call.duration:.1f} | "
            f"`{call_group(call)}` | {clip(call.summary)} |"
        )

    (out_dir / "SUMMARY.md").write_text("\n".join(md) + "\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_dir", type=Path, help="benchmark output run directory")
    parser.add_argument(
        "--out-dir",
        type=Path,
        help="summary directory (default: RUN_DIR/transcripts-summary)",
    )
    args = parser.parse_args()

    run_dir = args.run_dir
    out_dir = args.out_dir or run_dir / "transcripts-summary"
    out_dir.mkdir(parents=True, exist_ok=True)

    transcripts = [
        parse_tsv(path)
        for path in sorted(run_dir.glob("*/*.transcript.tsv"))
    ]
    for transcript in transcripts:
        write_transcript_markdown(
            transcript,
            out_dir / f"{transcript.repo}-{transcript.mode}.md",
        )
    write_summary(transcripts, out_dir)
    print(f"wrote {len(transcripts)} transcript summaries to {out_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
