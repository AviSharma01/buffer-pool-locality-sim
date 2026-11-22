"""Workloads: queries made from templates with seeded random parameters (4a)."""

from __future__ import annotations

import collections
import gzip
import re
import subprocess
import sys
from pathlib import Path

import pytest

import workload
from capture import _check_single_statement, capture, parse_scenario

ROOT = Path(__file__).resolve().parents[2]
SCENARIOS = ROOT / "capture" / "scenarios"

SETUP = (
    "-- setup\nCREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);\nINSERT INTO t VALUES (1, 'a');\n"
)


def load(body: str, seed: int | None = None) -> list[tuple[str, str]]:
    return parse_scenario(SETUP + body, "w", seed=seed).queries


def wl(body: str) -> workload.Workload:
    text = body.splitlines()
    return workload.parse(list(enumerate(text, start=1)), _check_single_statement)


GOOD = """-- workload: 2000 queries, seed 3
-- param id = zipf(1.1, 1..500)
-- 70% Point
SELECT v FROM t WHERE id = {id};
-- 30% Range
SELECT v FROM t WHERE id BETWEEN {id} AND {id} + 5;
-- every 400 Scan
SELECT count(*) FROM t;
"""


H = "-- workload: 10 queries, seed 1\n"
P = "-- param x = "
ONE = "-- 100% A\nSELECT {x};"


@pytest.mark.parametrize(
    ("body", "error"),
    [
        (
            "-- workload: 10 queries\n-- 100% A\nSELECT 1;",
            "expected '-- workload: N queries, seed S'",
        ),
        ("-- workload: 0 queries, seed 1\n-- 100% A\nSELECT 1;", "1 to 1,000,000"),
        (H + "-- 60% A\nSELECT 1;\n-- 30% B\nSELECT 2;", "add up to 90"),
        (H + "-- every 5 A\nSELECT 1;", "at least one 'N%' template"),
        (H + "-- every 50 A\nSELECT 1;\n-- 100% B\nSELECT 2;", "between 1 and 10"),
        (H + ONE, "uses {x}, which is not declared"),
        (H + P + "uniform(1..9)\n-- 100% A\nSELECT 1;", "declared but no template"),
        (H + "-- 100% A\nSELECT 1;\n" + P + "uniform(1..9)", "before the first template"),
        (H + P + "normal(1..9)\n" + ONE, "unknown distribution"),
        (H + P + "uniform(9..1)\n" + ONE, "is empty"),
        (H + P + "zipf(9, 1..9)\n" + ONE, "between 0 and 5"),
        (H + P + "zipf(1, 1..9, old)\n" + ONE, "(s, a..b, recent)"),
        (H + P + "choice(a, 'b')\n" + ONE, "not 'a'"),
        (H + P + "choice('a)\n" + ONE, "unterminated"),
        (H + P + "uniform(1..99999999)\n" + ONE, "at most"),
        (H + "-- 100 % A\nSELECT 1;", "'--' lines are"),
        (H + "SELECT 1;\n-- 100% A\nSELECT 1;", "SQL before the first template"),
        (H + "-- 100% A\nSELECT 1; SELECT 2;", "single statement"),
        (H + "-- 100% A\n", "is empty"),
    ],
)
def test_mistakes_are_refused_with_the_line(body: str, error: str) -> None:
    with pytest.raises(ValueError, match=re.escape(error)):
        wl(body)


def test_a_workload_needs_the_setup_first() -> None:
    with pytest.raises(ValueError, match="before '-- setup'"):
        parse_scenario("-- workload: 1 query, seed 1\n-- 100% A\nSELECT 1;\n", "w")


def test_the_same_seed_gives_the_same_queries() -> None:
    a, b = load(GOOD), load(GOOD)
    assert a == b and len(a) == 2000
    assert load(GOOD, seed=4) != a, "another seed, other queries"
    assert load(GOOD, seed=3) == a, "--seed with the file's own seed changes nothing"


def test_every_n_runs_on_schedule() -> None:
    qs = load(GOOD)
    at = [i + 1 for i, (title, _) in enumerate(qs) if title == "Scan"]
    assert at == [400, 800, 1200, 1600, 2000]
    shares = collections.Counter(t for t, _ in qs if t != "Scan")
    assert abs(shares["Point"] / (2000 - 5) - 0.7) < 0.04


def test_one_parameter_gets_one_value_per_query() -> None:
    for title, sql in load(GOOD):
        if title == "Range":
            a, b = sql.split("BETWEEN ")[1].split(" AND ")
            assert b.startswith(a + " + 5")


def draws(spec: str, n: int, seed: int = 1) -> list[str]:
    w = wl(f"-- workload: {n} queries, seed {seed}\n-- param x = {spec}\n-- 100% A\nSELECT {{x}};")
    return [sql.removeprefix("SELECT ").rstrip(";") for _, sql in workload.generate(w)]


def test_zipf_matches_its_theory() -> None:
    n, s, k = 1000, 1.1, 100_000
    seen = collections.Counter(draws(f"zipf({s}, 1..{n}, recent)", k))
    h = sum(1 / r**s for r in range(1, n + 1))
    for rank in (1, 2, 10):
        want = k / rank**s / h
        got = seen[str(n - rank + 1)]  # recent: hottest at the top of the range
        assert abs(got - want) < 4 * want**0.5 + 0.02 * want, (rank, got, want)
    assert all(1 <= int(v) <= n for v in seen)


def test_zipf_spreads_hot_values_but_keeps_them_in_range() -> None:
    seen = collections.Counter(draws("zipf(1.1, 1..1000)", 20_000))
    hottest = [int(v) for v, _ in seen.most_common(5)]
    assert all(1 <= int(v) <= 1000 for v in seen)
    assert hottest != [1, 2, 3, 4, 5] and hottest != [1000, 999, 998, 997, 996]
    assert draws("zipf(1.1, 1..1000)", 50) == draws("zipf(1.1, 1..1000)", 50), "the spread is fixed"


def test_uniform_choice_and_seq() -> None:
    u = [int(v) for v in draws("uniform(5..15)", 20_000)]
    assert min(u) == 5 and max(u) == 15 and abs(sum(u) / len(u) - 10) < 0.15
    c = collections.Counter(draws("choice(1, 2.5, 'x')", 9_000))
    assert set(c) == {"1", "2.5", "'x'"} and min(c.values()) > 2_700
    assert draws("seq(7..9)", 3) == ["7", "8", "9"]
    with pytest.raises(ValueError, match="ran out"):
        draws("seq(7..9)", 4)


def test_values_cannot_inject_sql(tmp_path: Path) -> None:
    hostile = "'x''); DROP TABLE t; --'"
    assert set(draws(f"choice({hostile})", 3)) == {"'x''); DROP TABLE t; --'"}
    body = (
        f"-- workload: 5 queries, seed 1\n-- param v = choice({hostile})\n"
        "-- 100% Find\nSELECT id FROM t WHERE v = {v};\n"
    )
    trace = capture(parse_scenario(SETUP + body, "w"), tmp_path)
    assert len(trace["queries"]) == 5
    assert any(o["name"] == "t" for o in trace["objects"]), "the table is still there"


def test_a_gzipped_capture_is_byte_identical(tmp_path: Path) -> None:
    scenario = tmp_path / "w.sql"
    scenario.write_text(
        SETUP + GOOD.replace("2000 queries", "300 queries").replace("every 400", "every 100")
    )
    outs = []
    for k in range(2):
        out = tmp_path / f"w{k}.trace.json.gz"
        done = subprocess.run(
            [sys.executable, str(ROOT / "capture" / "capture.py"), str(scenario), "-o", str(out)],
            capture_output=True,
            text=True,
            check=False,
        )
        assert done.returncode == 0, done.stderr
        outs.append(out.read_bytes())
    assert outs[0] == outs[1]
    assert outs[0][4:8] == bytes(4), "no timestamp in the gzip header: same trace, same bytes"
    assert gzip.decompress(outs[0]).startswith(b"{")


@pytest.mark.parametrize("name", ["oltp", "reports", "ranges", "reports-big"])
def test_shipped_workloads_parse_and_generate(name: str) -> None:
    text = (SCENARIOS / f"{name}.sql").read_text(encoding="utf-8")
    queries = parse_scenario(text, name).queries
    declared = int(text.split("-- workload: ")[1].split(" queries")[0])
    assert len(queries) == declared
