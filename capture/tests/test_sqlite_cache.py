"""SQLite's own page cache as an outside check on the engine's LRU.

SQLite keeps recently used pages in an LRU cache of `cache_size` pages. Run a
scenario with `cache_size = N`, count the whole pages SQLite reads from the
file, and compare with the engine's LRU at N frames. This check was not
written from the engine's rules, so a shared misunderstanding cannot make both
agree.

It is close, not exact: SQLite pins the pages a b-tree descent is using (so
its cache briefly holds more than N), checks the file header with a 16-byte
read at every statement instead of reading page 1, and never reads pages it
writes without reading. The engine's page-1 misses and fresh pages are taken
out, and the rest must agree within a few reads.

That makes it a coarse check on these small traces: it catches a pool that
caches the wrong pages (evicting the most recent page fails it by 5 to 7
reads), but FIFO and LRU differ by less than the tolerance. The exact LRU
check is guarantees_hold_on_random_traces in sim/tests/engine.rs.

The workload traces carry the same measurement at real sizes, made when they
were captured (capture.py --sqlite-cache): hundreds of thousands of reads,
where the engine must land within a fraction of a percent.
"""

from __future__ import annotations

import argparse
import gzip
import json
import subprocess
from pathlib import Path

import pytest

import capture as cap

HERE = Path(__file__).parent.parent
TRACES = HERE.parent / "traces"
SIM = cap.REPO / "sim" / "target" / "release" / "sim"
TOLERANCE = 3


def built(scenario: cap.Scenario, tmp_path: Path) -> Path:
    """The scenario's database after setup, before any query."""
    tmp_path.mkdir(parents=True, exist_ok=True)
    db = tmp_path / "built.db"
    setup = cap._open(db)
    cap._restrict(setup, cap.Limits().max_pages)
    cap._run(setup, scenario.setup, "setup", cap.SETUP_SECONDS)
    setup.close()
    return db


def sqlite_reads(scenario: cap.Scenario, built: Path, work: Path, n: int) -> int:
    sql = [query for _, query in scenario.queries]
    return cap.sqlite_cache_reads(built, sql, n, 4096, work, cap.Limits())


def scenario(name: str) -> cap.Scenario:
    return cap.parse_scenario(
        (HERE / "scenarios" / f"{name}.sql").read_text(encoding="utf-8"), name
    )


def engine_reads(name: str, n: int, out: Path) -> int:
    """Engine LRU misses at n frames, less page-1 misses and fresh pages."""
    trace = TRACES / f"{name}.trace.json"
    # SQLite's own cache does not read ahead, so neither may the engine here.
    cmd = [
        str(SIM),
        "--trace",
        str(trace),
        "--policy",
        "lru",
        "--frames",
        str(n),
        "--read-ahead",
        "off",
    ]
    done = subprocess.run([*cmd, "--out", str(out)], capture_output=True, text=True, check=True)
    run_dir = Path(done.stdout.strip().splitlines()[-1].removeprefix("Run written to "))
    run = json.loads((run_dir / "run.json").read_text(encoding="utf-8"))["runs"][0]
    misses = [e for e in run["events"] if e["kind"] == "miss"]
    page_one = sum(e["page"] == 1 for e in misses)
    return len(misses) - page_one - run["summary"]["fresh_pages"]


@pytest.mark.skipif(not SIM.is_file(), reason="engine not built")
@pytest.mark.parametrize("name", ["shop", "checkout", "growth"])
def test_engine_lru_agrees_with_sqlites_own_cache(tmp_path: Path, name: str) -> None:
    sc = scenario(name)
    db = built(sc, tmp_path)
    for n in [4, 8, 16, 24]:
        ours = engine_reads(name, n, tmp_path / "runs")
        theirs = sqlite_reads(sc, db, tmp_path, n)
        assert abs(ours - theirs) <= TOLERANCE, (
            f"{name} at {n} pages: engine {ours}, SQLite {theirs}"
        )


def engine_disk_reads(trace: Path, n: int, out: Path) -> int:
    """Pages the engine's LRU reads from disk at n frames, read-ahead off."""
    cmd = [str(SIM), "--trace", str(trace), "--policy", "lru", "--frames", str(n)]
    cmd += ["--read-ahead", "off", "--no-viewer", "--out", str(out)]
    done = subprocess.run(cmd, capture_output=True, text=True, check=True)
    run_dir = Path(done.stdout.strip().splitlines()[-1].removeprefix("Run written to "))
    summary = json.loads((run_dir / "summary.json").read_text(encoding="utf-8"))
    return summary["misses"] - summary["fresh_pages"]


@pytest.mark.skipif(not SIM.is_file(), reason="engine not built")
@pytest.mark.parametrize("name", ["oltp", "reports", "ranges"])
def test_engine_lru_reads_what_sqlites_own_cache_read(tmp_path: Path, name: str) -> None:
    """Within 1% below 256 pages, where SQLite's pinning matters most (the
    worst seen is 0.9%), and within 0.2% from 256 pages (the worst seen is
    0.06%). Page 1 is left in: a dozen reads at most here."""
    path = TRACES / f"{name}.trace.json.gz"
    points = json.loads(gzip.decompress(path.read_bytes()))["sqlite_cache"]
    assert [p["pages"] for p in points] == [64, 128, 256, 512, 1024, 2048, 4096]
    for p in points:
        ours = engine_disk_reads(path, p["pages"], tmp_path)
        tolerance = 0.01 if p["pages"] < 256 else 0.002
        assert abs(ours - p["reads"]) <= tolerance * p["reads"], (
            f"{name} at {p['pages']} pages: engine {ours}, SQLite {p['reads']}"
        )


def test_capture_records_what_sqlites_own_cache_reads(tmp_path: Path) -> None:
    sc = scenario("shop")
    trace = cap.capture(sc, tmp_path, sqlite_cache=[16, 8])
    db = built(sc, tmp_path / "direct")
    direct = [sqlite_reads(sc, db, tmp_path, n) for n in (8, 16)]
    assert trace["sqlite_cache"] == [
        {"pages": 8, "reads": direct[0]},
        {"pages": 16, "reads": direct[1]},
    ]
    assert "sqlite_cache" not in cap.capture(sc, tmp_path)


def test_sqlite_cache_sizes_are_positive_and_different() -> None:
    assert cap._sizes("256,64") == [64, 256]
    for bad in ["0,64", "64,64", "sixty"]:
        with pytest.raises(argparse.ArgumentTypeError):
            cap._sizes(bad)
