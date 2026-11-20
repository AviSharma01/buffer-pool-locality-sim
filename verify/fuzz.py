"""Random traces through the engine and the independent model (model.py).

The shipped traces exercise few shapes. These are small random databases:
tables laid out in runs of pages with free pages between them, queries that
walk forward, jump or write, and random scans; every policy at several pool
sizes, both drives, read-ahead auto, off and 3. A disagreement or an engine
crash is a bug in one of the two. This is how the 3.5a review found the
PostgreSQL ring crash and the LRU read-ahead order.

    python verify/fuzz.py                   # 150 traces, seed 1, ~1 min
    python verify/fuzz.py --seed 7 --traces 500
    python verify/fuzz.py --keep bad/       # save failing traces there

Optimal is compared on hits only (see compare.py).
"""

from __future__ import annotations

import argparse
import json
import random
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import compare  # noqa: E402

OBJECTS = ["a", "b", "c"]


def random_trace(rng: random.Random, n: int) -> dict:
    """A version 2 trace: page 1 is the schema; each later page continues the
    previous page's object with probability 0.6, so objects come in runs."""
    page_count = rng.randint(6, 40)
    pages = [{"page": 1, "object": "sqlite_schema", "kind": "leaf"}]
    for p in range(2, page_count + 1):
        prev = pages[-1]["object"]
        if prev in OBJECTS and rng.random() < 0.6:
            obj = prev
        else:
            obj = rng.choice([*OBJECTS, None]) if rng.random() < 0.9 else None
        pages.append({"page": p, "object": obj, "kind": "leaf" if obj else "free"})
    queries = []
    for q in range(rng.randint(1, 8)):
        io, page = [], rng.randint(1, page_count)
        for _ in range(rng.randint(1, 30)):
            r = rng.random()
            if r < 0.5:
                page = page + 1 if page < page_count else 1
            elif r < 0.8:
                page = rng.randint(1, page_count)
            io.append(["w" if rng.random() < 0.15 else "r", page])
        scans = rng.sample(OBJECTS, rng.randint(1, 2)) if rng.random() < 0.4 else []
        queries.append({"title": f"q{q}", "sql": "SELECT 1", "plan": [], "scans": scans, "io": io})
    return {
        "format": "bpsim-trace",
        "version": 2,
        "scenario": f"fuzz{n}",
        "sqlite_version": "random",
        "page_size": 4096,
        "page_count": page_count,
        "objects": [{"name": o, "type": "table", "table": o} for o in ["sqlite_schema", *OBJECTS]],
        "pages": pages,
        "queries": queries,
        "rows": {},
        "notes": [],
    }


def fuzz(seed: int, count: int, keep: Path | None = None, sim: Path | None = None) -> dict:
    """Returns counts of runs, mismatching and crashing configurations, and
    the first few problems."""
    sim = sim or compare.build()
    rng = random.Random(seed)
    result = {"runs": 0, "mismatches": 0, "crashes": 0, "problems": []}
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        for n in range(count):
            trace = random_trace(rng, n)
            path = work / f"fuzz{n}.trace.json"
            path.write_text(json.dumps(trace), encoding="utf-8")
            bad = False
            for frames in sorted({1, 2, 4, 5, 8, 12, min(16, trace["page_count"])}):
                if frames > trace["page_count"]:
                    continue
                for read_ahead in ["auto", "off", "3"]:
                    for storage in ["ssd", "hdd"]:
                        out = work / "out"
                        shutil.rmtree(out, ignore_errors=True)
                        args = ["--trace", str(path), "--policy", "all", "--frames", str(frames)]
                        args += ["--read-ahead", read_ahead, "--storage", storage]
                        done = subprocess.run(
                            [str(sim), *args, "--out", str(out)],
                            capture_output=True,
                            text=True,
                            check=False,
                        )
                        what = f"seed {seed} trace {n}, {frames} frames, {storage}, {read_ahead}"
                        if done.returncode:
                            result["crashes"] += 1
                            result["problems"].append(f"{what}: {done.stderr.strip()[-200:]}")
                            bad = True
                            continue
                        run_file = next(out.glob("*/compare/*/run.json"))
                        found = compare.check_run(run_file, path, frames, storage, read_ahead)
                        result["runs"] += len(compare.model.POLICIES) + 1
                        if found:
                            result["mismatches"] += 1
                            result["problems"] += found[:2]
                            bad = True
            if bad and keep:
                keep.mkdir(parents=True, exist_ok=True)
                shutil.copy(path, keep / path.name)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--traces", type=int, default=150)
    parser.add_argument("--keep", type=Path, help="save each failing trace in this folder")
    a = parser.parse_args()
    r = fuzz(a.seed, a.traces, a.keep)
    for p in r["problems"][:20]:
        print("PROBLEM", p)
    print(
        f"{r['runs']} runs on {a.traces} random traces (seed {a.seed}): "
        f"{r['mismatches']} configurations disagree, {r['crashes']} crash"
    )
    return 1 if r["mismatches"] or r["crashes"] else 0


if __name__ == "__main__":
    sys.exit(main())
