#!/usr/bin/env python3
"""
Plot per-thread proof-time composition from a Verus --time-expanded
--output-json log.

Simulates Verus's FIFO + first-idle scheduling onto times-ms.num-threads
threads and produces one stacked bar per thread. Within a thread, the bar
is the concatenation (in start-time order) of the buckets assigned to that
thread; each bucket contributes:

  - air            (AIR encoding + post-query Z3 round-trips)
  - smt-init       (initial SMT-LIB dispatch + version check)
  - <function>...  (per-function smt-run; one segment per function)

The tallest bar is the parallel-phase makespan; threads finishing earlier
show how unbalanced the workload is.

Usage:
  ./plot_proof_time.py [path/to/log.json] [-o out.png] [--schedule fifo|lpt]

Default input is ./log.json next to this script.
"""

import argparse
import itertools
import json
import sys
from pathlib import Path

import matplotlib
import matplotlib.pyplot as plt


def short_fn(name: str, max_len: int = 40) -> str:
    """Shorten function paths for the legend (keep the leaf)."""
    if len(name) <= max_len:
        return name
    leaf = name.rsplit("::", 1)[-1]
    return leaf if len(leaf) <= max_len else leaf[: max_len - 1] + "…"


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    p.add_argument(
        "log",
        nargs="?",
        default=str(Path(__file__).resolve().parent / "log.json"),
        help="Path to the JSON log",
    )
    p.add_argument("-o", "--output", default=None, help="Output image path (default: show interactively)")
    p.add_argument("--unit", choices=["ms", "s"], default="ms", help="Y-axis unit")
    p.add_argument(
        "--schedule",
        choices=["fifo", "lpt"],
        default="fifo",
        help=(
            "Bucket dispatch order: 'fifo' = lexicographic (matches buckets::get_buckets), "
            "'lpt' = longest-first (theoretical optimum packing)."
        ),
    )
    p.add_argument(
        "--no-legend",
        action="store_true",
        help="Suppress the legend (useful when there are too many functions).",
    )
    args = p.parse_args()

    if args.output:
        # Avoid pulling in a GUI backend when we just want to write a PNG.
        matplotlib.use("Agg")

    with open(args.log) as f:
        data = json.load(f)

    times_ms = data.get("times-ms", {})
    buckets = times_ms.get("module-times")
    if not buckets:
        print("error: no times-ms.module-times in log; was --time-expanded --output-json passed?", file=sys.stderr)
        return 1

    scale = 1.0 if args.unit == "ms" else 1 / 1000.0
    unit_label = "ms" if args.unit == "ms" else "s"

    # Schedule buckets onto N threads using FIFO+first-idle (matches Verus).
    num_threads = times_ms.get("num-threads")
    if not num_threads:
        print("error: times-ms.num-threads missing in log", file=sys.stderr)
        return 1
    if args.schedule == "fifo":
        # Matches buckets::get_buckets, which sorts by BucketId lex order.
        queue = sorted(buckets, key=lambda b: b["module"])
    else:  # lpt
        queue = sorted(buckets, key=lambda b: -b["total"])
    # First-idle-thread scheduling: min-heap of (next_free_time, thread_id).
    import heapq
    heap = [(0, t) for t in range(num_threads)]
    heapq.heapify(heap)
    column_buckets: list[list[dict]] = [[] for _ in range(num_threads)]
    for b in queue:
        free, tid = heapq.heappop(heap)
        column_buckets[tid].append(b)
        heapq.heappush(heap, (free + b["total"], tid))
    labels = [f"T{t}" for t in range(num_threads)]
    x_axis_label = f"thread (simulated, schedule={args.schedule}, N={num_threads})"
    title_prefix = "Per-thread proof-time composition"

    n = len(column_buckets)
    fig_width = max(8.0, 1.0 * n + 3)
    fig, ax = plt.subplots(figsize=(fig_width, 7.5))

    air_color = "#4c4c4c"   # dark gray
    init_color = "#bdbdbd"  # light gray
    func_palette = list(plt.get_cmap("tab20").colors) + list(plt.get_cmap("tab20b").colors)
    color_iter = itertools.cycle(func_palette)
    func_color: dict[str, tuple] = {}

    def color_for(fn: str):
        if fn not in func_color:
            func_color[fn] = next(color_iter)
        return func_color[fn]

    x = list(range(n))
    bottoms = [0.0] * n
    legend_seen: set[str] = set()

    def add_segment(col: int, height: float, color, label: str | None):
        kwargs = {"color": color, "edgecolor": "white", "linewidth": 0.4}
        if label and label not in legend_seen:
            kwargs["label"] = label
            legend_seen.add(label)
        ax.bar([col], [height], bottom=[bottoms[col]], **kwargs)
        bottoms[col] += height

    # Walk each column's buckets in order and stack air → init → per-function smt.
    for col_idx, bucket_list in enumerate(column_buckets):
        for b in bucket_list:
            v = b["air"] * scale
            if v > 0:
                add_segment(col_idx, v, air_color, "air")
            v = b["smt-init"] * scale
            if v > 0:
                add_segment(col_idx, v, init_color, "smt-init")
            for fn in sorted(b["function-breakdown"], key=lambda f: -f["smt-time"]):
                fv = fn["smt-time"] * scale
                if fv <= 0:
                    continue
                add_segment(col_idx, fv, color_for(fn["function"]), short_fn(fn["function"]))

    # Cosmetics
    ax.set_xticks(x)
    ax.set_xticklabels(labels, rotation=0, ha="center", fontsize=9)
    ax.set_ylabel(f"wall time ({unit_label})")
    ax.set_xlabel(x_axis_label)

    # Spinoff buckets are named "module#function"; strip the suffix so
    # each source module is counted once regardless of dispatch mode.
    # Functions are counted from function-breakdown so the total stays
    # consistent across spinoff/non-spinoff (spinoff buckets without a
    # real SMT query have an empty/zero breakdown and are excluded).
    num_modules = len(set(b["module"].split("#", 1)[0] for b in buckets))
    num_functions = sum(
        1
        for b in buckets
        for fn in b.get("function-breakdown", [])
        if fn.get("smt-time", 0) > 0
    )

    title = title_prefix
    totals = times_ms.get("verify-totals", {})
    if totals:
        title += (
            f"  —  total verify {totals.get('total-verify', '?')} ms,"
            f" air {totals.get('air', '?')} ms, smt-run {totals.get('smt-run', '?')} ms"
        )
    title += f"  —  {num_modules} modules, {num_functions} functions"
    if bottoms:
        title += f"  —  makespan {max(bottoms):.0f} {unit_label}"
    ax.set_title(title, fontsize=10)
    ax.grid(axis="y", linestyle=":", alpha=0.4)

    # Annotate column totals (this thread's accumulated wall time).
    for col, total in enumerate(bottoms):
        ax.text(col, total, f"{total:.0f}", ha="center", va="bottom", fontsize=7, color="black")

    # Reference lines:
    #   makespan      = simulation's tallest column
    #   verify-crate  = real wall-clock for the parallel phase
    #                   (verification.total - verification.vir.total)
    makespan = max(bottoms) if bottoms else 0.0
    if bottoms:
        ax.axhline(
            makespan,
            linestyle="--",
            linewidth=0.8,
            color="#888888",
            label=f"simulated makespan ({makespan:.0f} {unit_label})",
        )
    verification = times_ms.get("verification", {})
    vir_block = verification.get("vir", {})
    if "total" in verification and "total" in vir_block:
        actual = (verification["total"] - vir_block["total"]) * scale
        ax.axhline(
            actual,
            linestyle="--",
            linewidth=1.0,
            color="#d62728",
            label=f"actual verify-crate wall-clock ({actual:.0f} {unit_label})",
        )
        # Annotate gap (positive = sequential overhead the simulation doesn't see).
        gap = actual - makespan
        pct = (gap / actual * 100.0) if actual > 0 else 0.0
        ax.text(
            n - 0.5,
            actual,
            f" gap={gap:+.0f} {unit_label} ({pct:+.1f}%)",
            ha="right",
            va="bottom" if gap >= 0 else "top",
            fontsize=8,
            color="#d62728",
        )

    if not args.no_legend:
        handles, lbls = ax.get_legend_handles_labels()
        ncol = 1 if len(lbls) <= 12 else 2
        ax.legend(handles, lbls, loc="upper right", fontsize=7, ncol=ncol, framealpha=0.9)

    fig.tight_layout()
    if args.output:
        fig.savefig(args.output, dpi=300)
        print(f"wrote {args.output}")
    else:
        plt.show()
    return 0


if __name__ == "__main__":
    sys.exit(main())
