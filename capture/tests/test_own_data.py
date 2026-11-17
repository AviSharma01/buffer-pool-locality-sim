"""Capturing from your own data (an existing database, CSV files, command-line
SQL) and the guards that apply when the database or the SQL may not be ours."""

from __future__ import annotations

import shutil
import struct
import subprocess
import sys
from itertools import pairwise
from pathlib import Path
from typing import Any

import apsw
import pytest

import capture as cap
from capture import Limits, Scenario, capture, decode_rows, parse_scenario

HERE = Path(__file__).parent.parent
PEOPLE = HERE / "examples" / "people.csv"
CHECKOUT = HERE / "scenarios" / "checkout.sql"
GROWTH = HERE / "scenarios" / "growth.sql"
TRACES = HERE.parent / "traces"


def make_db(path: Path, sql: str) -> Path:
    conn = apsw.Connection(str(path))
    conn.execute(sql)
    conn.close()
    return path


@pytest.fixture
def notes_db(tmp_path: Path) -> Path:
    return make_db(
        tmp_path / "notes.sqlite",
        """CREATE TABLE notes(id INTEGER PRIMARY KEY, title TEXT, body TEXT);
           WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 400)
           INSERT INTO notes SELECT i, 'note ' || i, hex(randomblob(40)) FROM n;""",
    )


def run(tmp_path: Path, sql: list[str], **kwargs: Any) -> dict[str, Any]:
    limits = kwargs.pop("limits", None)
    rows = kwargs.pop("rows", True)
    scenario = Scenario(name="t", setup="", queries=[(s, s) for s in sql], **kwargs)
    work = tmp_path / "work"
    work.mkdir(exist_ok=True)
    return capture(scenario, work, limits, rows=rows)


def people(tmp_path: Path, sql: list[str], **kwargs: Any) -> dict[str, Any]:
    return run(tmp_path, sql, csvs=[("people", PEOPLE)], **kwargs)


# ---- CSV ----


def test_csv_becomes_a_keyed_table_with_inferred_types(tmp_path: Path) -> None:
    trace = people(tmp_path, ["SELECT * FROM people WHERE id = 900"])
    assert [o["name"] for o in trace["objects"]] == ["people"]
    # `id` holds unique integers, so it is the key and a lookup walks the b-tree.
    assert trace["queries"][0]["plan"] == ["SEARCH people USING INTEGER PRIMARY KEY (rowid=?)"]
    leaf = trace["rows"][str(trace["queries"][0]["io"][-1][1])]
    assert leaf["columns"] == ["id", "name", "email", "city", "age", "joined"]
    row = next(r for r in leaf["rows"] if r[0] == 900)
    assert isinstance(row[4], int) and isinstance(row[1], str)


def test_an_index_changes_a_scan_into_a_search(tmp_path: Path) -> None:
    sql = ["SELECT * FROM people WHERE email = 'kofi.berg250@example.com'"]
    scan = people(tmp_path, sql)
    search = people(tmp_path, sql, indexes=["people(email)"])
    assert scan["queries"][0]["plan"] == ["SCAN people"]
    assert "USING INDEX idx_people_email" in search["queries"][0]["plan"][0]
    assert len(search["queries"][0]["io"]) < len(scan["queries"][0]["io"])
    for trace in (scan, search):
        assert all(1 <= page <= trace["page_count"] for _, page in trace["queries"][0]["io"])


def test_csv_with_awkward_headers_and_values(tmp_path: Path) -> None:
    path = tmp_path / "odd.csv"
    path.write_text(
        'na"me,,select,select,amount\n'
        "a,1,x,y,1.5\n"
        "'); DROP TABLE odd; --,2,x,y,\n"
        "b,3,x,y,99999999999999999999999\n",
        encoding="utf-8",
    )
    trace = run(tmp_path, ["SELECT * FROM odd"], csvs=[("odd", path)])
    table = trace["rows"][str(trace["queries"][0]["io"][-1][1])]
    assert table["columns"] == ["rowid", 'na"me', "column_2", "select", "select_2", "amount"]
    assert [r[1] for r in table["rows"]] == ["a", "'); DROP TABLE odd; --", "b"]
    # An empty cell is NULL; a number too long for an integer is still a number.
    assert table["rows"][1][5] is None and table["rows"][2][5] == pytest.approx(1e23)


@pytest.mark.parametrize(
    ("table", "content", "message"),
    [
        ("bad name", "a\n1\n", "letters, digits and underscores"),
        ("t", "a,b\n1\n", "row 2 has 1 fields"),
        ("t", "a,b\n", "header row and at least one data row"),
        ("t", b"a\n\xff\xfe\n", "not a UTF-8 CSV file"),
    ],
)
def test_bad_csv_is_refused(tmp_path: Path, table: str, content: str | bytes, message: str) -> None:
    path = tmp_path / "bad.csv"
    path.write_bytes(content if isinstance(content, bytes) else content.encode())
    with pytest.raises(ValueError, match=message):
        run(tmp_path, ["SELECT 1"], csvs=[(table, path)])


def test_bad_index_specs_are_refused(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match=r"expected table\(column"):
        people(tmp_path, ["SELECT 1"], indexes=["people(email); DROP TABLE people"])
    with pytest.raises(ValueError, match="no such column"):
        people(tmp_path, ["SELECT 1"], indexes=["people(nope)"])


# ---- an existing database ----


def test_the_original_database_is_never_changed(tmp_path: Path, notes_db: Path) -> None:
    before = notes_db.read_bytes()
    trace = run(tmp_path, ["UPDATE notes SET title = 'changed' WHERE id = 7"], db=notes_db)
    assert any(op == "w" for op, _ in trace["queries"][0]["io"])
    assert notes_db.read_bytes() == before
    assert sorted(p.name for p in notes_db.parent.iterdir() if p.is_file()) == ["notes.sqlite"]


def test_a_write_ahead_log_is_folded_into_the_copy(tmp_path: Path) -> None:
    src = tmp_path / "wal.sqlite"
    conn = apsw.Connection(str(src))
    conn.execute("PRAGMA journal_mode = WAL").fetchall()
    conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)")
    conn.execute("INSERT INTO t VALUES(1, 'only in the log')")
    assert src.with_name("wal.sqlite-wal").stat().st_size > 0
    try:
        trace = run(tmp_path, ["SELECT * FROM t"], db=src)
    finally:
        conn.close()
    assert [1, "only in the log"] in trace["rows"]["2"]["rows"]


def test_files_that_are_not_databases_are_refused(tmp_path: Path) -> None:
    text = tmp_path / "notes.txt"
    text.write_text("hello " * 100)
    with pytest.raises(ValueError, match="not a SQLite database file"):
        run(tmp_path, ["SELECT 1"], db=text)
    with pytest.raises(ValueError, match="no such file"):
        run(tmp_path, ["SELECT 1"], db=tmp_path / "missing.sqlite")
    # The right first bytes, then nothing usable.
    fake = tmp_path / "fake.sqlite"
    fake.write_bytes(cap.SQLITE_MAGIC + b"\x00" * 5000)
    with pytest.raises(ValueError, match="cannot be read as a SQLite database|damaged"):
        run(tmp_path, ["SELECT 1"], db=fake)


def test_a_damaged_database_is_refused(tmp_path: Path, notes_db: Path) -> None:
    data = bytearray(notes_db.read_bytes())
    data[4096 * 3 : 4096 * 4] = bytes(range(256)) * 16
    notes_db.write_bytes(data)
    with pytest.raises(ValueError, match="damaged|cannot be read"):
        run(tmp_path, ["SELECT count(*) FROM notes"], db=notes_db)


def test_decoding_survives_damaged_pages_and_looping_overflow_chains(tmp_path: Path) -> None:
    db = make_db(
        tmp_path / "big.sqlite",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, body TEXT);"
        "INSERT INTO t VALUES(1, printf('%.*c', 20000, 'x'));",
    )
    _, objects, pages = cap._layout(db, Limits())
    first_overflow = next(p["page"] for p in pages if p["kind"] == "overflow")
    data = bytearray(db.read_bytes())
    # Point the first overflow page back at itself.
    start = (first_overflow - 1) * 4096
    data[start : start + 4] = struct.pack(">I", first_overflow)
    db.write_bytes(data)
    rows = decode_rows(db, pages, objects)
    assert rows[2]["rows"][0][0] == 1  # decoded, and it returned

    data[4096 : 4096 * 2] = b"\x0d" + b"\xff" * 4095  # a leaf whose cell pointers lead nowhere
    db.write_bytes(data)
    decoded, skipped, _ = cap._decode(db, pages, objects, None)
    assert 2 not in decoded and skipped == 1


def test_untrusted_databases_are_opened_defensively(notes_db: Path) -> None:
    conn = cap._open(notes_db)
    assert conn.config(apsw.SQLITE_DBCONFIG_DEFENSIVE, -1) == 1
    assert conn.config(apsw.SQLITE_DBCONFIG_TRUSTED_SCHEMA, -1) == 0
    conn.close()


# ---- SQL that reaches outside, or never ends ----


@pytest.mark.parametrize(
    "sql",
    [
        "ATTACH '{out}' AS other",
        "VACUUM INTO '{out}'",
        "VACUUM",
        "PRAGMA journal_mode = WAL",
        "PRAGMA writable_schema = ON",
        "PRAGMA cache_size = 5000",
        "SELECT load_extension('{out}')",
    ],
)
def test_sql_cannot_reach_other_files_or_change_how_io_is_done(tmp_path: Path, sql: str) -> None:
    out = tmp_path / "escaped.db"
    with pytest.raises(ValueError, match="not allowed here|not authorized"):
        people(tmp_path, [sql.format(out=out)])
    assert not out.exists()


def test_scenario_setup_is_restricted_too(tmp_path: Path) -> None:
    out = tmp_path / "escaped.db"
    text = f"-- setup\nCREATE TABLE t(a);\nATTACH '{out}' AS other;\n-- query: q\nSELECT 1;\n"
    with pytest.raises(ValueError, match="setup: not allowed here"):
        capture(parse_scenario(text, "t"), tmp_path)
    assert not out.exists()


def test_vacuum_in_sql_points_to_the_flag(tmp_path: Path) -> None:
    text = "-- setup\nCREATE TABLE t(a);\nVACUUM;\n-- query: q\nSELECT 1;\n"
    with pytest.raises(ValueError, match=r"setup: not allowed here.*VACUUM \(use --vacuum\)"):
        capture(parse_scenario(text, "t"), tmp_path)


def test_runaway_queries_are_stopped(tmp_path: Path) -> None:
    forever = (
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) FROM n"
    )
    with pytest.raises(ValueError, match=r"stopped after 0\.2 s"):
        people(tmp_path, [forever], limits=Limits(timeout=0.2))
    join = "SELECT count(*) FROM people a, people b WHERE a.name < b.name"
    with pytest.raises(ValueError, match="stopped after 50 page reads and writes"):
        people(tmp_path, [join], limits=Limits(max_io=50))


def test_oversized_databases_are_refused(tmp_path: Path, notes_db: Path) -> None:
    with pytest.raises(ValueError, match=r"has \d+ pages; the limit is 5"):
        run(tmp_path, ["SELECT 1"], db=notes_db, limits=Limits(max_pages=5))
    with pytest.raises(ValueError, match="larger than"):
        people(tmp_path, ["SELECT 1"], limits=Limits(max_pages=2))


# ---- what ends up in the trace ----


def test_rows_can_be_left_out(tmp_path: Path) -> None:
    trace = people(tmp_path, ["SELECT * FROM people WHERE id = 5"], rows=False)
    assert trace["rows"] == {}
    assert "left out" in trace["notes"][2]
    assert not list((tmp_path / "work").glob("*.before.db"))


def test_long_values_are_cut(tmp_path: Path, notes_db: Path) -> None:
    trace = run(
        tmp_path, ["SELECT * FROM notes WHERE id = 3"], db=notes_db, limits=Limits(max_cell=10)
    )
    leaf = trace["rows"][str(trace["queries"][0]["io"][-1][1])]
    assert all(len(r[2]) == 11 and r[2].endswith("…") for r in leaf["rows"])
    assert "cut to 10 characters" in trace["notes"][-1]


def test_unusual_tables_decode_with_the_right_columns(tmp_path: Path) -> None:
    db = make_db(
        tmp_path / "odd.sqlite",
        """CREATE TABLE kv(v TEXT, k TEXT PRIMARY KEY) WITHOUT ROWID;
           INSERT INTO kv VALUES('value', 'key');
           CREATE TABLE grown(id INTEGER PRIMARY KEY, a TEXT);
           INSERT INTO grown VALUES(1, 'old');
           ALTER TABLE grown ADD COLUMN b TEXT;
           INSERT INTO grown VALUES(2, 'new', 'b');""",
    )
    _, objects, pages = cap._layout(db, Limits())
    rows = {r["object"]: r for r in decode_rows(db, pages, objects).values()}
    # WITHOUT ROWID tables store the key first.
    assert (rows["kv"]["columns"], rows["kv"]["rows"]) == (["k", "v"], [["key", "value"]])
    # A row written before the column was added is stored short.
    assert rows["grown"]["rows"] == [[1, "old", None], [2, "new", "b"]]


def test_checkout_grows_the_file_and_stays_replayable(tmp_path: Path) -> None:
    trace = capture(parse_scenario(CHECKOUT.read_text(encoding="utf-8"), "checkout"), tmp_path)
    again = capture(parse_scenario(CHECKOUT.read_text(encoding="utf-8"), "checkout"), tmp_path)
    assert trace == again
    io = [(op, page) for q in trace["queries"] for op, page in q["io"]]
    assert all(1 <= page <= trace["page_count"] for _, page in io)
    assert len(trace["pages"]) == trace["page_count"] == 32
    # The restock splits a full products page: page 32 is written without ever being read.
    assert ("w", 32) in io and ("r", 32) not in io and "32" not in trace["rows"]
    assert trace["pages"][31] == {"page": 32, "object": "products", "kind": "leaf"}
    assert "1 page(s) were created by the queries" in trace["notes"][3]


# ---- layout ----


def runs_out_of_order(trace: dict[str, Any]) -> dict[str, int]:
    """For each table and index, how often its next page is not the next page in the file."""
    pages: dict[str, list[int]] = {}
    for p in trace["pages"]:
        if p["object"] not in (None, "sqlite_schema"):
            pages.setdefault(p["object"], []).append(p["page"])
    return {o: sum(b != a + 1 for a, b in pairwise(ps)) for o, ps in pages.items()}


def test_vacuum_puts_each_table_and_index_in_one_run(tmp_path: Path) -> None:
    text = GROWTH.read_text(encoding="utf-8")
    grown = capture(parse_scenario(text, "growth"), tmp_path)
    scenario = parse_scenario(text, "growth")
    scenario.vacuum = True
    vacuumed = capture(scenario, tmp_path)
    assert runs_out_of_order(grown) == {"orders": 24, "events": 27, "events_order": 13}
    # One jump each: from the root page, written first, to the leaves after it.
    assert runs_out_of_order(vacuumed) == {"orders": 1, "events": 1, "events_order": 1}
    assert [q["sql"] for q in grown["queries"]] == [q["sql"] for q in vacuumed["queries"]]
    assert cap.VACUUM_NOTE in vacuumed["notes"] and cap.VACUUM_NOTE not in grown["notes"]
    # The rebuilt index packs into one page fewer, so layouts can differ in size too.
    assert (grown["page_count"], vacuumed["page_count"]) == (77, 76)


def test_vacuum_changes_where_rows_live_not_what_queries_return(tmp_path: Path) -> None:
    scenario = parse_scenario(GROWTH.read_text(encoding="utf-8"), "growth")
    grown = make_db(tmp_path / "grown.db", scenario.setup)
    vacuumed = tmp_path / "vacuumed.db"
    shutil.copyfile(grown, vacuumed)
    cap._vacuum(vacuumed, 10)
    results = []
    for path in (grown, vacuumed):
        conn = apsw.Connection(str(path))
        results.append([list(conn.execute(sql)) for _, sql in scenario.queries])
        conn.close()
    assert results[0] == results[1] and results[0][0] != []


@pytest.mark.parametrize("name", ["shop", "checkout", "growth", "shop-vacuum", "growth-vacuum"])
def test_shipped_traces_match_a_fresh_capture(tmp_path: Path, name: str) -> None:
    shipped = (TRACES / f"{name}.trace.json").read_text(encoding="utf-8")
    base = name.removesuffix("-vacuum")
    scenario = parse_scenario(
        (HERE / "scenarios" / f"{base}.sql").read_text(encoding="utf-8"), name
    )
    scenario.vacuum = name.endswith("-vacuum")
    fresh = capture(scenario, tmp_path)
    if f'"sqlite_version": "{fresh["sqlite_version"]}"' not in shipped:
        pytest.skip(f"traces were captured with another SQLite than {fresh['sqlite_version']}")
    assert cap.dump(fresh) == shipped


# ---- command line ----


def cli(*args: str | Path) -> subprocess.CompletedProcess[str]:
    cmd = [sys.executable, str(HERE / "capture.py"), *map(str, args)]
    return subprocess.run(cmd, capture_output=True, text=True, check=False)


def test_command_line_builds_a_scenario_from_parts(tmp_path: Path, notes_db: Path) -> None:
    queries = tmp_path / "q.sql"
    queries.write_text("-- query: First note\nSELECT * FROM notes WHERE id = 1;\n")
    out = tmp_path / "out.trace.json"
    done = cli(queries, "--db", notes_db, "--sql", "SELECT count(*) FROM notes", "-o", out)
    assert done.returncode == 0, done.stderr
    assert "q: 2 queries" in done.stdout and "--no-rows leaves them out" in done.stdout
    text = out.read_text(encoding="utf-8")
    assert '"title":"First note"' in text and '"title":"SELECT count(*) FROM notes"' in text


@pytest.mark.parametrize(
    ("args", "message"),
    [
        ([], "give a scenario .sql file"),
        (["--csv", f"people={PEOPLE}"], "no queries"),
        (["--csv", "people", "--sql", "SELECT 1"], "expected table=file.csv"),
        (["--csv", f"people={PEOPLE}", "--sql", "SELECT 1; SELECT 2"], "single statement"),
        (
            ["--csv", f"people={PEOPLE}", "--sql", "SELECT 1", "--max-pages", "0"],
            "must be positive",
        ),
        (["--csv", f"people={PEOPLE}", "--sql", "SELEC 1"], "syntax error"),
    ],
)
def test_command_line_errors_are_one_line(args: list[str], message: str) -> None:
    done = cli(*args)
    assert done.returncode == 1
    assert done.stderr.startswith("error: ") and message in done.stderr
    assert "Traceback" not in done.stderr


def test_vacuum_works_on_your_own_database_and_leaves_it_alone(
    tmp_path: Path, notes_db: Path
) -> None:
    before = notes_db.read_bytes()
    out = tmp_path / "o.json"
    done = cli("--db", notes_db, "--sql", "SELECT count(*) FROM notes", "--vacuum", "-o", out)
    assert done.returncode == 0, done.stderr
    assert done.stdout.startswith("notes-vacuum: 1 queries")
    assert cap.VACUUM_NOTE in out.read_text(encoding="utf-8")
    assert notes_db.read_bytes() == before
    csv = cli(
        "--csv", f"people={PEOPLE}", "--sql", "SELECT 1", "--vacuum", "--name", "p", "-o", out
    )
    assert csv.returncode == 0 and csv.stdout.startswith("p-vacuum:"), csv.stderr


def test_scenario_name_is_made_safe_for_paths(tmp_path: Path, notes_db: Path) -> None:
    odd = notes_db.with_name("../my notes (v2).sqlite".replace("../", ""))
    shutil.copyfile(notes_db, odd)
    out = tmp_path / "o.json"
    done = cli("--db", odd, "--sql", "SELECT 1", "-o", out, "--name", "../../etc/x y")
    assert done.returncode == 0, done.stderr
    assert '"scenario": "etc-x-y"' in out.read_text(encoding="utf-8")


@pytest.mark.skipif(
    not (cap.REPO / "sim" / "target" / "release" / "sim").is_file(), reason="engine not built"
)
def test_view_replays_with_the_engine(tmp_path: Path) -> None:
    out = tmp_path / "people.trace.json"
    args = ["--csv", f"people={PEOPLE}", "--sql", "SELECT * FROM people WHERE id = 9", "-o", out]
    done = cli(*args, "--view", "--frames", "4", "--policy", "lru,opt")
    assert done.returncode == 0, done.stderr
    viewer = Path(done.stdout.strip().splitlines()[-1].removeprefix("viewer -> "))
    assert viewer.name == "viewer.html" and "bpsim-run" in viewer.read_text(encoding="utf-8")
    shutil.rmtree(viewer.parent)
    too_many = cli(*args, "--view", "--frames", "4", "--policy", "nope")
    assert too_many.returncode == 1 and "unknown policy" in too_many.stderr
