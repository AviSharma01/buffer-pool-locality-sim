from __future__ import annotations

from pathlib import Path
from typing import Any

import apsw
import pytest

from capture import (
    PageReader,
    capture,
    decode_rows,
    dump,
    parse_scenario,
)

SHOP = Path(__file__).resolve().parents[1] / "scenarios" / "shop.sql"


@pytest.fixture(scope="module")
def shop(tmp_path_factory: pytest.TempPathFactory) -> tuple[dict[str, Any], Path]:
    workdir = tmp_path_factory.mktemp("shop")
    scenario = parse_scenario(SHOP.read_text(encoding="utf-8"), "shop")
    return capture(scenario, workdir), workdir / "shop.db"


def leaf_pages_of(trace: dict[str, Any], obj: str) -> list[int]:
    return [p["page"] for p in trace["pages"] if p["object"] == obj and p["kind"] == "leaf"]


def test_parse_scenario_splits_setup_and_queries() -> None:
    text = "-- comment\n-- setup\nCREATE TABLE t(x);\n-- query: First\nSELECT 1;\n"
    s = parse_scenario(text, "t")
    assert "CREATE TABLE" in s.setup
    assert s.queries == [("First", "SELECT 1;")]


@pytest.mark.parametrize(
    ("text", "message"),
    [
        ("SELECT 1;\n-- setup\n", "SQL before"),
        ("-- setup\nCREATE TABLE t(x);\n", "no '-- query:'"),
        ("-- setup\n-- query: Two\nSELECT 1; SELECT 2;\n", "single statement"),
        ("-- setup\n-- query:\nSELECT 1;\n", "needs a title"),
        ("-- query: A\nSELECT 1;\n", "before '-- setup'"),
    ],
)
def test_parse_scenario_rejects_bad_files(text: str, message: str) -> None:
    with pytest.raises(ValueError, match=message):
        parse_scenario(text, "bad")


def test_capture_is_deterministic(shop: tuple[dict[str, Any], Path], tmp_path: Path) -> None:
    trace, _ = shop
    again = capture(parse_scenario(SHOP.read_text(encoding="utf-8"), "shop"), tmp_path)
    assert dump(again) == dump(trace)


def test_capture_works_through_a_symlinked_directory(
    shop: tuple[dict[str, Any], Path], tmp_path: Path
) -> None:
    real = tmp_path / "real"
    real.mkdir()
    link = tmp_path / "link"
    link.symlink_to(real, target_is_directory=True)
    trace, _ = shop
    again = capture(parse_scenario(SHOP.read_text(encoding="utf-8"), "shop"), link)
    assert dump(again) == dump(trace)


def test_page_map_covers_every_page(shop: tuple[dict[str, Any], Path]) -> None:
    trace, _ = shop
    assert [p["page"] for p in trace["pages"]] == list(range(1, trace["page_count"] + 1))
    assert all(p["kind"] != "free" for p in trace["pages"])


def test_decoded_table_rows_match_select(shop: tuple[dict[str, Any], Path]) -> None:
    trace, db = shop
    rows = decode_rows(db, trace["pages"], trace["objects"])
    conn = apsw.Connection(str(db), flags=apsw.SQLITE_OPEN_READONLY)
    for table in ("users", "orders"):
        decoded = [tuple(r) for p in leaf_pages_of(trace, table) for r in rows[p]["rows"]]
        expected = [tuple(r) for r in conn.execute(f"SELECT * FROM {table} ORDER BY rowid")]
        assert decoded == expected, table


def test_decoded_index_entries_match_select(shop: tuple[dict[str, Any], Path]) -> None:
    # Index b-trees keep some keys in interior pages, so every page is needed.
    trace, db = shop
    rows = decode_rows(db, trace["pages"], trace["objects"])
    conn = apsw.Connection(str(db), flags=apsw.SQLITE_OPEN_READONLY)
    decoded = []
    for p in (p["page"] for p in trace["pages"] if p["object"] == "users_email"):
        cols = rows[p]["columns"]
        keyed = [r[1:] if cols[0] == "child_page" else r for r in rows[p]["rows"]]
        decoded += [tuple(r) for r in keyed if r[0] is not None]
    expected = [tuple(r) for r in conn.execute("SELECT email, rowid FROM users ORDER BY email")]
    assert sorted(decoded) == expected


def test_table_root_routes_to_every_leaf_in_order(shop: tuple[dict[str, Any], Path]) -> None:
    trace, db = shop
    rows = decode_rows(db, trace["pages"], trace["objects"])
    root = next(
        p["page"] for p in trace["pages"] if p["object"] == "users" and p["kind"] == "internal"
    )
    assert rows[root]["columns"] == ["child_page", "rowid_up_to"]
    assert [r[0] for r in rows[root]["rows"]] == leaf_pages_of(trace, "users")
    assert rows[root]["rows"][-1][1] is None


def test_point_lookup_reads_header_root_then_leaf(shop: tuple[dict[str, Any], Path]) -> None:
    trace, _ = shop
    lookup = trace["queries"][0]
    assert lookup["sql"] == "SELECT * FROM users WHERE id = 42;"
    pages = [page for op, page in lookup["io"]]
    root = next(
        p["page"] for p in trace["pages"] if p["object"] == "users" and p["kind"] == "internal"
    )
    assert pages[:2] == [1, root]
    leaf = trace["rows"][str(pages[2])]
    assert leaf["object"] == "users"
    assert any(row[0] == 42 for row in leaf["rows"])


def test_report_reads_every_orders_leaf_in_order(shop: tuple[dict[str, Any], Path]) -> None:
    trace, _ = shop
    report = next(q for q in trace["queries"] if q["plan"] == ["SCAN orders"])
    reads = [page for op, page in report["io"] if op == "r"]
    leaves = leaf_pages_of(trace, "orders")
    assert reads[-len(leaves) :] == leaves


def test_update_writes_header_and_the_changed_leaf(shop: tuple[dict[str, Any], Path]) -> None:
    trace, _ = shop
    update = next(q for q in trace["queries"] if q["sql"].startswith("UPDATE"))
    writes = [page for op, page in update["io"] if op == "w"]
    lookup_leaf = trace["queries"][0]["io"][-1][1]
    assert writes == [1, lookup_leaf]


def test_page_reader_rejects_non_btree_pages(shop: tuple[dict[str, Any], Path]) -> None:
    _, db = shop
    data = bytearray(db.read_bytes())
    reader = PageReader(bytes(data))
    data[reader.page_size * 4] = 0x00
    with pytest.raises(ValueError, match="not a b-tree page"):
        PageReader(bytes(data)).cells(5)
