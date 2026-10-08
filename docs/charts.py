"""Draw the README benchmark charts: `python3 docs/charts.py`.

The numbers are copied from the tables in README.md; update both together.
Writes a light and a dark SVG per chart into docs/.
"""

import math
from pathlib import Path

CASES = [
    "static png, 2 lines",
    "static jpg, wrapped text",
    "static png, 3 lines rotated",
    "static png, emoji",
    "animated gif, 24 frames",
    "animated webp, 24 frames",
]

CHARTS = {
    "bench-latency": {
        "title": "New meme over HTTP, one connection",
        "subtitle": "Median latency in ms, log scale (lower is better)",
        "unit": "ms",
        "domain": (0.1, 4_000),
        "series": {
            "This port": [0.80, 1.2, 0.96, 0.74, 12.7, 15.2],
            "memegen-rs": [9.3, 9.3, 11.6, 8.5, 1555, 10.2],
            "Upstream Python": [90, 129, 531, 521, 1567, 2088],
        },
        "flags": {("memegen-rs", 3): "¹", ("memegen-rs", 5): "²"},
    },
    "bench-throughput": {
        "title": "New memes over HTTP, 64 connections",
        "subtitle": "Requests per second, log scale (higher is better)",
        "unit": "req/s",
        "domain": (1, 25_000),
        "series": {
            "This port": [9964, 5665, 2838, 10192, 190, 94],
            "memegen-rs": [1342, 1498, 1120, 1384, 6.9, 1228],
            "Upstream Python": [137, 73, 62, 88, 8.4, 5.8],
        },
        "flags": {("memegen-rs", 3): "¹", ("memegen-rs", 5): "²"},
    },
    "bench-render": {
        "title": "Render time in-process",
        "subtitle": "Median wall time per new meme in ms, log scale (lower is better)",
        "unit": "ms",
        "domain": (0.1, 2_500),
        "series": {
            "Rust, 18 threads": [0.50, 1.05, 0.71, 0.55, 12.2, 13.9],
            "Rust, 1 thread": [0.87, 2.0, 2.6, 0.70, 59, 131],
            "Upstream Python": [43.4, 77.9, 423, 130, 761, 1057],
        },
        "slots": [0, 3, 2],
        "flags": {},
    },
}

NOTES = {
    "bench-latency": "memegen-rs: ¹ draws :fire: as text, no color emoji · ² returns a single still frame",
    "bench-throughput": "memegen-rs: ¹ draws :fire: as text, no color emoji · ² returns a single still frame",
}

# Categorical slots, stepped per mode; ink and chrome per mode. A chart's
# `slots` keep each entity on one color across charts: this port is blue,
# memegen-rs orange, upstream aqua.
THEMES = {
    "light": {
        "series": ["#2a78d6", "#eb6834", "#1baf7a", "#eda100"],
        "primary": "#0b0b0b",
        "secondary": "#52514e",
        "muted": "#6b6964",
        "grid": "#e1e0d9",
        "axis": "#c3c2b7",
    },
    "dark": {
        "series": ["#3987e5", "#d95926", "#199e70", "#c98500"],
        "primary": "#f0f6fc",
        "secondary": "#c3c2b7",
        "muted": "#9a988f",
        "grid": "#2c2c2a",
        "axis": "#484845",
    },
}

WIDTH = 760
LEFT = 196  # case labels
RIGHT = 84  # room for value labels past the end of the axis
TOP = 92
BAR = 10
GAP = 2
GROUP_PAD = 16
FONT = 'system-ui, -apple-system, "Segoe UI", Helvetica, Arial, sans-serif'


def fmt(value: float) -> str:
    if value >= 1000:
        return f"{value:,.0f}"
    if value >= 100:
        return f"{value:.0f}"
    if value >= 10:
        return f"{value:.1f}".rstrip("0").rstrip(".")
    return f"{value:.2f}".rstrip("0").rstrip(".")


def tick(value: float) -> str:
    return f"{value:,.0f}" if value >= 1 else f"{value:g}"


def bar_path(x0: float, x1: float, y: float, h: float, r: float = 4) -> str:
    """Square at the baseline, rounded at the data end."""
    r = min(r, (x1 - x0) / 2, h / 2)
    return (
        f"M{x0:.1f},{y:.1f}H{x1 - r:.1f}"
        f"A{r},{r} 0 0 1 {x1:.1f},{y + r:.1f}V{y + h - r:.1f}"
        f"A{r},{r} 0 0 1 {x1 - r:.1f},{y + h:.1f}H{x0:.1f}Z"
    )


def esc(text: str) -> str:
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def render(chart: dict, theme: dict, note: str | None) -> str:
    lo, hi = chart["domain"]
    plot_w = WIDTH - LEFT - RIGHT
    names = list(chart["series"])
    colors = [theme["series"][i] for i in chart.get("slots", range(len(names)))]
    group_h = len(names) * BAR + (len(names) - 1) * GAP
    plot_h = len(CASES) * group_h + (len(CASES) + 1) * GROUP_PAD
    height = TOP + plot_h + 32 + (22 if note else 0)

    def x(value: float) -> float:
        return LEFT + plot_w * math.log10(value / lo) / math.log10(hi / lo)

    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{height}" '
        f'viewBox="0 0 {WIDTH} {height}" font-family=\'{FONT}\'>',
        f'<title>{esc(chart["title"])}</title>',
        f'<text x="0" y="20" font-size="16" font-weight="600" fill="{theme["primary"]}">'
        f'{esc(chart["title"])}</text>',
        f'<text x="0" y="40" font-size="13" fill="{theme["secondary"]}">'
        f'{esc(chart["subtitle"])}</text>',
    ]

    # Legend.
    lx = 0
    for name, color in zip(names, colors):
        out.append(f'<rect x="{lx}" y="58" width="12" height="12" rx="3" fill="{color}"/>')
        out.append(
            f'<text x="{lx + 18}" y="68.5" font-size="13" fill="{theme["secondary"]}">'
            f"{esc(name)}</text>"
        )
        lx += 18 + len(name) * 7.4 + 24

    # Gridlines and ticks at each power of ten.
    bottom = TOP + plot_h
    exponent = round(math.log10(lo))
    while 10**exponent <= hi:
        value = 10**exponent
        gx = x(value)
        color = theme["axis"] if value == lo else theme["grid"]
        out.append(
            f'<line x1="{gx:.1f}" y1="{TOP}" x2="{gx:.1f}" y2="{bottom}" '
            f'stroke="{color}" stroke-width="1"/>'
        )
        out.append(
            f'<text x="{gx:.1f}" y="{bottom + 18}" font-size="11" text-anchor="middle" '
            f'fill="{theme["muted"]}" style="font-variant-numeric: tabular-nums">'
            f"{tick(value)}</text>"
        )
        exponent += 1

    # Bars, each with its value at the tip.
    for i, case in enumerate(CASES):
        gy = TOP + GROUP_PAD + i * (group_h + GROUP_PAD)
        out.append(
            f'<text x="{LEFT - 12}" y="{gy + group_h / 2 + 4.5:.1f}" font-size="13" '
            f'text-anchor="end" fill="{theme["primary"]}">{esc(case)}</text>'
        )
        for j, (name, color) in enumerate(zip(names, colors)):
            value = chart["series"][name][i]
            y = gy + j * (BAR + GAP)
            x1 = x(value)
            flag = chart["flags"].get((name, i), "")
            out.append(
                f'<path d="{bar_path(LEFT, x1, y, BAR)}" fill="{color}">'
                f"<title>{esc(name)}: {fmt(value)} {chart['unit']}</title></path>"
            )
            out.append(
                f'<text x="{x1 + 5:.1f}" y="{y + BAR - 1}" font-size="11" '
                f'fill="{theme["secondary"]}">{fmt(value)}{flag}</text>'
            )

    if note:
        out.append(
            f'<text x="0" y="{height - 6}" font-size="12" fill="{theme["muted"]}">'
            f"{esc(note)}</text>"
        )
    out.append("</svg>")
    return "\n".join(out) + "\n"


def main() -> None:
    docs = Path(__file__).parent
    for key, chart in CHARTS.items():
        for mode, theme in THEMES.items():
            path = docs / f"{key}-{mode}.svg"
            path.write_text(render(chart, theme, NOTES.get(key)))
            print(path)


if __name__ == "__main__":
    main()
