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

That makes it a coarse check: it catches a pool that caches the wrong pages
(evicting the most recent page fails it by 5 to 7 reads), but on these traces
FIFO and LRU differ by less than the tolerance. The exact LRU check is
guarantees_hold_on_random_traces in sim/tests/engine.rs.
"""

from __future__ import annotations

import json
import shutil
import subprocess
from pathlib import Path

import apsw
import pytest

import capture as cap

HERE = Path(__file__).parent.parent
TRACES = HERE.parent / "traces"
SIM = cap.REPO / "sim" / "target" / "release" / "sim"
TOLERANCE = 3


class _CountingFile(apsw.VFSFile):
    def __init__(self, vfs: _CountingVFS, name: apsw.URIFilename | str | None, flags: list[int]):
        self._vfs = vfs
        self._main = bool(flags[0] & apsw.mapping_open_flags["SQLITE_OPEN_MAIN_DB"])
        super().__init__("", name, flags)

    def xRead(self, amount: int, offset: int) -> bytes:
        if self._main and amount == self._vfs.page_size:
            self._vfs.full_reads += 1
        return super().xRead(amount, offset)


class _CountingVFS(apsw.VFS):
    def __init__(self, page_size: int):
        self.page_size = page_size
        self.full_reads = 0
        super().__init__("bpsim-count", "")

    def xOpen(self, name: apsw.URIFilename | str | None, flags: list[int]) -> _CountingFile:
        return _CountingFile(self, name, flags)


def sqlite_reads(scenario: cap.Scenario, built: Path, work: Path, n: int) -> int:
    db = work / "run.db"
    shutil.copyfile(built, db)
    vfs = _CountingVFS(4096)
    try:
        conn = cap._open(db, vfs="bpsim-count")
        conn.execute(f"PRAGMA cache_size = {n}; PRAGMA mmap_size = 0;")
        for _, sql in scenario.queries:
            list(conn.execute(sql))
        conn.close()
    finally:
        vfs.unregister()
    return vfs.full_reads


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
    text = (HERE / "scenarios" / f"{name}.sql").read_text(encoding="utf-8")
    scenario = cap.parse_scenario(text, name)
    built = tmp_path / "built.db"
    setup = cap._open(built)
    cap._restrict(setup, cap.Limits().max_pages)
    cap._run(setup, scenario.setup, "setup", cap.SETUP_SECONDS)
    setup.close()
    for n in [4, 8, 16, 24]:
        ours = engine_reads(name, n, tmp_path / "runs")
        theirs = sqlite_reads(scenario, built, tmp_path, n)
        assert abs(ours - theirs) <= TOLERANCE, (
            f"{name} at {n} pages: engine {ours}, SQLite {theirs}"
        )
