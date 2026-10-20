"""Render the Portal vs LiveKit SDK comparison as light and dark SVGs, plus
the markdown tables that go under them.

  python charts.py --portal results/portal-r*.json --sdk results/sdk-r*.json \
    --sdk-playout results/sdk-pd-r*.json --out ../../docs/reference/benchmarks
"""
from __future__ import annotations

import argparse
import json
import statistics
from pathlib import Path

# Categorical slots 1-3 of the reference palette, validated as a set in both
# themes.
THEMES = {
    "light": {
        "surface": "#fcfcfb", "ink": "#0b0b0b", "ink2": "#52514e", "muted": "#898781",
        "grid": "#e1e0d9", "axis": "#c3c2b7",
        "series": ["#2a78d6", "#eb6834", "#1baf7a"],
    },
    "dark": {
        "surface": "#1a1a19", "ink": "#f0efec", "ink2": "#c3c2b7", "muted": "#898781",
        "grid": "#2c2c2a", "axis": "#383835",
        "series": ["#3987e5", "#d95926", "#199e70"],
    },
}
CODEC_NAMES = {"H264": "H.264", "VP8": "VP8", "VP9": "VP9", "AV1": "AV1"}
FONT = 'system-ui, -apple-system, &quot;Segoe UI&quot;, sans-serif'


def load(paths: list[str]) -> list[dict]:
    return [{s["scenario"]: s for s in json.load(open(p))["scenarios"]} for p in paths]


def values(runs: list[dict], scenario: str, role: str, key: str) -> list[float]:
    out = []
    for r in runs:
        v = ((r.get(scenario, {}).get(role) or {}).get(key) or {}).get("p50")
        if v is not None:
            out.append(v)
    return out


def nice_max(v: float) -> float:
    candidates = sorted(s * m for s in (1, 2, 2.5, 5) for m in (0.1, 1, 10, 100, 1000))
    return next((c for c in candidates if c >= v), v)


def ticks(top: float) -> list[float]:
    for n in (5, 4, 6, 3):
        step = top / n
        if abs(step - round(step, 1)) < 1e-9:
            return [round(i * step, 2) for i in range(n + 1)]
    return [0, top]


def fmt(v: float) -> str:
    return f"{v:.0f}" if v >= 10 else f"{v:.1f}"


def bar_chart(names: list[str], rows: list[tuple[str, list[list[float]]]], axis_label: str,
              theme: str) -> str:
    """Grouped horizontal bars: per row, the median p50 of each series, with a
    thin line spanning the fastest to slowest run."""
    t = THEMES[theme]
    n = len(names)
    label_w, right_pad, top, bar_h, pair_gap, row_gap = 112, 24, 44, 9, 2, 14
    width = 720
    plot_w = width - label_w - right_pad
    row_h = n * bar_h + (n - 1) * pair_gap
    height = top + len(rows) * (row_h + row_gap) + 40
    top_val = nice_max(max(max(v) for _, series in rows for v in series if v) * 1.05)
    x = lambda v: label_w + v / top_val * plot_w  # noqa: E731

    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
           f'viewBox="0 0 {width} {height}" font-family="{FONT}" role="img" '
           f'aria-label="{axis_label}: {", ".join(names)}">',
           f'<rect width="{width}" height="{height}" fill="{t["surface"]}"/>']
    lx = label_w
    for name, color in zip(names, t["series"]):
        out.append(f'<rect x="{lx}" y="14" width="12" height="12" rx="3" fill="{color}"/>')
        out.append(f'<text x="{lx + 18}" y="24" font-size="13" fill="{t["ink2"]}">{name}</text>')
        lx += 18 + len(name) * 7.2 + 24
    plot_bottom = top + len(rows) * (row_h + row_gap) - row_gap + 6
    for tv in ticks(top_val):
        gx = x(tv)
        out.append(f'<line x1="{gx:.1f}" y1="{top - 6}" x2="{gx:.1f}" y2="{plot_bottom}" '
                   f'stroke="{t["grid"] if tv else t["axis"]}" stroke-width="1"/>')
        out.append(f'<text x="{gx:.1f}" y="{plot_bottom + 16}" font-size="11" fill="{t["muted"]}" '
                   f'text-anchor="middle" style="font-variant-numeric: tabular-nums">{tv:g}</text>')
    out.append(f'<text x="{label_w + plot_w / 2:.1f}" y="{plot_bottom + 34}" font-size="12" '
               f'fill="{t["ink2"]}" text-anchor="middle">{axis_label}</text>')
    y = top
    for label, series in rows:
        out.append(f'<text x="{label_w - 10}" y="{y + row_h / 2 + 4:.1f}" font-size="12" '
                   f'fill="{t["ink"]}" text-anchor="end">{label}</text>')
        for i, (vals, color, name) in enumerate(zip(series, t["series"], names)):
            by = y + i * (bar_h + pair_gap)
            if not vals:
                continue
            med = statistics.median(vals)
            w = max(x(med) - label_w, 2)
            # Rounded only at the data end, square at the baseline.
            r = min(4, w / 2, bar_h / 2)
            out.append(
                f'<path d="M{label_w},{by} h{w - r:.1f} a{r},{r} 0 0 1 {r},{r} v{bar_h - 2 * r} '
                f'a{r},{r} 0 0 1 -{r},{r} h-{w - r:.1f} z" fill="{color}">'
                f'<title>{label}, {name}: {fmt(med)} ms median of {len(vals)} runs '
                f'({fmt(min(vals))}–{fmt(max(vals))})</title></path>')
            if len(vals) > 1:
                cy = by + bar_h / 2
                out.append(f'<line x1="{x(min(vals)):.1f}" y1="{cy}" x2="{x(max(vals)):.1f}" y2="{cy}" '
                           f'stroke="{t["ink2"]}" stroke-width="1.5" stroke-linecap="round" opacity="0.7"/>')
        y += row_h + row_gap
    out.append("</svg>")
    return "\n".join(out)


def video_rows(sets, role, key):
    rows = []
    for codec in CODEC_NAMES:
        for res in ("480p", "720p", "1080p"):
            s = f"{codec}-{res}"
            rows.append((f"{CODEC_NAMES[codec]} {res}", [values(r, s, role, key) for r in sets]))
    return rows


def table(names, rows) -> str:
    def cell(v):
        if not v:
            return "–"
        return f"{fmt(statistics.median(v))} ({fmt(min(v))}–{fmt(max(v))})"
    lines = ["| | " + " | ".join(names) + " |", "|---" * (len(names) + 1) + "|"]
    lines += [f"| {label} | " + " | ".join(cell(v) for v in series) + " |" for label, series in rows]
    return "\n".join(lines)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--portal", nargs="+", required=True)
    ap.add_argument("--sdk", nargs="+", required=True)
    ap.add_argument("--sdk-playout", nargs="+",
                    help="SDK runs in a room with min/max playout delay 0/1")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    names = ["Portal", "LiveKit SDK"]
    sets = [load(a.portal), load(a.sdk)]
    if a.sdk_playout:
        names.append("LiveKit SDK, room playout delay")
        sets.append(load(a.sdk_playout))
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    charts = {
        "e2e-latency": (video_rows(sets, "operator", "video_latency_ms"),
                        "End-to-end latency, p50 (ms)"),
        "send-blocking": (video_rows(sets[:2], "robot", "send_block_ms"),
                          "Time the send call blocks, p50 (ms)"),
    }
    for name, (rows, axis) in charts.items():
        series_names = names if name == "e2e-latency" else names[:2]
        for theme in THEMES:
            (out / f"{name}-{theme}.svg").write_text(bar_chart(series_names, rows, axis, theme))
        print(f"### {name}\n\n{table(series_names, rows)}\n")
    control = [
        ("State, robot to operator", "control-100hz", "operator", "state_latency_ms"),
        ("Action, operator to robot", "control-100hz", "robot", "action_latency_ms"),
        ("H.264 720p video, with control", "mixed-H264-720p+control", "operator", "video_latency_ms"),
        ("State, alongside H.264 720p", "mixed-H264-720p+control", "operator", "state_latency_ms"),
    ]
    rows = [(label, [values(r, s, role, k) for r in sets]) for label, s, role, k in control]
    print(f"### control\n\n{table(names, rows)}\n")


if __name__ == "__main__":
    main()
