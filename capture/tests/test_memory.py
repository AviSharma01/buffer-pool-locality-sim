"""Memory guards: SQL from elsewhere must not exhaust memory (3.5b).

Before these guards, one 900 MB value peaked capture at 1.7 GB and succeeded;
a query streaming small rows grew to 1.1 GB before its timeout."""

from __future__ import annotations

import tracemalloc
from pathlib import Path

import apsw
import pytest

import capture as cap
from capture import capture, parse_scenario

TABLE = "-- setup\nCREATE TABLE t(x); INSERT INTO t VALUES (1);\n"


def run(tmp_path: Path, sql: str) -> dict:
    return capture(parse_scenario(f"{TABLE}-- query: q\n{sql};\n", "mem"), tmp_path)


def test_a_huge_value_is_refused_without_building_it(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="larger than 10 MB"):
        run(tmp_path, "SELECT randomblob(900000000), x FROM t")


def test_result_rows_are_not_kept(tmp_path: Path) -> None:
    sql = (
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300000) "
        "SELECT i, 'some text', x FROM n, t"
    )
    tracemalloc.start()
    try:
        run(tmp_path, sql)
        _, peak = tracemalloc.get_traced_memory()
    finally:
        tracemalloc.stop()
    # Kept as a list, 300,000 rows take over 30 MB of Python objects.
    assert peak < 10_000_000, f"{peak / 1e6:.0f} MB"


def test_sqlite_memory_is_capped(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(cap, "HEAP_LIMIT", 2 * 2**20)
    sql = (
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200000) "
        "SELECT group_concat(i) FROM n"
    )
    with pytest.raises(ValueError, match="more than 2 MB of memory"):
        run(tmp_path, sql)


def test_the_cap_is_lifted_afterwards(tmp_path: Path) -> None:
    before = apsw.hard_heap_limit(-1)
    run(tmp_path, "SELECT x FROM t")
    assert apsw.hard_heap_limit(-1) == before
