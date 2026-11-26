#!/usr/bin/env python3
"""Measure the findings in FINDINGS.md and write their numbers into it.

    python verify/findings.py               # measure, then rewrite FINDINGS.md's numbers
    python verify/findings.py --check       # measure; fail if FINDINGS.md is stale or a claim fails
    python verify/findings.py --recapture   # capture the variants again (a scenario changed)

The first run captures the trace variants into traces/local/variants (about
4 minutes); later runs reuse them, after checking each says it is the variant
it should be. Every number comes from the engine (disk
reads with read-ahead off, and the exact pool LRU needs) or, for the
breakdown by page, from the independent model in model.py.
"""

from __future__ import annotations

import argparse
import collections
import gzip
import json
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import compare  # noqa: E402
import model  # noqa: E402

REPO = compare.REPO
DOC = REPO / "FINDINGS.md"
VARIANT_DIR = REPO / "traces" / "local" / "variants"
CAPTURE = REPO / "capture" / "capture.py"

# Each trace's --max-pages, as in its scenario's header.
TRACES = {"oltp": 20000, "reports": 20000, "ranges": 30000}
# (label, --seed, --param): seed 7 with no changes is the committed trace.
VARIANTS = [
    ("seed 7", None, None),
    ("seed 11", 11, None),
    ("seed 23", 23, None),
    ("skew 0.9", None, "user=zipf(0.9, 1..100000)"),
    ("skew 1.3", None, "user=zipf(1.3, 1..100000)"),
]
POOLS = {"1/8": 8, "1/4": 4, "1/2": 2}  # pool as a share of the database
SHOWN = ["innodb", "pg", "opt"]
NAMES = {"innodb": "InnoDB-style", "pg": "PostgreSQL-style", "opt": "Optimal"}
USES = [(1, 1, "1"), (2, 9, "2–9"), (10, 99, "10–99"), (100, None, "100+")]


@dataclass
class Point:
    reads: int  # pages read from disk, read-ahead off
    lru_frames: int | None  # the smallest pool where LRU reads no more


@dataclass
class Measured:
    pages: int
    pools: dict[str, int]  # "1/4" -> frames
    points: dict[tuple[str, str], Point]  # (pool, policy)

    def fewer(self, pool: str, policy: str) -> float:
        """Share of LRU's disk reads this policy does without."""
        return 1 - self.points[pool, policy].reads / self.points[pool, "lru"].reads

    def lru_needs(self, pool: str, policy: str) -> float:
        frames = self.points[pool, policy].lru_frames
        return frames / self.pools[pool] if frames else float("inf")


# ---------- traces ----------


def trace_path(trace: str, label: str) -> Path:
    if label == "seed 7":
        return REPO / "traces" / f"{trace}.trace.json.gz"
    return VARIANT_DIR / f"{trace}-{label.replace(' ', '-')}.trace.json.gz"


def expected_variant(seed: int | None, param: str | None) -> dict:
    """What capture.py records in a trace for these changes (see --seed, --param)."""
    out: dict = {}
    if seed is not None:
        out["seed"] = seed
    if param:
        out["params"] = [" ".join(param.split())]
    return out


def capture(
    trace: str, label: str, seed: int | None, param: str | None, again: bool = False
) -> Path:
    path = trace_path(trace, label)
    if label == "seed 7":
        return path  # the committed trace
    if path.exists() and not again:
        recorded = json.loads(gzip.decompress(path.read_bytes())).get("variant", {})
        if recorded != expected_variant(seed, param):
            raise SystemExit(f"{path}: records {recorded}; run with --recapture")
        return path
    path.parent.mkdir(parents=True, exist_ok=True)
    cmd = [sys.executable, str(CAPTURE), str(REPO / "capture" / "scenarios" / f"{trace}.sql")]
    cmd += ["--no-rows", "--max-pages", str(TRACES[trace]), "-o", str(path)]
    cmd += ["--seed", str(seed)] if seed is not None else []
    cmd += ["--param", param] if param else []
    print(f"capturing {path.relative_to(REPO)}", flush=True)
    subprocess.run(cmd, check=True, capture_output=True)
    return path


# ---------- measuring ----------


def measure(sim: Path, path: Path, work: Path) -> Measured:
    """Every policy at an eighth, a quarter and half of the database."""
    pages = json.loads(gzip.decompress(path.read_bytes()))["page_count"]
    pools = {pool: round(pages / d) for pool, d in POOLS.items()}
    out = work / path.name.removesuffix(".trace.json.gz")
    frames = ",".join(str(f) for f in pools.values())
    cmd = [str(sim), "--trace", str(path), "--policy", "all", "--frames", frames]
    cmd += ["--read-ahead", "off", "--out", str(out)]
    subprocess.run(cmd, check=True, capture_output=True)
    sweep = json.loads(next(out.glob("**/sweep.json")).read_text(encoding="utf-8"))
    by_frames = {f: pool for pool, f in pools.items()}
    points = {
        (by_frames[p["frames"]], p["policy"]): Point(p["disk_reads"], p["lru_frames"])
        for p in sweep["points"]
    }
    return Measured(pages, pools, points)


def reads_by_uses(path: Path, frames: int) -> dict[str, dict[str, int]]:
    """LRU's and InnoDB-style's disk reads, by how many queries use the page."""
    trace = json.loads(gzip.decompress(path.read_bytes()))
    uses: collections.Counter[int] = collections.Counter()
    for q in trace["queries"]:
        uses.update({a[1] for a in q["io"]})
    out = {}
    for policy in ("lru", "innodb"):
        by_page = model.run(path, policy, frames, "ssd", "off")["reads_by_page"]
        out[policy] = {
            name: sum(
                n for p, n in by_page.items() if lo <= uses[p] and (hi is None or uses[p] <= hi)
            )
            for lo, hi, name in USES
        }
    return out


# ---------- claims ----------


def pct(x: float) -> str:
    return f"{round(100 * x)}%"


def span(values: list[float], form=pct) -> str:
    lo, hi = form(min(values)), form(max(values))
    return lo if lo == hi else f"{lo.rstrip('%×')}–{hi}"


def times(x: float) -> str:
    return f"{x:.1f}×"


def claims(m: dict[tuple[str, str], Measured], uses: dict[str, dict]) -> tuple[str, list[str]]:
    """The summary, and every claim in it that the numbers contradict."""
    failed = []

    def need(ok: bool, what: str) -> None:
        if not ok:
            failed.append(what)

    labels = [label for label, _, _ in VARIANTS]
    scans = [m["reports", v].fewer("1/2", p) for v in labels for p in ("innodb", "pg")]
    scan_mem = [m["reports", v].lru_needs("1/2", p) for v in labels for p in ("innodb", "pg")]
    no_scans_pg = [
        m[t, v].fewer(pool, "pg") for t in ("oltp", "ranges") for v in labels for pool in POOLS
    ]
    need(
        min(scans) >= 0.15,
        "on reports at 1/2, InnoDB- and PostgreSQL-style read 15%+ less than LRU",
    )
    need(
        max(abs(x) for x in no_scans_pg) <= 0.03,
        "without full scans, PostgreSQL-style is within 3% of LRU",
    )

    inno = [m[t, v].fewer("1/4", "innodb") for t in ("oltp", "ranges") for v in labels]
    need(min(inno) >= 0.03, "without full scans, InnoDB-style reads 3%+ less than LRU at 1/4")
    warm_share = []
    for t, r in uses.items():
        net = sum(r["lru"].values()) - sum(r["innodb"].values())
        warm = sum(r["lru"][n] - r["innodb"][n] for n in ("10–99", "100+"))
        need(
            net > 0 and warm >= 2 / 3 * net,
            f"{t}: most of InnoDB-style's saving is on pages 10+ queries use",
        )
        warm_share.append(min(1.0, warm / net) if net > 0 else 0.0)

    room = [m[t, v].fewer("1/4", "opt") for t in TRACES for v in labels]
    need(min(room) >= 0.3, "Optimal reads 30%+ less than LRU at 1/4 on every trace")

    pg_gap = pct(max(abs(x) for x in no_scans_pg))
    summary = "\n".join(
        [
            f"1. **Full scans are where the policies differ most.** On `reports` with half the "
            f"database in memory, InnoDB-style and PostgreSQL-style read {span(scans)} fewer "
            f"pages than LRU, and LRU needs {span(scan_mem, times)} the memory to match. "
            f"Without the full scans (`oltp` is the same queries minus the reports, `ranges` has "
            f"only short ones), PostgreSQL-style is within {pg_gap} of LRU.",
            f"2. **InnoDB-style helps without full scans too:** {span(inno)} fewer reads than LRU "
            f"with a quarter of the database in memory, on `oltp` and `ranges`. "
            f"{span(warm_share)} of the saving is on pages used by 10 or more queries. A page "
            f"enters InnoDB-style's list partway down, at the head of its old sublist, and moves "
            f"up only when a later query uses it, so pages used a few times don't push out pages "
            f"used often, as they do under LRU.",
            f"3. Optimal reads {span(room)} less than LRU with a quarter of the database in "
            f"memory, so most of the possible saving is still unclaimed.",
        ]
    )
    return summary, failed


# ---------- tables ----------


def results_table(m: dict[tuple[str, str], Measured]) -> str:
    """One row per trace and pool: the low and high across the variants."""
    head = "| Trace | Pool | " + " | ".join(NAMES[p] for p in SHOWN) + " | LRU needs |"
    rows = [head, "|" + "---|" * (len(SHOWN) + 3)]
    for trace in TRACES:
        runs = [r for (t, _), r in m.items() if t == trace]
        for pool in POOLS:
            cells = [span([r.fewer(pool, p) for r in runs]) for p in SHOWN]
            # the better policy reads less, so LRU needs more memory to match it
            better = [max(r.lru_needs(pool, p) for p in ("innodb", "pg")) for r in runs]
            rows.append(
                f"| {trace} | {pool} | " + " | ".join(cells) + f" | {span(better, times)} |"
            )
    return "\n".join(rows)


def uses_table(uses: dict[str, dict[str, dict[str, int]]]) -> str:
    rows = [
        "| Trace | Queries using the page | LRU | InnoDB-style | Change |",
        "|---|---|---|---|---|",
    ]
    for trace, r in uses.items():
        for _, _, name in USES:
            a, b = r["lru"][name], r["innodb"][name]
            change = f"{(b - a) / a:+.0%}" if a else "–"
            rows.append(f"| {trace} | {name} | {a:,} | {b:,} | {change} |")
    return "\n".join(rows)


# ---------- the document ----------

BLOCK = re.compile(r"(<!-- findings:(\w+) -->\n)(.*?)(<!-- /findings:\2 -->)", re.S)


def fill(doc: str, blocks: dict[str, str]) -> str:
    seen = set()

    def put(m: re.Match) -> str:
        seen.add(m[2])
        return m[1] + blocks[m[2]] + "\n" + m[4]

    out = BLOCK.sub(put, doc)
    if missing := blocks.keys() - seen:
        raise SystemExit(f"FINDINGS.md has no block for {sorted(missing)}")
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument(
        "--check", action="store_true", help="fail if FINDINGS.md is stale; change nothing"
    )
    ap.add_argument("--recapture", action="store_true", help="capture the variants again first")
    args = ap.parse_args()
    sim = compare.build()
    m: dict[tuple[str, str], Measured] = {}
    with tempfile.TemporaryDirectory() as tmp:
        for trace in TRACES:
            for label, seed, param in VARIANTS:
                path = capture(trace, label, seed, param, args.recapture)
                m[trace, label] = measure(sim, path, Path(tmp))
    uses = {
        t: reads_by_uses(trace_path(t, "seed 7"), m[t, "seed 7"].pools["1/4"])
        for t in ("oltp", "ranges")
    }
    summary, failed = claims(m, uses)
    blocks = {"summary": summary, "results": results_table(m), "uses": uses_table(uses)}
    doc = DOC.read_text(encoding="utf-8")
    new = fill(doc, blocks)
    for f in failed:
        print("CLAIM FAILS:", f)
    if args.check:
        stale = new != doc
        print(
            "FINDINGS.md is out of date: run verify/findings.py"
            if stale
            else "FINDINGS.md is up to date"
        )
        return 1 if failed or stale else 0
    DOC.write_text(new, encoding="utf-8")
    print(f"FINDINGS.md written; {len(failed)} claim(s) fail")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
