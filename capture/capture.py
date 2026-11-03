"""Capture the page-level I/O SQLite performs for a scenario of queries.

A scenario is one SQL file: a `-- setup` section that builds the database,
then `-- query: <title>` sections, one statement each. The tool builds the
database, then replays the queries through a VFS shim that logs every read
and write SQLite makes on the main database file. The result is a trace the
Rust engine can replay through any buffer pool.

Trace format (JSON, version 1):
    format, version, scenario, sqlite_version, page_size, page_count
    objects  [{name, type, table}]                 tables and indexes
    pages    [{page, object, kind}]                kind: internal | leaf | overflow | free
    queries  [{title, sql, plan, io}]              io: [["r"|"w", page], ...] in order
    rows     {"<page>": {object, columns, rows}}   decoded page contents, for pages queries touch
    notes    [str]                                 how to read the trace

Usage:
    python capture/capture.py capture/scenarios/shop.sql -o traces/shop.trace.json
"""

from __future__ import annotations

import argparse
import json
import struct
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import apsw

TRACE_VERSION = 1
VFS_NAME = "bpsim-trace"

NOTES = [
    "Page 1 holds the file header; SQLite reads it at the start of every statement to check "
    "whether the file changed since its last look.",
    "Reads are what SQLite asked the file for with its own page cache at the minimum size, so "
    "almost every page reference is visible. Writes are logged when the statement commits.",
    "Rows show each page as it was right after setup, before any query ran.",
]


@dataclass
class Scenario:
    name: str
    setup: str
    queries: list[tuple[str, str]]


def parse_scenario(text: str, name: str) -> Scenario:
    """Split a scenario file into its setup SQL and titled queries."""
    setup: list[str] | None = None
    queries: list[tuple[str, list[str]]] = []
    current: list[str] | None = None
    for lineno, line in enumerate(text.splitlines(), start=1):
        stripped = line.strip()
        if stripped == "-- setup":
            if setup is not None:
                raise ValueError(f"line {lineno}: a scenario has only one '-- setup' section")
            setup = current = []
        elif stripped.startswith("-- query:"):
            if setup is None:
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
    if setup is None:
        raise ValueError("scenario has no '-- setup' section")
    if not queries:
        raise ValueError("scenario has no '-- query:' sections")
    parsed = []
    for title, lines in queries:
        sql = "\n".join(lines).strip()
        if not sql:
            raise ValueError(f"query {title!r} is empty")
        if ";" in sql.rstrip(";"):
            raise ValueError(f"query {title!r} must be a single statement")
        parsed.append((title, sql))
    return Scenario(name=name, setup="\n".join(setup), queries=parsed)


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
            values.append(payload[pos : pos + n].decode("utf-8"))
            pos += n
        else:
            raise ValueError(f"unsupported serial type {serial}")
    return values


class PageReader:
    """Decodes the cells of b-tree pages in a database file."""

    def __init__(self, data: bytes):
        self.data = data
        self.page_size = struct.unpack(">H", data[16:18])[0] or 65536
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
        while next_page and len(out) < size:
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
        kind = page[base]
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


def _columns(conn: apsw.Connection, obj: dict[str, str]) -> tuple[list[str], int | None]:
    """Column names for decoded rows, and the rowid-alias column index if any."""
    if obj["type"] == "index":
        names = [r[2] for r in conn.execute(f"PRAGMA index_info({json.dumps(obj['name'])})")]
        return names + ["rowid"], None
    info = list(conn.execute(f"PRAGMA table_info({json.dumps(obj['name'])})"))
    names = [r[1] for r in info]
    pk = [r for r in info if r[5]]
    alias = info.index(pk[0]) if len(pk) == 1 and pk[0][2].upper() == "INTEGER" else None
    return names, alias


def decode_rows(
    db_path: Path, pages: list[dict[str, Any]], objects: list[dict[str, str]]
) -> dict[int, dict[str, Any]]:
    """Decode every b-tree page of every table and index into {page: {...}}.

    Table interior pages become routing rows (child page, highest rowid on the
    left of it); the last row is the right-most child, which has no key.
    Index interior pages also list their child before each key.
    """
    reader = PageReader(db_path.read_bytes())
    by_name = {o["name"]: o for o in objects}
    conn = apsw.Connection(str(db_path), flags=apsw.SQLITE_OPEN_READONLY)
    decoded: dict[int, dict[str, Any]] = {}
    for p in pages:
        obj = by_name.get(p["object"])
        if p["kind"] not in ("leaf", "internal") or obj is None:
            continue
        names, alias = _columns(conn, obj)
        kind, cells = reader.cells(p["page"])
        if kind == 0x05:
            columns = ["child_page", "rowid_up_to"]
            rows = [[c["child"], c.get("rowid")] for c in cells]
        elif kind == 0x02:
            columns = ["child_page", *names]
            rows = [[c["child"], *c.get("values", [None] * len(names))] for c in cells]
        elif kind == 0x0A:
            columns, rows = names, [c["values"] for c in cells]
        else:
            columns = names if alias is not None else ["rowid", *names]
            rows = []
            for c in cells:
                values = c["values"]
                if alias is not None:
                    values[alias] = c["rowid"]
                else:
                    values = [c["rowid"], *values]
                rows.append(values)
        decoded[p["page"]] = {"object": obj["name"], "columns": columns, "rows": rows}
    conn.close()
    return decoded


# ---- capture ----


def capture(scenario: Scenario, workdir: Path) -> dict[str, Any]:
    # SQLite reports a symlinked path (macOS temp dirs live under /var -> /private/var)
    # with SQLITE_OK_SYMLINK, which APSW's Python VFS layer raises as an error.
    db_path = workdir.resolve() / f"{scenario.name}.db"
    db_path.unlink(missing_ok=True)

    setup = apsw.Connection(str(db_path))
    setup.execute(scenario.setup)
    page_size = setup.execute("PRAGMA page_size").fetchall()[0][0]
    page_count = setup.execute("PRAGMA page_count").fetchall()[0][0]
    objects = [
        {"name": name, "type": kind, "table": table}
        for name, kind, table in setup.execute(
            "SELECT name, type, tbl_name FROM sqlite_schema "
            "WHERE type IN ('table', 'index') ORDER BY rootpage"
        )
    ]
    owners = {
        pageno: {"page": pageno, "object": name, "kind": kind}
        for name, pageno, kind in setup.execute(
            "SELECT name, pageno, pagetype FROM dbstat ORDER BY pageno"
        )
    }
    setup.close()
    pages = [
        owners.get(n, {"page": n, "object": None, "kind": "free"}) for n in range(1, page_count + 1)
    ]
    rows = decode_rows(db_path, pages, objects)

    vfs = TraceVFS(page_size)
    try:
        conn = apsw.Connection(str(db_path), vfs=VFS_NAME)
        conn.execute("PRAGMA cache_size = 1; PRAGMA mmap_size = 0;")
        queries = []
        for title, sql in scenario.queries:
            plan = [row[3] for row in conn.execute("EXPLAIN QUERY PLAN " + sql)]
            vfs.log.clear()
            for _ in conn.execute(sql):
                pass
            queries.append({"title": title, "sql": sql, "plan": plan, "io": list(vfs.log)})
        conn.close()
    finally:
        vfs.unregister()

    touched = sorted({page for q in queries for _, page in q["io"]})
    return {
        "format": "bpsim-trace",
        "version": TRACE_VERSION,
        "scenario": scenario.name,
        "sqlite_version": apsw.sqlite_lib_version(),
        "page_size": page_size,
        "page_count": page_count,
        "objects": objects,
        "pages": pages,
        "queries": queries,
        "rows": {str(p): rows[p] for p in touched if p in rows},
        "notes": NOTES,
    }


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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("scenario", type=Path, help="scenario .sql file")
    parser.add_argument("-o", "--out", type=Path, required=True, help="trace .json to write")
    args = parser.parse_args()

    scenario = parse_scenario(args.scenario.read_text(encoding="utf-8"), args.scenario.stem)
    with tempfile.TemporaryDirectory() as tmp:
        trace = capture(scenario, Path(tmp))
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(dump(trace), encoding="utf-8")
    reads = sum(op == "r" for q in trace["queries"] for op, _ in q["io"])
    writes = sum(op == "w" for q in trace["queries"] for op, _ in q["io"])
    print(
        f"{scenario.name}: {len(trace['queries'])} queries, {reads} page reads, {writes} writes, "
        f"{trace['page_count']} pages -> {args.out}"
    )


if __name__ == "__main__":
    main()
