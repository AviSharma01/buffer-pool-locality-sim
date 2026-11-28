"""Workloads: queries made from templates with seeded random parameters."""

from __future__ import annotations

import collections
import gzip
import json
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
        (H + P + "nurand(1..9)\n" + ONE, "nurand takes (A, a..b)"),
        (H + P + "nurand(2.5, 1..9)\n" + ONE, "A a whole number"),
        (H + P + "nurand(-1, 1..9)\n" + ONE, "A a whole number"),
        (H + P + "nurand(\u00b2, 1..9)\n" + ONE, "A a whole number"),
        (H + P + "nurand(0, 1..9)\n" + ONE, "from 1 to 65,535"),
        (H + P + "nurand(70000, 1..9)\n" + ONE, "from 1 to 65,535"),
        (H + P + "nurand(255, 9..1)\n" + ONE, "is empty"),
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


def test_nurand_is_tpc_cs_formula() -> None:
    """NURand(A, x, y) = (((random(0, A) | random(x, y)) + C) % (y - x + 1)) + x,
    with C fixed for the run (TPC-C 2.1.6)."""
    param = wl(f"{H}-- param x = nurand(1023, 1..3000)\n{ONE}").params["x"]
    c = __import__("random").Random("1:x:nurand").randint(0, 1023)
    rng, mirror = __import__("random").Random(5), __import__("random").Random(5)
    for _ in range(2000):
        got = int(param.draw(rng, 1))
        want = (((mirror.randint(0, 1023) | mirror.randint(1, 3000)) + c) % 3000) + 1
        assert got == want


def test_nurand_is_skewed_but_covers_its_range() -> None:
    seen = collections.Counter(int(v) for v in draws("nurand(1023, 1..3000)", 60_000))
    assert min(seen) >= 1 and max(seen) <= 3000
    assert len(seen) > 2400, "most values come up"
    top = sum(n for _, n in seen.most_common(300))
    assert top > 0.5 * 60_000, "the hottest tenth of the values gets over half the draws"
    assert draws("nurand(8191, 1..100000)", 50) == draws("nurand(8191, 1..100000)", 50)
    assert draws("nurand(8191, 1..100000)", 50, seed=2) != draws("nurand(8191, 1..100000)", 50)


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


@pytest.mark.parametrize(
    "name", ["oltp", "reports", "ranges", "reports-big", "warehouse", "analytics"]
)
def test_shipped_workloads_parse_and_generate(name: str) -> None:
    text = (SCENARIOS / f"{name}.sql").read_text(encoding="utf-8")
    queries = parse_scenario(text, name).queries
    declared = int(text.split("-- workload: ")[1].split(" queries")[0])
    assert len(queries) == declared


def ids(queries: list[tuple[str, str]]) -> list[int]:
    return [int(m[1]) for _, sql in queries if (m := re.search(r"id = (-?\d+)", sql))]


def test_a_parameter_can_be_replaced_for_a_variant() -> None:
    text = SETUP + GOOD
    before = ids(parse_scenario(text, "w").queries)
    after = parse_scenario(text, "w", params=["id = uniform(5..6)"])
    assert set(ids(after.queries)) == {5, 6}
    assert set(before) != {5, 6}
    assert after.variant == {"params": ["id = uniform(5..6)"]}
    assert parse_scenario(text, "w").variant == {}
    assert parse_scenario(text, "w", seed=9).variant == {"seed": 9}


@pytest.mark.parametrize(
    ("spec", "error"),
    [
        ("nobody=uniform(1..2)", "--param nobody: the workload has no such parameter (it has id)"),
        ("id", "expected name=dist(args)"),
        ("id=zipf(0, 1..10)", "--param id: zipf's skew must be between 0 and 5"),
        ("id=uniform(1..9999999)", "--param id: a range may span at most"),
        ("id=shell(1..2)", "--param id: unknown distribution 'shell'"),
        ("id=choice('a'); DROP TABLE t; --)", "--param id: choice values are numbers or"),
    ],
)
def test_a_replacement_is_checked_like_a_declared_parameter(spec: str, error: str) -> None:
    with pytest.raises(ValueError, match=re.escape(error)):
        parse_scenario(SETUP + GOOD, "w", params=[spec])


def test_seed_and_param_need_a_workload() -> None:
    plain = SETUP + "-- query: q\nSELECT 1;"
    for kwargs in [{"seed": 3}, {"params": ["x=uniform(1..2)"]}]:
        with pytest.raises(ValueError, match="this scenario has none"):
            parse_scenario(plain, "p", **kwargs)


def test_a_variant_says_what_it_changed_and_a_plain_capture_says_nothing(tmp_path: Path) -> None:
    scenario = tmp_path / "w.sql"
    scenario.write_text(SETUP + GOOD.replace("2000 queries", "50 queries").replace("400", "25"))
    script = [sys.executable, str(ROOT / "capture" / "capture.py"), str(scenario)]
    plain, variant = tmp_path / "plain.json", tmp_path / "variant.json"
    done = subprocess.run([*script, "-o", str(plain)], capture_output=True, text=True)
    assert done.returncode == 0, done.stderr
    args = ["--seed", "4", "--param", "id=zipf(0.9,  1..400)", "-o", str(variant)]
    done = subprocess.run([*script, *args], capture_output=True, text=True)
    assert done.returncode == 0, done.stderr
    assert "variant" not in json.loads(plain.read_text())
    assert json.loads(variant.read_text())["variant"] == {
        "seed": 4,
        "params": ["id=zipf(0.9, 1..400)"],
    }
