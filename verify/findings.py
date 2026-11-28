#!/usr/bin/env python3
"""Measure the findings in FINDINGS.md and write their numbers into it and the README.

    python verify/findings.py               # measure, then rewrite FINDINGS.md's numbers
    python verify/findings.py --check       # measure; fail if FINDINGS.md is stale or a claim fails
    python verify/findings.py --recapture   # capture the variants again (a scenario changed)

The first run captures the trace variants, and the analytics trace, into
traces/local/variants (about 4 minutes); later runs reuse them, after checking
each says it is the variant it should be. Every number comes from the engine
(disk reads with read-ahead off, and the exact pool LRU needs, for every policy
and for each one with a rule changed), from the independent model in model.py
(the breakdown by page, and its check against the engine on analytics' long
joins), or from the traces (pages re-read within a query).
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
# Each document and the blocks findings.py writes in it.
DOCS = {
    REPO / "FINDINGS.md": ("summary", "results", "causes", "uses", "limit"),
    REPO / "README.md": ("headline",),
}
VARIANT_DIR = REPO / "traces" / "local" / "variants"
CAPTURE = REPO / "capture" / "capture.py"

# label -> (--seed, --param). Seed 7 with no changes is each scenario's own.
VARIANTS = {
    "seed 7": (None, None),
    "seed 11": (11, None),
    "seed 23": (23, None),
    "skew 0.9": (None, "user=zipf(0.9, 1..100000)"),
    "skew 1.3": (None, "user=zipf(1.3, 1..100000)"),
}
SEEDS = ("seed 7", "seed 11", "seed 23")


@dataclass(frozen=True)
class Source:
    """A scenario's capture options, as in its header, and its variants."""

    max_pages: int
    variants: tuple[str, ...] = tuple(VARIANTS)
    committed: bool = True  # seed 7 is in traces/; otherwise it is captured too
    max_io: int | None = None


TRACES = {
    "oltp": Source(20000),
    "reports": Source(20000),
    "ranges": Source(30000),
    "warehouse": Source(30000, SEEDS),  # the skew is TPC-C's NURand, not one parameter
    "analytics": Source(20000, SEEDS, committed=False, max_io=200000),
}
SCANS = ("reports", "analytics")  # full scans or long joins in most queries
NO_SCANS = ("oltp", "ranges", "warehouse")
POOLS = {"1/8": 8, "1/4": 4, "1/2": 2}  # pool as a share of the database
SHOWN = ["innodb", "pg", "opt"]
# Each policy with one rule changed, to see what its saving comes from.
OLD_SHARES = [5, 20, 50, 75, 95]  # InnoDB-style's old sublist, % (the default is 3/8)
CHANGED = ["innodb:promote=any", "pg:ring=off"] + [f"innodb:old={p}" for p in OLD_SHARES]
POLICIES = ["fifo", "lru", "clock", "pg", "innodb", "opt", *CHANGED]
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
    if label == "seed 7" and TRACES[trace].committed:
        return REPO / "traces" / f"{trace}.trace.json.gz"
    return VARIANT_DIR / f"{trace}-{label.replace(' ', '-')}.trace.json.gz"


def variants() -> list[tuple[str, str]]:
    """Every (trace, label) the findings measure."""
    return [(t, v) for t, src in TRACES.items() for v in src.variants]


def expected_variant(seed: int | None, param: str | None) -> dict:
    """What capture.py records in a trace for these changes (see --seed, --param)."""
    out: dict = {}
    if seed is not None:
        out["seed"] = seed
    if param:
        out["params"] = [" ".join(param.split())]
    return out


def capture(trace: str, label: str, again: bool = False) -> Path:
    seed, param = VARIANTS[label]
    src = TRACES[trace]
    path = trace_path(trace, label)
    if label == "seed 7" and src.committed:
        return path
    if path.exists() and not again:
        recorded = json.loads(gzip.decompress(path.read_bytes())).get("variant", {})
        if recorded != expected_variant(seed, param):
            raise SystemExit(f"{path}: records {recorded}; run with --recapture")
        return path
    path.parent.mkdir(parents=True, exist_ok=True)
    cmd = [sys.executable, str(CAPTURE), str(REPO / "capture" / "scenarios" / f"{trace}.sql")]
    cmd += ["--no-rows", "--max-pages", str(src.max_pages), "-o", str(path)]
    cmd += ["--max-io", str(src.max_io)] if src.max_io else []
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
    cmd = [str(sim), "--trace", str(path), "--policy", ",".join(POLICIES), "--frames", frames]
    cmd += ["--read-ahead", "off", "--out", str(out)]
    subprocess.run(cmd, check=True, capture_output=True)
    sweep = json.loads(next(out.glob("**/sweep.json")).read_text(encoding="utf-8"))
    by_frames = {f: pool for pool, f in pools.items()}
    points = {
        (by_frames[p["frames"]], p["policy"]): Point(p["disk_reads"], p["lru_frames"])
        for p in sweep["points"]
    }
    return Measured(pages, pools, points)


def rereads(path: Path) -> int:
    """How many pages some query reads more than once."""
    trace = json.loads(gzip.decompress(path.read_bytes()))
    pages = set()
    for q in trace["queries"]:
        reads = [a[1] for a in q["io"] if a[0] == "r"]
        pages |= {p for p, n in collections.Counter(reads).items() if n > 1}
    return len(pages)


JOIN_QUERIES = 20  # analytics queries the model replays, three long joins among them (~2 min)


def model_gap(sim: Path, work: Path) -> dict[str, int]:
    """Pages the independent model reads beyond the engine, per policy, on the
    first JOIN_QUERIES queries of analytics at an eighth of the database: long
    joins, which compare.py's traces don't have. (The whole trace takes the
    model hours.)"""
    full = json.loads(gzip.decompress(trace_path("analytics", "seed 7").read_bytes()))
    path = work / "analytics-start.trace.json.gz"
    path.write_bytes(
        gzip.compress(json.dumps({**full, "queries": full["queries"][:JOIN_QUERIES]}).encode())
    )
    start = measure(sim, path, work)
    frames = start.pools["1/8"]
    gap = {}
    for policy in ("innodb", "pg"):
        r = model.run(path, policy, frames, "ssd", "off")
        reads = r["next_page_reads"] + r["ahead_reads"] + r["jump_reads"]
        gap[policy] = reads - start.points["1/8", policy].reads
    return gap


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


def warm_text(share: dict[tuple[str, str], float]) -> str:
    """How much of InnoDB-style's saving is on pages 10+ queries use, across the
    traces. Over 100% means it reads the other pages more than LRU does, so the
    saving is all theirs."""
    (t, v), low = min(share.items(), key=lambda kv: kv[1])
    most = "all" if max(share.values()) >= 1 else pct(max(share.values()))
    return (
        f"pages used by 10 or more queries account for {pct(low)} to {most} of the saving "
        f"(the least on `{t}` at {v})."
    )


def times(x: float) -> str:
    return f"{x:.1f}×"


def claims(
    m: dict[tuple[str, str], Measured],
    uses: dict[tuple[str, str], dict],
    reread: dict[str, int],
    model_gap: dict[str, int],
) -> tuple[dict[str, str], list[str]]:
    """The summary, the README's headline and the limit line, and every claim in
    them that the numbers contradict. `model_gap` is, per policy, how many pages
    the independent model reads beyond the engine on analytics."""
    failed = []

    def need(ok: bool, what: str) -> None:
        if not ok:
            failed.append(what)

    def runs(traces: tuple[str, ...]) -> list[Measured]:
        return [r for (t, _), r in m.items() if t in traces]

    scans = [r.fewer("1/2", p) for r in runs(SCANS) for p in ("innodb", "pg")]
    scan_mem = [r.lru_needs("1/2", p) for r in runs(SCANS) for p in ("innodb", "pg")]
    no_scans_pg = [r.fewer(pool, "pg") for r in runs(NO_SCANS) for pool in POOLS]
    ringless = [abs(r.fewer("1/2", "pg:ring=off")) for r in runs(SCANS)]
    need(min(scans) >= 0.15, "with full scans at 1/2, InnoDB- and PostgreSQL-style read 15%+ less")
    need(
        max(abs(x) for x in no_scans_pg) <= 0.04,
        "without full scans, PostgreSQL-style is within 4%",
    )
    need(max(ringless) <= 0.05, "without its ring, PostgreSQL-style is within 5% of LRU at 1/2")

    inno = [r.fewer("1/4", "innodb") for r in runs(NO_SCANS)]
    need(min(inno) >= 0.025, "without full scans, InnoDB-style reads 2.5%+ less than LRU at 1/4")
    at_head = [r.fewer(pool, "innodb:old=95") for r in m.values() for pool in POOLS]
    need(
        max(at_head) <= 0.02,
        "with new pages entering at the head (old 95%), InnoDB-style saves at most 2%",
    )
    warm_share = {}
    for key, r in uses.items():
        net = sum(r["lru"].values()) - sum(r["innodb"].values())
        warm = sum(r["lru"][n] - r["innodb"][n] for n in ("10–99", "100+"))
        need(net > 0, f"{key[0]} {key[1]}: InnoDB-style saves reads at 1/4")
        warm_share[key] = warm / net if net > 0 else 0.0
    main_share = {k: x for k, x in warm_share.items() if k[0] in ("oltp", "ranges")}
    need(
        min(main_share.values()) >= 0.4,
        "on oltp and ranges, pages 10+ queries use carry 40%+ of InnoDB-style's saving",
    )
    small_share = [x for k, x in warm_share.items() if k[0] == "warehouse"]

    same = ("oltp", "reports", "ranges")
    need(
        all(
            r.points[pool, "innodb:promote=any"].reads == r.points[pool, "innodb"].reads
            for r in runs(same)
            for pool in POOLS
        ),
        "on oltp, reports and ranges, promoting on any re-use reads exactly the same",
    )
    near = [
        abs(r.points[pool, "innodb:promote=any"].reads - r.points[pool, "innodb"].reads)
        / r.points[pool, "lru"].reads
        for r in runs(("warehouse",))
        for pool in POOLS
    ]
    need(max(near) < 0.001, "on warehouse, the later-query rule changes under 0.1% of LRU's reads")
    joins = runs(("analytics",))
    rule = [r.fewer("1/4", "innodb") for r in joins]
    no_rule = [r.fewer("1/4", "innodb:promote=any") for r in joins]
    need(
        all(r.fewer("1/4", "innodb") > r.fewer("1/4", "innodb:promote=any") for r in joins),
        "on analytics at 1/4, the later-query rule saves reads",
    )
    half_rule = [r.fewer("1/2", "innodb") for r in joins]
    half_no_rule = [r.fewer("1/2", "innodb:promote=any") for r in joins]
    for policy, gap in model_gap.items():
        need(gap == 0, f"the independent model reads what the engine does on analytics ({policy})")

    room = [r.fewer("1/4", "opt") for r in m.values()]
    need(min(room) >= 0.2, "Optimal reads 20%+ less than LRU at 1/4 on every trace")

    pg_gap = pct(max(abs(x) for x in no_scans_pg))
    summary = "\n".join(
        [
            f"1. **Full scans are where the policies differ most.** On `reports` (an online "
            f"shop's queries plus periodic full-table reports) and `analytics` (decision-support "
            f"queries shaped like TPC-H's) with half the database in memory, InnoDB-style and "
            f"PostgreSQL-style read {span(scans)} fewer pages than LRU, and LRU needs "
            f"{span(scan_mem, times)} the memory to match. PostgreSQL-style's edge there is its "
            f"scan ring (without it, it reads within {pct(max(ringless))} of LRU), and "
            f"InnoDB-style's is where new pages enter (finding 2). Without full scans (`oltp` is "
            f"the shop's queries without the reports, `ranges` has only short ones, `warehouse` "
            f"is order processing shaped like TPC-C), PostgreSQL-style is within {pg_gap} of LRU.",
            f"2. **InnoDB-style helps without full scans too:** {span(inno)} fewer reads than LRU "
            f"with a quarter of the database in memory, on `oltp`, `ranges` and `warehouse`. The "
            f"cause is where a new page enters: partway down the list, at the head of its old "
            f"sublist, so a page that isn't used again leaves from there instead of pushing out "
            f"pages that are. With new pages entering at the head (old sublist 95%), it saves at "
            f"most {pct(max(at_head))} on any trace. On `oltp` and `ranges`, "
            f"{warm_text(main_share)} On `warehouse`, whose saving is smaller, "
            f"{span(small_share)} of it is.",
            f"3. Optimal reads {span(room)} less than LRU with a quarter of the database in "
            f"memory, so most of the possible saving is still unclaimed.",
        ]
    )
    headline = (
        f"With full-table scans in the mix (periodic reports, or decision-support queries "
        f"shaped like TPC-H's), InnoDB-style and PostgreSQL-style eviction read {span(scans)} "
        f"fewer pages from disk than LRU with half the database in memory, and LRU needs "
        f"{span(scan_mem, times)} the memory to match. Without full scans, including order "
        f"processing shaped like TPC-C, PostgreSQL-style is within {pg_gap} of LRU."
    )
    most = max(reread[t] for t in same)
    limit = (
        f"- InnoDB-style's rule that only a later query moves an old page up changes no read "
        f"on `oltp`, `reports` and `ranges` (`innodb:promote=any` reads the same), and under "
        f"0.1% of LRU's reads on `warehouse`: within a query, SQLite re-reads at most {most} "
        f"pages on the first three and {reread['warehouse']} on `warehouse`. On `analytics`, "
        f"whose joins re-read "
        f"{reread['analytics']:,} pages within a query, the rule helps with a quarter of the "
        f"database in memory ({span(rule)} fewer reads than LRU, {span(no_rule)} without it) "
        f"but not consistently with half ({span(half_rule)}, {span(half_no_rule)} without "
        f"it). Neither is MySQL's `innodb_old_blocks_time`, which counts a second, not a query."
    )
    return {"summary": summary, "headline": headline, "limit": limit}, failed


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


CAUSES = [
    ("innodb:old=5", "InnoDB-style, old sublist 5%"),
    ("innodb:old=20", "InnoDB-style, old sublist 20%"),
    ("innodb", "InnoDB-style (old sublist 3/8)"),
    ("innodb:old=50", "InnoDB-style, old sublist 50%"),
    ("innodb:old=75", "InnoDB-style, old sublist 75%"),
    ("innodb:old=95", "InnoDB-style, old sublist 95%"),
    ("innodb:promote=any", "InnoDB-style, promoted on any re-use"),
    ("pg", "PostgreSQL-style"),
    ("pg:ring=off", "PostgreSQL-style without its ring"),
]
CAUSE_POOLS = [
    ("oltp", "1/4"),
    ("ranges", "1/4"),
    ("warehouse", "1/4"),
    ("reports", "1/2"),
    ("analytics", "1/2"),
]


def causes_table(m: dict[tuple[str, str], Measured]) -> str:
    """Each changed policy's saving over LRU, low to high across the variants."""
    head = "| Policy | " + " | ".join(f"{t} ({p})" for t, p in CAUSE_POOLS) + " |"
    rows = [head, "|" + "---|" * (len(CAUSE_POOLS) + 1)]
    for policy, name in CAUSES:
        cells = [
            span([r.fewer(pool, policy) for (t, _), r in m.items() if t == trace])
            for trace, pool in CAUSE_POOLS
        ]
        rows.append(f"| {name} | " + " | ".join(cells) + " |")
    return "\n".join(rows)


def signed_span(values: list[float]) -> str:
    lo, hi = (f"{x:+.0%}" for x in (min(values), max(values)))
    return lo if lo == hi else f"{lo} to {hi}"


def uses_table(uses: dict[tuple[str, str], dict[str, dict[str, int]]]) -> str:
    """InnoDB-style's change in reads by page use, low to high across the variants."""
    rows = ["| Trace | Queries using the page | InnoDB-style vs LRU |", "|---|---|---|"]
    for trace in dict.fromkeys(t for t, _ in uses):
        for _, _, name in USES:
            runs = [r for (t, _), r in uses.items() if t == trace]
            change = [(r["innodb"][name] - r["lru"][name]) / r["lru"][name] for r in runs]
            rows.append(f"| {trace} | {name} | {signed_span(change)} |")
    return "\n".join(rows)


# ---------- the document ----------

BLOCK = re.compile(r"(<!-- findings:(\w+) -->\n)(.*?)(<!-- /findings:\2 -->)", re.S)


def fill(doc: str, blocks: dict[str, str], name: str) -> str:
    seen = set()

    def put(m: re.Match) -> str:
        if m[2] not in blocks:
            raise SystemExit(f"{name} has a block findings.py doesn't write: {m[2]}")
        seen.add(m[2])
        return m[1] + blocks[m[2]] + "\n" + m[4]

    out = BLOCK.sub(put, doc)
    if missing := blocks.keys() - seen:
        raise SystemExit(f"{name} has no block for {sorted(missing)}")
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument(
        "--check",
        action="store_true",
        help="fail if FINDINGS.md or README.md is stale; change nothing",
    )
    ap.add_argument("--recapture", action="store_true", help="capture the variants again first")
    args = ap.parse_args()
    sim = compare.build()
    m: dict[tuple[str, str], Measured] = {}
    with tempfile.TemporaryDirectory() as tmp:
        for trace, label in variants():
            m[trace, label] = measure(sim, capture(trace, label, args.recapture), Path(tmp))
        gap = model_gap(sim, Path(tmp))
    uses = {
        (t, v): reads_by_uses(trace_path(t, v), m[t, v].pools["1/4"])
        for t, v in variants()
        if t in NO_SCANS
    }
    reread = {t: max(rereads(trace_path(t, v)) for v in TRACES[t].variants) for t in TRACES}
    texts, failed = claims(m, uses, reread, gap)
    blocks = {
        **texts,
        "results": results_table(m),
        "causes": causes_table(m),
        "uses": uses_table(uses),
    }
    for f in failed:
        print("CLAIM FAILS:", f)
    stale = []
    for doc, names in DOCS.items():
        old = doc.read_text(encoding="utf-8")
        new = fill(old, {n: blocks[n] for n in names}, doc.name)
        if new != old:
            stale.append(doc.name)
            if not args.check:
                doc.write_text(new, encoding="utf-8")
    if args.check:
        print(
            f"{', '.join(stale)} out of date: run verify/findings.py"
            if stale
            else "FINDINGS.md and README.md are up to date"
        )
        return 1 if failed or stale else 0
    print(f"written: {', '.join(stale) or 'nothing changed'}; {len(failed)} claim(s) fail")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
