"""The engine against the independent model in verify/ (see verify/model.py).

The comparison covers every shipped trace and a free-page scenario: every
policy, pool sizes 1-32, both drives, read-ahead auto/off/1/3 (about 2,800
runs, ~10 s), and every policy at one size of a sweep of a workload trace
(~30 s). A short fuzz adds random traces (verify/fuzz.py, ~10 s). Both build
the release engine first, so they never test a stale binary."""

from __future__ import annotations

import shutil
import sys
from pathlib import Path

import pytest

VERIFY = Path(__file__).resolve().parents[2] / "verify"
sys.path.insert(0, str(VERIFY))
import compare  # noqa: E402
import fuzz  # noqa: E402

needs_engine = pytest.mark.skipif(
    not shutil.which("cargo") and not compare.SIM.is_file(), reason="no Rust and no built engine"
)


@needs_engine
def test_engine_agrees_with_the_independent_model() -> None:
    runs, mismatches = compare.compare()
    assert runs > 2000
    assert not mismatches, "\n".join(mismatches[:10])


@needs_engine
def test_a_sweep_agrees_with_the_independent_model() -> None:
    runs, mismatches = compare.compare_sweep()
    assert runs == 18
    assert not mismatches, "\n".join(mismatches[:10])


@needs_engine
def test_engine_agrees_with_the_model_on_random_traces() -> None:
    r = fuzz.fuzz(seed=1, count=25)
    assert r["runs"] > 5000
    assert not r["problems"], "\n".join(r["problems"][:10])
