"""The engine against the independent model in verify/ (see verify/model.py):
every shipped trace and a free-page scenario, every policy, pool sizes 1-32,
both drives, read-ahead auto/off/1/3. About 2,800 runs, ~10 s."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

VERIFY = Path(__file__).resolve().parents[2] / "verify"
sys.path.insert(0, str(VERIFY))
import compare  # noqa: E402


@pytest.mark.skipif(not compare.SIM.is_file(), reason="engine not built")
def test_engine_agrees_with_the_independent_model() -> None:
    runs, mismatches = compare.compare()
    assert runs > 2000
    assert not mismatches, "\n".join(mismatches[:10])
