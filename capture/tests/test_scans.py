"""Which tables and indexes a query reads end to end, from its bytecode.

The expected answers were measured, not assumed: each case was run on a
populated file through the trace VFS, and an object counts as scanned when the
query read every one of its pages (3.5b)."""

from __future__ import annotations

from pathlib import Path

import apsw
import pytest

from capture import _scans, capture, parse_scenario

SCHEMA = """
CREATE TABLE orders(id INTEGER PRIMARY KEY, customer INT, total REAL, note TEXT, day TEXT);
CREATE INDEX orders_customer ON orders(customer);
CREATE INDEX orders_day_total ON orders(day, total);
CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT UNIQUE, name TEXT);
CREATE TABLE logs(ts INT, msg TEXT);
CREATE TABLE kv(k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID;
"""

FILL = """
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
INSERT INTO orders(customer, total, note, day)
  SELECT i % 97, i * 1.5, printf('%.40c%d', 'n', i), printf('2026-%02d', i % 12 + 1) FROM n;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
INSERT INTO users(email, name) SELECT printf('u%d@x', i), printf('%.40c%d', 'a', i) FROM n;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
INSERT INTO logs SELECT i, printf('%.60c%d', 'm', i) FROM n;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
INSERT INTO kv SELECT printf('k%05d', i), printf('%.30c', 'v') FROM n;
"""

# (query, objects it reads end to end)
CASES = [
    ("SELECT * FROM orders", ["orders"]),
    # The plan says "SCAN o": only the root page says which table that is.
    ("SELECT * FROM orders AS o WHERE o.note LIKE '%x%'", ["orders"]),
    ("SELECT * FROM users u JOIN orders o ON o.customer = u.id", ["orders"]),
    ("SELECT customer FROM orders", ["orders_customer"]),
    ("SELECT count(*) FROM orders", ["orders_customer"]),
    ("SELECT * FROM orders ORDER BY id DESC", ["orders"]),
    ("UPDATE orders SET total = total + 1", ["orders"]),
    # Lookups and ranges are searches.
    ("SELECT * FROM orders WHERE id = 7", []),
    ("SELECT * FROM orders WHERE customer = 3", []),
    ("SELECT * FROM kv WHERE k = 'k00007'", []),
    # min()/max() on an index edge read one entry: no scan.
    ("SELECT max(id) FROM orders", []),
    ("SELECT max(customer) FROM orders", []),
    ("SELECT min(customer) FROM orders", []),
    # With a filter, min() walks the index only until the first match.
    ("SELECT min(customer) FROM orders WHERE note LIKE '%5%'", []),
    # The plan calls these searches, but nothing can answer them without
    # reading everything.
    ("SELECT max(msg) FROM logs", ["logs"]),
    ("SELECT min(total) FROM orders", ["orders_day_total"]),
    ("SELECT min(k) FROM kv WHERE v = 'x'", ["kv"]),
    # Building an automatic index reads the inner table end to end too.
    ("SELECT * FROM logs JOIN users ON logs.msg = users.name", ["logs", "users"]),
]


@pytest.fixture(scope="module")
def conn() -> apsw.Connection:
    c = apsw.Connection(":memory:")
    c.execute(SCHEMA)
    return c


@pytest.mark.parametrize(("sql", "expected"), CASES)
def test_scans_come_from_the_bytecode(conn: apsw.Connection, sql: str, expected: list[str]) -> None:
    assert sorted(_scans(conn, sql, "test", 10)) == sorted(expected)


def test_every_listed_scan_reads_its_object_in_full(tmp_path: Path) -> None:
    """End to end: capture the cases on a populated file and check each listed
    object's pages were all read, and no other sizeable object's were."""
    queries = "\n".join(
        f"-- query: q{i}\n{sql};"
        for i, (sql, _) in enumerate(CASES)
        if not sql.startswith("UPDATE")
    )
    scenario = parse_scenario(f"-- setup\n{SCHEMA}\n{FILL}\n{queries}\n", "scans")
    trace = capture(scenario, tmp_path)
    owner = {p["page"]: p["object"] for p in trace["pages"] if p["object"]}
    size: dict[str, int] = {}
    for o in owner.values():
        size[o] = size.get(o, 0) + 1
    for q in trace["queries"]:
        read: dict[str, set[int]] = {}
        for _, page in q["io"]:
            if page in owner:
                read.setdefault(owner[page], set()).add(page)
        full = {o for o, pages in read.items() if size[o] > 2 and len(pages) == size[o]}
        assert set(q["scans"]) == full, (q["sql"], q["scans"], sorted(full))
