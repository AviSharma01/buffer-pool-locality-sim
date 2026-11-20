"""Compare the engine with the independent model (model.py) on every shipped
trace plus a free-page scenario: every policy, pool sizes from 1 to 32,
both drives, read-ahead auto, off, 1 and 3 pages. About 2,200 runs, ~10 s.

    python verify/compare.py            # builds the release engine first

Optimal is compared on hits only: when several pages are never needed again,
any of them is an optimal victim, and the two may pick different ones (same
hits, different write-backs).
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
sys.path.insert(0, str(HERE))
import model  # noqa: E402

SIM = REPO / "sim" / "target" / "release" / "sim"
TRACES = ["shop", "checkout", "growth", "growth-vacuum", "shop-vacuum"]
FRAMES = [1, 2, 3, 4, 6, 8, 12, 16, 24, 32]
READ_AHEAD = ["auto", "off", "1", "3"]
FIELDS = [
    "hits",
    "next_page_reads",
    "ahead_reads",
    "jump_reads",
    "fresh_pages",
    "dirty_evictions",
    "dirty_at_end",
    "pages_read_ahead",
    "read_ahead_hits",
    "read_ahead_unused",
    "read_requests",
]


def build() -> Path:
    """Build the release engine, so the check never runs a stale binary.
    Without cargo, an existing binary is used as it is."""
    if shutil.which("cargo"):
        cmd = ["cargo", "build", "--release", "--quiet"]
        subprocess.run(cmd, cwd=REPO / "sim", check=True)
    if not SIM.is_file():
        raise FileNotFoundError(f"no engine at {SIM}: install Rust, or build it with cargo")
    return SIM


def traces(work: Path) -> dict[str, Path]:
    """The shipped traces, plus freed.sql captured fresh."""
    found = {name: REPO / "traces" / f"{name}.trace.json" for name in TRACES}
    freed = work / "freed.trace.json"
    capture = [sys.executable, str(REPO / "capture" / "capture.py"), str(HERE / "freed.sql")]
    subprocess.run([*capture, "-o", str(freed)], check=True, capture_output=True)
    found["freed"] = freed
    return found


def check_run(run_file: Path, path: Path, frames: int, storage: str, read_ahead: str) -> list[str]:
    """Mismatches between one engine run file (all policies) and the model."""
    mismatches = []
    for r in json.loads(run_file.read_text(encoding="utf-8"))["runs"]:
        got, policy = r["summary"], r["policy"]
        want = model.run(path, policy, frames, storage, read_ahead)
        keys = ["hits"] if policy == "opt" else FIELDS
        bad = [k for k in keys if got[k] != want[k]]
        ms_off = abs(got["total_ms"] - want["total_ms"]) > 1e-6 * max(1.0, want["total_ms"])
        if policy != "opt" and ms_off:
            bad.append("total_ms")
        if bad:
            trace = path.name.split(".")[0]
            what = f"{trace}, {policy}, {frames} frames, {storage}, read-ahead {read_ahead}"
            diffs = ", ".join(f"{k} engine {got[k]} model {want[k]}" for k in bad)
            mismatches.append(f"{what}: {diffs}")
    return mismatches


def compare(sim: Path | None = None) -> tuple[int, list[str]]:
    """Returns (runs compared, mismatch descriptions). Builds the engine
    unless given one."""
    sim = sim or build()
    runs, mismatches = 0, []
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        out = work / "runs"
        for path in traces(work).values():
            pages = json.loads(path.read_text(encoding="utf-8"))["page_count"]
            for frames in [f for f in FRAMES if f <= pages]:
                for storage in model.PROFILES:
                    for read_ahead in READ_AHEAD:
                        shutil.rmtree(out, ignore_errors=True)
                        args = ["--trace", str(path), "--policy", "all", "--frames", str(frames)]
                        args += ["--storage", storage, "--read-ahead", read_ahead]
                        cmd = [str(sim), *args, "--out", str(out)]
                        subprocess.run(cmd, check=True, capture_output=True)
                        run_file = next(out.glob("*/compare/*/run.json"))
                        mismatches += check_run(run_file, path, frames, storage, read_ahead)
                        runs += len(model.POLICIES) + 1
    return runs, mismatches


def main() -> int:
    try:
        sim = build()
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        print(f"cannot build the engine: {e}")
        return 2
    runs, mismatches = compare(sim)
    for m in mismatches[:20]:
        print("MISMATCH", m)
    print(f"{runs} runs compared, {len(mismatches)} mismatches (Optimal: hits only)")
    return 1 if mismatches else 0


if __name__ == "__main__":
    sys.exit(main())
