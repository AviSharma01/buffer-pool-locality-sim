"""Capture the page-level I/O SQLite performs for a scenario of queries.

A scenario is one SQL file: a `-- setup` section that builds the database,
then `-- query: <title>` sections, one statement each. The database can also
come from your own data: an existing SQLite file (`--db`, always used as a
copy) or CSV files that become tables (`--csv`), with queries from a file or
from `--sql`. `--vacuum` rebuilds the database with VACUUM before the queries,
so each table and index sits in one run of pages. The tool replays the queries
through a VFS shim that logs every read and write SQLite makes on the main
database file. The result is a trace the Rust engine can replay through any
buffer pool.

Databases and SQL from elsewhere are treated as untrusted: the original file
is never opened; ATTACH, VACUUM, extensions and most PRAGMAs are refused in
SQL (--vacuum runs VACUUM itself, on the copy); schemas are not trusted; and
size and time limits apply. This guards against hostile files and mistakes;
it is not a sandbox.

Trace format (JSON, version 2):
    format, version, scenario, sqlite_version, page_size, page_count
    objects  [{name, type, table}]                 tables and indexes
    pages    [{page, object, kind}]                kind: internal | leaf | overflow | free
    queries  [{title, sql, plan, scans, io}]       io: [["r"|"w", page], ...] in order
                                                   scans: tables and indexes read end to end
    rows     {"<page>": {object, columns, rows}}   decoded page contents, for pages queries touch
    notes    [str]                                 how to read the trace

Usage:
    python capture/capture.py capture/scenarios/shop.sql -o traces/shop.trace.json
    python capture/capture.py capture/scenarios/growth.sql --vacuum \\
        -o traces/growth-vacuum.trace.json
    python capture/capture.py --db my.sqlite --sql "SELECT * FROM t WHERE id = 7" --view
    python capture/capture.py --csv people=people.csv --index "people(email)" \\
        --sql "SELECT * FROM people WHERE email = 'a@b.c'" --view
"""

from __future__ import annotations

import argparse
import csv
import gzip
import json
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import apsw

import workload

TRACE_VERSION = 2
VFS_NAME = "bpsim-trace"
COUNT_VFS_NAME = "bpsim-count"
REPO = Path(__file__).resolve().parent.parent
SQLITE_MAGIC = b"SQLite format 3\x00"
MAX_PAGE_SIZE = 65536
SETUP_SECONDS = 60.0
# Memory guards: no single string or blob may exceed MAX_VALUE bytes, and
# SQLite may not hold more than HEAP_LIMIT bytes in all while capturing.
MAX_VALUE = 10_000_000
HEAP_LIMIT = 256 * 1024 * 1024

NOTES = [
    "Page 1 holds the file header; SQLite reads it at the start of every statement to check "
    "whether the file changed since its last look.",
    "Reads are what SQLite asked the file for with its own page cache at the minimum size, so "
    "almost every page reference is visible. Writes are logged when the statement commits.",
    "Rows show each page as it was right after setup, before any query ran.",
]
VACUUM_NOTE = (
    "The database was rebuilt with VACUUM after setup, so each table and index sits in one run "
    "of pages. Capture it without --vacuum to compare the layout it grew into."
)

# What scenario and command-line SQL may use. Everything that reaches another
# file or changes how SQLite does I/O (which would make the trace meaningless)
# is left out.
ALLOWED_PRAGMAS = frozenset(
    {
        "page_size",
        "auto_vacuum",
        "foreign_keys",
        "case_sensitive_like",
        "table_info",
        "table_xinfo",
        "index_list",
        "index_info",
        "index_xinfo",
    }
)


@dataclass(frozen=True)
class Limits:
    """Guards against oversized inputs and runaway queries."""

    max_pages: int = 5000
    timeout: float = 5.0
    max_cell: int = 200
    max_io: int = 20000


@dataclass
class Scenario:
    name: str
    setup: str
    queries: list[tuple[str, str]]
    db: Path | None = None
    csvs: list[tuple[str, Path]] = field(default_factory=list)
    indexes: list[str] = field(default_factory=list)
    vacuum: bool = False


def _check_single_statement(title: str, sql: str) -> str:
    sql = sql.strip()
    if not sql:
        raise ValueError(f"query {title!r} is empty")
    if ";" in sql.rstrip(";"):
        raise ValueError(f"query {title!r} must be a single statement")
    return sql


def parse_scenario(
    text: str, name: str, setup_required: bool = True, seed: int | None = None
) -> Scenario:
    """Split a scenario file into its setup SQL and titled queries.

    With `setup_required=False` (the database comes from `--db` or `--csv`)
    the file may hold only `-- query:` sections. A last `-- workload:`
    section adds many queries made from templates (see workload.py); `seed`
    replaces the seed it names, for a local variant.
    """
    setup: list[str] | None = None
    queries: list[tuple[str, list[str]]] = []
    current: list[str] | None = None
    load: workload.Workload | None = None
    lines = text.splitlines()
    for lineno, line in enumerate(lines, start=1):
        stripped = line.strip()
        if workload.is_header(stripped):
            if setup is None and setup_required:
                raise ValueError(f"line {lineno}: '-- workload:' before '-- setup'")
            rest = list(enumerate(lines[lineno - 1 :], start=lineno))
            load = workload.parse(rest, _check_single_statement)
            if seed is not None:
                load.seed = seed
            break
        if stripped == "-- setup":
            if setup is not None:
                raise ValueError(f"line {lineno}: a scenario has only one '-- setup' section")
            if queries:
                raise ValueError(f"line {lineno}: '-- setup' must come before the queries")
            setup = current = []
        elif stripped.startswith("-- query:"):
            if setup is None and setup_required:
                raise ValueError(f"line {lineno}: '-- query:' before '-- setup'")
            title = stripped.removeprefix("-- query:").strip()
            if not title:
                raise ValueError(f"line {lineno}: query needs a title")
            current = []
            queries.append((title, current))
        elif current is not None:
            current.append(line)
        elif stripped and not stripped.startswith("--"):
            raise ValueError(f"line {lineno}: SQL before '-- setup'")
    if setup is None and setup_required:
        raise ValueError("scenario has no '-- setup' section")
    if not queries and load is None and setup_required:
        raise ValueError("scenario has no '-- query:' or '-- workload:' sections")
    parsed = [(title, _check_single_statement(title, "\n".join(body))) for title, body in queries]
    if load is not None:
        parsed += workload.generate(load)
    return Scenario(name=name, setup="\n".join(setup or []), queries=parsed)


class _TraceFile(apsw.VFSFile):
    def __init__(self, vfs: TraceVFS, name: apsw.URIFilename | str | None, flags: list[int]):
        self._vfs = vfs
        self._main = bool(flags[0] & apsw.mapping_open_flags["SQLITE_OPEN_MAIN_DB"])
        super().__init__("", name, flags)

    def xRead(self, amount: int, offset: int) -> bytes:
        if self._main:
            self._vfs.log.append(("r", offset // self._vfs.page_size + 1))
        return super().xRead(amount, offset)

    def xWrite(self, data: bytes, offset: int) -> None:
        if self._main:
            self._vfs.log.append(("w", offset // self._vfs.page_size + 1))
        super().xWrite(data, offset)


class TraceVFS(apsw.VFS):
    """Passes everything to the default VFS, logging main-database page I/O."""

    def __init__(self, page_size: int):
        self.log: list[tuple[str, int]] = []
        self.page_size = page_size
        super().__init__(VFS_NAME, "")

    def xOpen(self, name: apsw.URIFilename | str | None, flags: list[int]) -> _TraceFile:
        return _TraceFile(self, name, flags)


class _CountingFile(apsw.VFSFile):
    def __init__(self, vfs: CountingVFS, name: apsw.URIFilename | str | None, flags: list[int]):
        self._vfs = vfs
        self._main = bool(flags[0] & apsw.mapping_open_flags["SQLITE_OPEN_MAIN_DB"])
        super().__init__("", name, flags)

    def xRead(self, amount: int, offset: int) -> bytes:
        if self._main and amount == self._vfs.page_size:
            self._vfs.page_reads += 1
        return super().xRead(amount, offset)


class CountingVFS(apsw.VFS):
    """Passes everything to the default VFS, counting whole-page reads of the
    main database (not SQLite's 16-byte header checks)."""

    def __init__(self, page_size: int):
        self.page_size = page_size
        self.page_reads = 0
        super().__init__(COUNT_VFS_NAME, "")

    def xOpen(self, name: apsw.URIFilename | str | None, flags: list[int]) -> _CountingFile:
        return _CountingFile(self, name, flags)


def sqlite_cache_reads(
    db: Path, sql: list[str], pages: int, page_size: int, workdir: Path, limits: Limits
) -> int:
    """Pages SQLite reads from the file running `sql` on a copy of `db` with its
    own page cache `pages` pages big. That cache is LRU, so this is an outside
    check on the engine's LRU (with read-ahead off)."""
    run_db = workdir.resolve() / "sqlite-cache.db"
    shutil.copyfile(db, run_db)
    vfs = CountingVFS(page_size)
    try:
        conn = _open(run_db, vfs=COUNT_VFS_NAME)
        conn.execute(f"PRAGMA cache_size = {pages}; PRAGMA mmap_size = 0;")
        _restrict(conn, limits.max_pages)
        for i, query in enumerate(sql, start=1):
            _run(conn, query, f"query {i} with a {pages}-page cache", limits.timeout, keep=False)
        conn.close()
    finally:
        vfs.unregister()
        for suffix in ("", "-wal", "-journal"):
            run_db.with_name(run_db.name + suffix).unlink(missing_ok=True)
    return vfs.page_reads


# ---- SQLite file-format decoding (https://www.sqlite.org/fileformat2.html) ----


def _varint(buf: bytes, pos: int) -> tuple[int, int]:
    value = 0
    for i in range(8):
        byte = buf[pos + i]
        value = (value << 7) | (byte & 0x7F)
        if byte < 0x80:
            return value, pos + i + 1
    return (value << 8) | buf[pos + 8], pos + 9


def _decode_record(payload: bytes) -> list[Any]:
    header_size, pos = _varint(payload, 0)
    types = []
    while pos < header_size:
        serial, pos = _varint(payload, pos)
        types.append(serial)
    values: list[Any] = []
    pos = header_size
    int_sizes = {1: 1, 2: 2, 3: 3, 4: 4, 5: 6, 6: 8}
    for serial in types:
        if serial == 0:
            values.append(None)
        elif serial in int_sizes:
            n = int_sizes[serial]
            values.append(int.from_bytes(payload[pos : pos + n], "big", signed=True))
            pos += n
        elif serial == 7:
            values.append(struct.unpack(">d", payload[pos : pos + 8])[0])
            pos += 8
        elif serial in (8, 9):
            values.append(serial - 8)
        elif serial >= 12 and serial % 2 == 0:
            n = (serial - 12) // 2
            values.append({"blob": payload[pos : pos + n].hex()})
            pos += n
        elif serial >= 13:
            n = (serial - 13) // 2
            values.append(payload[pos : pos + n].decode("utf-8", errors="replace"))
            pos += n
        else:
            raise ValueError(f"unsupported serial type {serial}")
    return values


class PageReader:
    """Decodes the cells of b-tree pages in a database file."""

    def __init__(self, data: bytes):
        if len(data) < 100 or not data.startswith(SQLITE_MAGIC):
            raise ValueError("not a SQLite database file")
        self.data = data
        self.page_size = struct.unpack(">H", data[16:18])[0] or MAX_PAGE_SIZE
        self.usable = self.page_size - data[20]
        if struct.unpack(">I", data[56:60])[0] != 1:
            raise ValueError("only UTF-8 databases are supported")

    def page(self, number: int) -> bytes:
        start = (number - 1) * self.page_size
        return self.data[start : start + self.page_size]

    def _payload(self, page: bytes, pos: int, size: int, table: bool) -> bytes:
        # Large payloads spill into a chain of overflow pages; how much stays
        # local is fixed by the format and differs for table and index pages.
        u = self.usable
        max_local = u - 35 if table else (u - 12) * 64 // 255 - 23
        if size <= max_local:
            return page[pos : pos + size]
        min_local = (u - 12) * 32 // 255 - 23
        local = min_local + (size - min_local) % (u - 4)
        if local > max_local:
            local = min_local
        out = bytearray(page[pos : pos + local])
        next_page = struct.unpack(">I", page[pos + local : pos + local + 4])[0]
        # A damaged file can link overflow pages in a loop; no real chain is
        # longer than the file.
        for _ in range(len(self.data) // self.page_size + 1):
            if not next_page or len(out) >= size:
                break
            overflow = self.page(next_page)
            next_page = struct.unpack(">I", overflow[:4])[0]
            out += overflow[4 : 4 + min(u - 4, size - len(out))]
        return bytes(out)

    def cells(self, number: int) -> tuple[int, list[dict[str, Any]]]:
        """Decode one b-tree page into (page type byte, cells).

        Each cell may carry `child` (interior pages), `rowid` (table pages) and
        `values` (a decoded record: table leaves and both kinds of index page).
        Index interior pages hold real keys too, not just routing information.
        """
        page = self.page(number)
        base = 100 if number == 1 else 0
        kind = page[base] if len(page) > base else None
        if kind not in (0x02, 0x05, 0x0A, 0x0D):
            raise ValueError(f"page {number} is not a b-tree page")
        interior = kind in (0x02, 0x05)
        header = 12 if interior else 8
        n_cells = struct.unpack(">H", page[base + 3 : base + 5])[0]
        cells: list[dict[str, Any]] = []
        for i in range(n_cells):
            at = base + header + 2 * i
            pos = struct.unpack(">H", page[at : at + 2])[0]
            cell: dict[str, Any] = {}
            if interior:
                cell["child"] = struct.unpack(">I", page[pos : pos + 4])[0]
                pos += 4
            if kind == 0x05:
                cell["rowid"], pos = _varint(page, pos)
            else:
                size, pos = _varint(page, pos)
                if kind == 0x0D:
                    cell["rowid"], pos = _varint(page, pos)
                cell["values"] = _decode_record(self._payload(page, pos, size, table=kind == 0x0D))
            cells.append(cell)
        if interior:
            cells.append({"child": struct.unpack(">I", page[base + 8 : base + 12])[0]})
        return kind, cells


def _quote(name: str) -> str:
    return '"' + name.replace('"', '""') + '"'


def _columns(conn: apsw.Connection, obj: dict[str, str]) -> tuple[list[str], int | None, list[str]]:
    """Column names for decoded rows, the rowid-alias column index if any, and
    the names in the order a WITHOUT ROWID table stores them (key first)."""
    if obj["type"] == "index":
        names = [r[2] or "expr" for r in conn.execute(f"PRAGMA index_info({_quote(obj['name'])})")]
        return names + ["rowid"], None, names
    # Hidden kinds: 0 normal, 2 virtual generated (not stored), 3 stored generated.
    info = [r for r in conn.execute(f"PRAGMA table_xinfo({_quote(obj['name'])})") if r[6] in (0, 3)]
    names = [r[1] for r in info]
    pk = sorted((r for r in info if r[5]), key=lambda r: r[5])
    alias = info.index(pk[0]) if len(pk) == 1 and pk[0][2].upper() == "INTEGER" else None
    keyed = [r[1] for r in pk] + [r[1] for r in info if not r[5]]
    return names, alias, keyed


def _clip(value: Any, max_cell: int | None) -> tuple[Any, bool]:
    if max_cell is None:
        return value, False
    if isinstance(value, str) and len(value) > max_cell:
        return value[:max_cell] + "…", True
    if isinstance(value, dict) and len(value.get("blob", "")) > max_cell:
        return {"blob": value["blob"][:max_cell] + "…"}, True
    return value, False


def _decode(
    db_path: Path, pages: list[dict[str, Any]], objects: list[dict[str, str]], max_cell: int | None
) -> tuple[dict[int, dict[str, Any]], int, int]:
    """Decode the given pages; also count pages that could not be decoded
    (damaged or unusual) and cells cut to `max_cell` characters."""
    reader = PageReader(db_path.read_bytes())
    by_name = {o["name"]: o for o in objects}
    conn = _open(db_path, flags=apsw.SQLITE_OPEN_READONLY)
    decoded: dict[int, dict[str, Any]] = {}
    skipped = cut = 0
    columns_of: dict[str, tuple[list[str], int | None, list[str]]] = {}
    for p in pages:
        obj = by_name.get(p["object"])
        if p["kind"] not in ("leaf", "internal") or obj is None:
            continue
        try:
            if obj["name"] not in columns_of:
                columns_of[obj["name"]] = _columns(conn, obj)
            names, alias, keyed = columns_of[obj["name"]]
            kind, cells = reader.cells(p["page"])
            if obj["type"] == "table" and kind in (0x02, 0x0A):
                names, alias = keyed, None  # WITHOUT ROWID: stored like an index
            if kind == 0x05:
                columns = ["child_page", "rowid_up_to"]
                rows = [[c["child"], c.get("rowid")] for c in cells]
            elif kind == 0x02:
                columns = ["child_page", *names]
                rows = [[c["child"], *c.get("values", [])] for c in cells]
            elif kind == 0x0A:
                columns, rows = list(names), [c["values"] for c in cells]
            else:
                columns = list(names) if alias is not None else ["rowid", *names]
                rows = []
                for c in cells:
                    values = c["values"]
                    if alias is not None and alias < len(values):
                        values[alias] = c["rowid"]
                    elif alias is None:
                        values = [c["rowid"], *values]
                    rows.append(values)
        except (ValueError, IndexError, struct.error, apsw.Error):
            skipped += 1
            continue
        # Rows added before an ALTER TABLE ADD COLUMN are stored short.
        width = max([len(columns), *(len(r) for r in rows)])
        columns += [f"column {i + 1}" for i in range(len(columns), width)]
        clipped = []
        for r in rows:
            out = []
            for value in [*r, *([None] * (width - len(r)))]:
                value, was_cut = _clip(value, max_cell)
                cut += was_cut
                out.append(value)
            clipped.append(out)
        decoded[p["page"]] = {"object": obj["name"], "columns": columns, "rows": clipped}
    conn.close()
    return decoded, skipped, cut


def decode_rows(
    db_path: Path,
    pages: list[dict[str, Any]],
    objects: list[dict[str, str]],
    max_cell: int | None = None,
) -> dict[int, dict[str, Any]]:
    """Decode the b-tree pages in `pages` into {page: {object, columns, rows}}.

    Table interior pages become routing rows (child page, highest rowid on the
    left of it); the last row is the right-most child, which has no key.
    Index interior pages also list their child before each key.
    """
    return _decode(db_path, pages, objects, max_cell)[0]


# ---- running SQL that may not be ours ----


def _open(path: Path, **kwargs: Any) -> apsw.Connection:
    """Open with the schema untrusted: views and triggers stored in the file
    cannot call unsafe functions, and SQL cannot corrupt the file on purpose."""
    conn = apsw.Connection(str(path), **kwargs)
    conn.config(apsw.SQLITE_DBCONFIG_DEFENSIVE, 1)
    conn.config(apsw.SQLITE_DBCONFIG_TRUSTED_SCHEMA, 0)
    # Otherwise CREATE INDEX on a misspelt "column" silently indexes a string.
    conn.config(apsw.SQLITE_DBCONFIG_DQS_DDL, 0)
    return conn


def _authorizer(
    op: int, one: str | None, two: str | None, _db: str | None, _src: str | None
) -> int:
    if op in (apsw.SQLITE_ATTACH, apsw.SQLITE_DETACH):
        return apsw.SQLITE_DENY
    if op == apsw.SQLITE_PRAGMA and (one or "").lower() not in ALLOWED_PRAGMAS:
        return apsw.SQLITE_DENY
    if op == apsw.SQLITE_FUNCTION and (two or "").lower() == "load_extension":
        return apsw.SQLITE_DENY
    return apsw.SQLITE_OK


def _restrict(conn: apsw.Connection, max_pages: int) -> None:
    """From here on the connection refuses anything that reaches another file.
    The ATTACH limit also stops VACUUM INTO, which attaches its target. The
    page cap stops setup or a query from growing the file without bound;
    SQL cannot raise it, since PRAGMA max_page_count is not allowed."""
    conn.execute(f"PRAGMA max_page_count = {int(max_pages)}")
    conn.limit(apsw.SQLITE_LIMIT_ATTACHED, 0)
    conn.limit(apsw.SQLITE_LIMIT_SQL_LENGTH, 1_000_000)
    conn.limit(apsw.SQLITE_LIMIT_LENGTH, MAX_VALUE)
    conn.authorizer = _authorizer


def _run(
    conn: apsw.Connection,
    sql: str,
    what: str,
    seconds: float,
    log: list[tuple[str, int]] | None = None,
    max_io: int = 0,
    keep: bool = True,
) -> list[tuple[Any, ...]]:
    """Run restricted SQL to completion, stopping it if it runs too long or
    does too much I/O. With keep=False the rows are read and dropped one at a
    time, so a query returning millions of rows costs no memory here. Errors
    come back as ValueError naming `what`."""
    deadline = time.monotonic() + seconds

    def over() -> bool:
        return time.monotonic() > deadline or (log is not None and len(log) > max_io)

    conn.set_progress_handler(over, 1000)
    try:
        if keep:
            return list(conn.execute(sql))
        for _ in conn.execute(sql):
            pass
        return []
    except apsw.InterruptError:
        if log is not None and len(log) > max_io:
            raise ValueError(
                f"{what}: stopped after {max_io} page reads and writes (--max-io raises the limit)"
            ) from None
        raise ValueError(
            f"{what}: stopped after {seconds:g} s (--timeout raises the limit)"
        ) from None
    except apsw.FullError:
        raise ValueError(
            f"{what}: the database grew past its page limit (--max-pages raises it)"
        ) from None
    except apsw.TooBigError:
        raise ValueError(
            f"{what}: a value is larger than {MAX_VALUE // 1_000_000} MB, the most a capture allows"
        ) from None
    except apsw.NoMemError:
        raise ValueError(
            f"{what}: SQLite needed more than {HEAP_LIMIT // 2**20} MB of memory, "
            "the most a capture allows"
        ) from None
    except apsw.AuthError:
        raise ValueError(
            f"{what}: not allowed here. ATTACH, VACUUM (use --vacuum), loading extensions and "
            "PRAGMAs other than "
            f"{', '.join(sorted(ALLOWED_PRAGMAS))} are refused"
        ) from None
    except apsw.Error as e:
        raise ValueError(f"{what}: {e}") from None
    finally:
        conn.set_progress_handler(None)


# ---- building the database from your own data ----

_IDENT = r"[A-Za-z_][A-Za-z0-9_]*"
_INT = re.compile(r"[+-]?\d{1,18}")


def _copy_database(src: Path, dst: Path, limits: Limits) -> None:
    """Copy `src` (and its write-ahead log, if any) so the original is never opened."""
    if not src.is_file():
        raise ValueError(f"{src}: no such file")
    size = src.stat().st_size
    if size > limits.max_pages * MAX_PAGE_SIZE:
        raise ValueError(
            f"{src}: {size // 2**20} MB is too large for {limits.max_pages} pages "
            "(--max-pages raises the limit)"
        )
    with src.open("rb") as f:
        if f.read(len(SQLITE_MAGIC)) != SQLITE_MAGIC:
            raise ValueError(f"{src}: not a SQLite database file")
    shutil.copyfile(src, dst)
    wal = src.with_name(src.name + "-wal")
    if wal.is_file():
        shutil.copyfile(wal, dst.with_name(dst.name + "-wal"))


def _check_database(conn: apsw.Connection, src: Path) -> None:
    try:
        # Fold any write-ahead log into the copy; the trace follows the main file only.
        conn.execute("PRAGMA journal_mode = DELETE").fetchall()
        if conn.execute("PRAGMA encoding").fetchall()[0][0] != "UTF-8":
            raise ValueError(f"{src}: only UTF-8 databases are supported")
        if conn.execute("PRAGMA quick_check(1)").fetchall() != [("ok",)]:
            raise ValueError(f"{src}: the database file is damaged")
    except apsw.Error as e:
        raise ValueError(f"{src}: cannot be read as a SQLite database ({e})") from None


def _csv_value(text: str, kind: str) -> Any:
    if text == "":
        return None
    return int(text) if kind == "INTEGER" else float(text) if kind == "REAL" else text


def _is_real(text: str) -> bool:
    try:
        return float(text) == float(text) and abs(float(text)) != float("inf")
    except ValueError:
        return False


def _import_csv(conn: apsw.Connection, table: str, path: Path, max_bytes: int) -> None:
    """Create `table` from a CSV file with a header row. Column types are
    inferred; a first column named `id` holding unique integers becomes the key."""
    if not re.fullmatch(_IDENT, table):
        raise ValueError(f"--csv: table name {table!r} must be letters, digits and underscores")
    if not path.is_file():
        raise ValueError(f"{path}: no such file")
    if path.stat().st_size > max_bytes:
        raise ValueError(f"{path}: larger than {max_bytes // 2**20} MB (--max-pages raises it)")
    try:
        with path.open(newline="", encoding="utf-8-sig") as f:
            rows = [r for r in csv.reader(f) if r]
    except (UnicodeDecodeError, csv.Error) as e:
        raise ValueError(f"{path}: not a UTF-8 CSV file ({e})") from None
    if len(rows) < 2:
        raise ValueError(f"{path}: needs a header row and at least one data row")
    header: list[str] = []
    for i, raw in enumerate(rows[0], start=1):
        name = raw.strip() or f"column_{i}"
        while name.lower() in (h.lower() for h in header):
            name += "_2"
        header.append(name)
    data = rows[1:]
    for lineno, r in enumerate(data, start=2):
        if len(r) != len(header):
            raise ValueError(
                f"{path}: row {lineno} has {len(r)} fields, the header has {len(header)}"
            )
    kinds = []
    for col in range(len(header)):
        cells = [r[col] for r in data if r[col] != ""]
        if cells and all(_INT.fullmatch(c) for c in cells):
            kinds.append("INTEGER")
        elif cells and all(_is_real(c) for c in cells):
            kinds.append("REAL")
        else:
            kinds.append("TEXT")
    first = [r[0] for r in data]
    keyed = header[0].lower() == "id" and kinds[0] == "INTEGER" and "" not in first
    keyed = keyed and len({int(v) for v in first}) == len(first)
    defs = [f"{_quote(n)} {k}" for n, k in zip(header, kinds, strict=True)]
    if keyed:
        defs[0] += " PRIMARY KEY"
    try:
        conn.execute(f"CREATE TABLE {_quote(table)}({', '.join(defs)})")
        with conn:
            conn.executemany(
                f"INSERT INTO {_quote(table)} VALUES({', '.join('?' * len(header))})",
                ([_csv_value(v, k) for v, k in zip(r, kinds, strict=True)] for r in data),
            )
    except apsw.Error as e:
        raise ValueError(f"{path}: could not load into table {table!r} ({e})") from None


def _create_index(conn: apsw.Connection, spec: str) -> None:
    m = re.fullmatch(rf"\s*({_IDENT})\s*\(\s*({_IDENT}(?:\s*,\s*{_IDENT})*)\s*\)\s*", spec)
    if not m:
        raise ValueError(f"--index: expected table(column, ...), got {spec!r}")
    table, cols = m.group(1), [c.strip() for c in m.group(2).split(",")]
    name = "_".join(["idx", table, *cols])
    try:
        conn.execute(
            f"CREATE INDEX {_quote(name)} ON {_quote(table)}({', '.join(map(_quote, cols))})"
        )
    except apsw.Error as e:
        raise ValueError(f"--index {spec}: {e}") from None


# ---- capture ----


def _layout(
    db_path: Path, limits: Limits
) -> tuple[int, list[dict[str, str]], list[dict[str, Any]]]:
    """Page size, the tables and indexes, and which of them owns each page."""
    conn = _open(db_path, flags=apsw.SQLITE_OPEN_READONLY)
    try:
        page_size = conn.execute("PRAGMA page_size").fetchall()[0][0]
        page_count = conn.execute("PRAGMA page_count").fetchall()[0][0]
        if page_count > limits.max_pages:
            raise ValueError(
                f"the database has {page_count} pages; the limit is {limits.max_pages} "
                "(--max-pages raises it)"
            )
        objects = [
            {"name": name, "type": kind, "table": table}
            for name, kind, table in conn.execute(
                "SELECT name, type, tbl_name FROM sqlite_schema "
                "WHERE type IN ('table', 'index') AND rootpage > 0 ORDER BY rootpage"
            )
        ]
        owners = {
            pageno: {"page": pageno, "object": name, "kind": kind}
            for name, pageno, kind in conn.execute(
                "SELECT name, pageno, pagetype FROM dbstat ORDER BY pageno"
            )
        }
    except apsw.Error as e:
        raise ValueError(f"cannot read the database layout ({e})") from None
    finally:
        conn.close()
    pages = [
        owners.get(n, {"page": n, "object": None, "kind": "free"}) for n in range(1, page_count + 1)
    ]
    return page_size, objects, pages


# Opcodes that may jump to the address in P2, from SQLite's opcode list.
_JUMPS = frozenset(
    "Goto Gosub If IfNot IfPos IfNotZero DecrJumpZero Eq Ne Lt Le Gt Ge ElseEq IsNull NotNull "
    "IsType Once SeekLT SeekLE SeekGE SeekGT SeekRowid SeekScan SeekEnd Found NotFound NotExists "
    "NoConflict IfNoHope IdxGT IdxGE IdxLT IdxLE Rewind Last Next Prev SorterNext SorterSort "
    "Sort Yield InitCoroutine Jump Filter IfNullRow IfEmpty IfSizeBetween VFilter VNext "
    "RowSetTest RowSetRead MustBeInt FkIfZero Program Init".split()
)
_NO_FALLTHROUGH = frozenset({"Goto", "Halt", "Return", "EndCoroutine"})
_P2_IS_REGISTER = 0x02  # OPFLAG_P2ISREG on OpenRead/OpenWrite


def _scans(conn: apsw.Connection, sql: str, what: str, seconds: float) -> list[str]:
    """The tables and indexes `sql` reads end to end, from its bytecode.

    A cursor on a b-tree scans it when the program moves the cursor to its
    first or last entry (Rewind, Last) and steps it (Next, Prev) without ever
    seeking, or counts the b-tree (Count). Cursors are matched to objects by
    root page, so aliases do not matter. A Next or Prev that nothing reaches
    (the min()/max() shortcut reads one entry, then jumps past it) is not a
    step. The plan text is not used: it names aliases, and it calls some full
    reads (an unindexed max(), an automatic index's build) searches."""
    roots = {
        root: name
        for name, root in _run(
            conn, "SELECT name, rootpage FROM sqlite_schema WHERE rootpage > 0", what, seconds
        )
    }
    program = _run(conn, "EXPLAIN " + sql, what, seconds)
    targets = {row[3] for row in program if row[1] in _JUMPS}
    name: dict[int, str | None] = {}
    used: dict[int, set[str]] = {}
    order: list[str] = []
    for i, (addr, op, p1, p2, p3, _p4, p5, *_rest) in enumerate(program):
        if op in ("OpenRead", "OpenWrite"):
            name[p1] = roots.get(p2) if p3 == 0 and not p5 & _P2_IS_REGISTER else None
            used[p1] = set()
            if name[p1] and name[p1] not in order:
                order.append(name[p1])
        elif p1 in used:
            if op in ("Next", "Prev") and not (
                addr in targets or (i > 0 and program[i - 1][1] not in _NO_FALLTHROUGH)
            ):
                continue
            used[p1].add(op)
    scanned = set()
    for cursor, ops in used.items():
        loop = bool(ops & {"Rewind", "Last"}) and bool(ops & {"Next", "Prev"})
        seeks = any(op.startswith("Seek") for op in ops)
        if name[cursor] and ((loop and not seeks) or "Count" in ops):
            scanned.add(name[cursor])
    return [o for o in order if o in scanned]


def _vacuum(db_path: Path, seconds: float) -> None:
    """Rebuild our working copy so every table and index is contiguous.

    The guards stay on for scenario SQL: SQLite runs VACUUM by attaching a
    temporary database, so this fixed statement runs on its own connection
    without the ATTACH guard, still with the schema untrusted and a time limit."""
    conn = _open(db_path)
    try:
        _run(conn, "VACUUM", "--vacuum", seconds)
    finally:
        conn.close()


def capture(
    scenario: Scenario,
    workdir: Path,
    limits: Limits | None = None,
    rows: bool = True,
    sqlite_cache: Sequence[int] = (),
) -> dict[str, Any]:
    """Build the database, run the queries through the trace VFS, and return
    the trace. With `sqlite_cache` sizes, also run them through SQLite's own
    page cache at each size and record the pages it reads. SQLite's memory is
    capped at HEAP_LIMIT meanwhile."""
    previous = apsw.hard_heap_limit(HEAP_LIMIT)
    try:
        return _capture(scenario, workdir, limits, rows, sqlite_cache)
    finally:
        apsw.hard_heap_limit(previous)


def _capture(
    scenario: Scenario,
    workdir: Path,
    limits: Limits | None,
    rows: bool,
    sqlite_cache: Sequence[int],
) -> dict[str, Any]:
    limits = limits or Limits()
    if not scenario.queries:
        raise ValueError("no queries to run")
    # SQLite reports a symlinked path (macOS temp dirs live under /var -> /private/var)
    # with SQLITE_OK_SYMLINK, which APSW's Python VFS layer raises as an error.
    db_path = workdir.resolve() / f"{scenario.name}.db"
    before_path = db_path.with_name(f"{scenario.name}.before.db")
    for suffix in ("", "-wal", "-journal"):
        db_path.with_name(db_path.name + suffix).unlink(missing_ok=True)

    if scenario.db is not None:
        _copy_database(scenario.db, db_path, limits)
    setup = _open(db_path)
    try:
        if scenario.db is not None:
            _check_database(setup, scenario.db)
        for table, path in scenario.csvs:
            _import_csv(setup, table, path, limits.max_pages * 4096)
        for spec in scenario.indexes:
            _create_index(setup, spec)
        if scenario.setup.strip():
            _restrict(setup, limits.max_pages)
            seconds = max(SETUP_SECONDS, limits.timeout)
            _run(setup, scenario.setup, "setup", seconds, keep=False)
    finally:
        setup.close()
    if scenario.vacuum:
        _vacuum(db_path, max(SETUP_SECONDS, limits.timeout))
    page_size, objects_before, pages_before = _layout(db_path, limits)
    if rows or sqlite_cache:
        shutil.copyfile(db_path, before_path)

    vfs = TraceVFS(page_size)
    try:
        conn = _open(db_path, vfs=VFS_NAME)
        conn.execute("PRAGMA cache_size = 1; PRAGMA mmap_size = 0;")
        _restrict(conn, limits.max_pages)
        queries = []
        for title, sql in scenario.queries:
            what = f"query {title!r}"
            plan = [row[3] for row in _run(conn, "EXPLAIN QUERY PLAN " + sql, what, limits.timeout)]
            scans = _scans(conn, sql, what, limits.timeout)
            vfs.log.clear()
            _run(conn, sql, what, limits.timeout, vfs.log, limits.max_io, keep=False)
            queries.append(
                {"title": title, "sql": sql, "plan": plan, "scans": scans, "io": list(vfs.log)}
            )
        conn.close()
    finally:
        vfs.unregister()

    # Inserts can grow the file, so the page map describes it after the queries.
    _, objects, pages = _layout(db_path, limits)
    touched = sorted({page for q in queries for _, page in q["io"]})
    notes = list(NOTES) + ([VACUUM_NOTE] if scenario.vacuum else [])
    decoded: dict[int, dict[str, Any]] = {}
    if rows:
        known = [pages_before[p - 1] for p in touched if p <= len(pages_before)]
        decoded, skipped, cut = _decode(before_path, known, objects_before, limits.max_cell)
        new = sum(p > len(pages_before) for p in touched)
        if new:
            notes.append(
                f"{new} page(s) were created by the queries; they have no rows here because "
                "rows show the database before the queries ran."
            )
        if skipped:
            notes.append(f"{skipped} page(s) could not be decoded and have no rows.")
        if cut:
            notes.append(f"{cut} long value(s) were cut to {limits.max_cell} characters.")
    else:
        notes[2] = "Rows were left out of this trace (--no-rows)."
    trace: dict[str, Any] = {
        "format": "bpsim-trace",
        "version": TRACE_VERSION,
        "scenario": scenario.name,
        "sqlite_version": apsw.sqlite_lib_version(),
        "page_size": page_size,
        "page_count": len(pages),
        "objects": objects,
        "pages": pages,
        "queries": queries,
        "rows": {str(p): decoded[p] for p in touched if p in decoded},
        "notes": notes,
    }
    if sqlite_cache:
        sql = [query for _, query in scenario.queries]
        points = []
        for n in sorted(sqlite_cache):
            reads = sqlite_cache_reads(before_path, sql, n, page_size, workdir, limits)
            points.append({"pages": n, "reads": reads})
        trace["sqlite_cache"] = points
    return trace


def dump(trace: dict[str, Any]) -> str:
    """Serialise with one line per page, query and row-page so diffs stay readable."""

    def compact(value: Any) -> str:
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"))

    lines = ["{"]
    for i, (key, value) in enumerate(trace.items()):
        comma = "," if i < len(trace) - 1 else ""
        if isinstance(value, list) and value:
            items = [f"  {compact(v)}" for v in value]
            lines.append(f' "{key}": [\n' + ",\n".join(items) + f"\n ]{comma}")
        elif isinstance(value, dict) and value:
            items = [f"  {compact(k)}: {compact(v)}" for k, v in value.items()]
            lines.append(f' "{key}": {{\n' + ",\n".join(items) + f"\n }}{comma}")
        else:
            lines.append(f' "{key}": {compact(value)}{comma}')
    lines.append("}")
    return "\n".join(lines) + "\n"


# ---- command line ----


def _safe_name(text: str) -> str:
    return re.sub(r"[^A-Za-z0-9_-]+", "-", text).strip("-") or "trace"


def _sql_title(sql: str) -> str:
    flat = " ".join(sql.split())
    return flat if len(flat) <= 48 else flat[:47] + "…"


def build_scenario(args: argparse.Namespace) -> Scenario:
    """Combine a scenario file, `--db`, `--csv`, `--index` and `--sql` into one scenario."""
    own_data = args.db is not None or bool(args.csv)
    if args.scenario is not None:
        text = args.scenario.read_text(encoding="utf-8")
        scenario = parse_scenario(
            text, args.scenario.stem, setup_required=not own_data, seed=args.seed
        )
    elif own_data:
        scenario = Scenario(name="", setup="", queries=[])
    else:
        raise ValueError("give a scenario .sql file, or your own data with --db or --csv")
    for spec in args.csv:
        table, sep, path = spec.partition("=")
        if not sep or not path:
            raise ValueError(f"--csv: expected table=file.csv, got {spec!r}")
        scenario.csvs.append((table, Path(path)))
    scenario.db = args.db
    scenario.indexes = list(args.index)
    scenario.queries += [
        (_sql_title(s), _check_single_statement(_sql_title(s), s)) for s in args.sql
    ]
    if not scenario.queries:
        raise ValueError("no queries: add --sql \"...\" or '-- query:' sections in a scenario file")
    fallback = args.db.stem if args.db is not None else scenario.csvs[0][0] if args.csv else ""
    scenario.name = _safe_name(args.name or scenario.name or fallback)
    if args.vacuum:
        scenario.vacuum = True
        scenario.name += "-vacuum"
    return scenario


def view(trace_path: Path, frames: int, policy: str, runs: Path | None = None) -> Path:
    """Replay the trace with the engine and return the viewer page it wrote
    under `runs` (default outputs/runs)."""
    sim = REPO / "sim" / "target" / "release" / ("sim.exe" if sys.platform == "win32" else "sim")
    if not sim.is_file():
        raise ValueError("--view needs the engine: run `cargo build --release` in sim/ first")
    cmd = [str(sim), "--trace", str(trace_path), "--policy", policy, "--frames", str(frames)]
    cmd += ["--out", str(runs or REPO / "outputs" / "runs")]
    done = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if done.returncode != 0:
        raise ValueError(done.stderr.strip().removeprefix("error: ") or "the engine failed")
    run_dir = None
    for line in done.stdout.splitlines():
        if line.startswith("Run written to "):
            run_dir = Path(line.removeprefix("Run written to "))
        else:
            print(line)
    if run_dir is None:
        raise ValueError("the engine did not report where it wrote the run")
    return run_dir / "viewer.html"


def _sizes(text: str) -> list[int]:
    """Comma-separated page counts, for --sqlite-cache."""
    try:
        sizes = [int(n) for n in text.split(",")]
    except ValueError:
        raise argparse.ArgumentTypeError(f"expected page counts like 64,256: {text!r}") from None
    if min(sizes) < 1 or len(set(sizes)) != len(sizes):
        raise argparse.ArgumentTypeError(f"page counts must be positive and different: {text!r}")
    return sorted(sizes)


def main() -> None:
    defaults = Limits()
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("scenario", type=Path, nargs="?", help="scenario .sql file")
    parser.add_argument(
        "-o", "--out", type=Path, help="trace to write: .json, or .json.gz to compress it"
    )
    parser.add_argument(
        "--seed", type=int, help="replace the seed of the scenario's workload (a local variant)"
    )
    parser.add_argument(
        "--sqlite-cache",
        type=_sizes,
        default=[],
        metavar="PAGES,...",
        help="also run the queries through SQLite's own page cache at these sizes and record "
        "the pages it reads: an outside check on the engine's LRU (slower: once per size)",
    )
    data = parser.add_argument_group("your own data")
    data.add_argument("--db", type=Path, help="existing SQLite file (a copy is used)")
    data.add_argument(
        "--csv", action="append", default=[], metavar="TABLE=FILE", help="CSV → table"
    )
    data.add_argument("--index", action="append", default=[], metavar="TABLE(COL,...)")
    data.add_argument("--sql", action="append", default=[], metavar="STATEMENT", help="a query")
    data.add_argument("--name", help="scenario name (letters, digits, - and _)")
    data.add_argument("--no-rows", action="store_true", help="leave row contents out of the trace")
    data.add_argument(
        "--vacuum",
        action="store_true",
        help="rebuild the database with VACUUM before the queries (name gets -vacuum)",
    )
    lim = parser.add_argument_group("limits")
    lim.add_argument("--max-pages", type=int, default=defaults.max_pages)
    lim.add_argument("--timeout", type=float, default=defaults.timeout, help="seconds per query")
    lim.add_argument("--max-cell", type=int, default=defaults.max_cell, help="characters per value")
    lim.add_argument("--max-io", type=int, default=defaults.max_io, help="page I/Os per query")
    show = parser.add_argument_group("viewer")
    show.add_argument(
        "--view", action="store_true", help="replay with the engine, write viewer.html"
    )
    show.add_argument(
        "--runs", type=Path, help="where --view writes its run (default outputs/runs)"
    )
    show.add_argument("--frames", type=int, default=8, help="buffer pool size for --view")
    show.add_argument("--policy", default="all", help="policies for --view (default: all)")
    args = parser.parse_args()

    try:
        scenario = build_scenario(args)
        if min(args.max_pages, args.max_cell, args.max_io, args.frames) < 1 or args.timeout <= 0:
            raise ValueError("limits and --frames must be positive")
        limits = Limits(args.max_pages, args.timeout, args.max_cell, args.max_io)
        with tempfile.TemporaryDirectory() as tmp:
            trace = capture(scenario, Path(tmp), limits, not args.no_rows, args.sqlite_cache)
        out = args.out or REPO / "outputs" / "traces" / f"{scenario.name}.trace.json"
        out.parent.mkdir(parents=True, exist_ok=True)
        if out.name.endswith(".gz"):
            # mtime 0: the same trace always gives the same bytes
            out.write_bytes(gzip.compress(dump(trace).encode("utf-8"), mtime=0))
        else:
            out.write_text(dump(trace), encoding="utf-8")
        reads = sum(op == "r" for q in trace["queries"] for op, _ in q["io"])
        writes = sum(op == "w" for q in trace["queries"] for op, _ in q["io"])
        print(
            f"{scenario.name}: {len(trace['queries'])} queries, {reads} page reads, "
            f"{writes} writes, {trace['page_count']} pages -> {out}"
        )
        for point in trace.get("sqlite_cache", []):
            print(f"  SQLite's own cache at {point['pages']} pages read {point['reads']} pages")
        if trace["rows"] and (args.db is not None or args.csv):
            print(
                "note: rows from your data are stored in the trace and the viewer page; "
                "--no-rows leaves them out"
            )
        if args.view:
            frames = min(args.frames, trace["page_count"])
            print(f"viewer -> {view(out, frames, args.policy, args.runs)}")
    except (ValueError, OSError) as e:
        sys.exit(f"error: {e}")


if __name__ == "__main__":
    main()
