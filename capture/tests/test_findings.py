"""verify/findings.py fails when the numbers contradict a claim in FINDINGS.md.

Each case starts from numbers that satisfy every claim and breaks one.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "verify"))
import findings  # noqa: E402
from findings import Measured, Point  # noqa: E402


def run(lru: int, innodb: int, pg: int, opt: int, changed: dict | None = None) -> Measured:
    """Reads at every pool. Changed policies default to what the claims say:
    promote=any reads as InnoDB-style, old=95 and ring=off as LRU."""
    pools = {"1/8": 100, "1/4": 200, "1/2": 400}
    reads = {"lru": lru, "innodb": innodb, "pg": pg, "opt": opt}
    reads |= {"innodb:promote=any": innodb, "pg:ring=off": lru}
    reads |= {f"innodb:old={p}": lru if p == 95 else innodb for p in findings.OLD_SHARES}
    reads |= changed or {}
    points = {(pool, p): Point(r, 300) for pool in pools for p, r in reads.items()}
    return Measured(800, pools, points)


GOOD = {
    "reports": lambda: run(1000, 760, 780, 470),
    "analytics": lambda: run(1000, 700, 720, 400, {"innodb:promote=any": 730}),
    "oltp": lambda: run(1000, 920, 990, 600),
    "ranges": lambda: run(1000, 910, 980, 600),
    "warehouse": lambda: run(1000, 960, 985, 700),
}
REREAD = {"oltp": 45, "reports": 45, "ranges": 0, "warehouse": 351, "analytics": 4210}


def good() -> tuple[dict, dict]:
    m = {(t, v): GOOD[t]() for t, v in findings.variants()}
    uses = {
        (t, v): {
            "lru": {"1": 10, "2–9": 500, "10–99": 300, "100+": 5},
            "innodb": {"1": 10, "2–9": 510, "10–99": 150, "100+": 5},
        }
        for t, v in findings.variants()
        if t in findings.NO_SCANS
    }
    return m, uses


def claims(m: dict, uses: dict, gap: dict | None = None) -> tuple[dict, list[str]]:
    return findings.claims(m, uses, REREAD, gap or {"innodb": 0, "pg": 0})


def test_good_numbers_pass() -> None:
    m, uses = good()
    texts, failed = claims(m, uses)
    assert failed == []
    assert "22–30% fewer pages" in texts["summary"]
    assert "within 0% of LRU), and InnoDB-style's" in texts["summary"]
    assert "22–30% fewer pages from disk than LRU" in texts["headline"]
    assert "re-reads at most 45 pages on the first three and 351 on `warehouse`" in texts["limit"]
    assert "4,210 pages" in texts["limit"]


def put(trace: str, label: str, *args, **changed):
    return lambda m, u: m.__setitem__((trace, label), run(*args, changed or None))


@pytest.mark.parametrize(
    ("change", "claim"),
    [
        (put("analytics", "seed 11", 1000, 900, 720, 400), "15%+"),
        (put("warehouse", "seed 23", 1000, 960, 950, 700), "within 4%"),
        (put("ranges", "seed 23", 1000, 990, 980, 600), "2.5%+ less"),
        # saves plenty, but on pages few queries use
        (
            lambda m, u: u["oltp", "skew 1.3"]["innodb"].update({"2–9": 300, "10–99": 290}),
            "pages 10+ queries use carry 40%+",
        ),
        (
            lambda m, u: u["warehouse", "seed 7"]["innodb"].update({"2–9": 700}),
            "saves reads at 1/4",
        ),
        # the ring is not what helps
        (
            lambda m, u: m.__setitem__(
                ("analytics", "seed 23"),
                run(1000, 700, 720, 400, {"pg:ring=off": 850, "innodb:promote=any": 730}),
            ),
            "without its ring",
        ),
        # the later-query rule changes a read where it shouldn't, or doesn't help
        (
            lambda m, u: m.__setitem__(
                ("ranges", "skew 0.9"), run(1000, 910, 980, 600, {"innodb:promote=any": 911})
            ),
            "reads exactly the same",
        ),
        (
            lambda m, u: m.__setitem__(
                ("warehouse", "seed 11"), run(1000, 960, 985, 700, {"innodb:promote=any": 962})
            ),
            "under 0.1%",
        ),
        (put("analytics", "seed 7", 1000, 700, 720, 400), "the later-query rule saves reads"),
        # entering at the head still saves
        (
            lambda m, u: m.__setitem__(
                ("oltp", "seed 11"), run(1000, 920, 990, 600, {"innodb:old=95": 970})
            ),
            "saves at most 2%",
        ),
        (put("warehouse", "seed 7", 1000, 960, 985, 850), "20%+"),
    ],
)
def test_numbers_that_contradict_a_claim_fail_it(change, claim: str) -> None:
    m, uses = good()
    change(m, uses)
    _, failed = claims(m, uses)
    assert any(claim in f for f in failed), failed


def test_a_small_share_on_warehouse_is_reported_not_failed() -> None:
    m, uses = good()
    uses["warehouse", "seed 7"]["innodb"].update({"2–9": 400, "10–99": 290})
    texts, failed = claims(m, uses)
    assert failed == []
    assert "On `warehouse`, whose saving is smaller," in texts["summary"]


def test_the_model_must_agree_on_analytics() -> None:
    m, uses = good()
    _, failed = claims(m, uses, {"innodb": 0, "pg": 3})
    assert failed == ["the independent model reads what the engine does on analytics (pg)"]


def test_the_document_must_have_every_block_and_no_others() -> None:
    doc = "<!-- findings:summary -->\nold\n<!-- /findings:summary -->\n"
    assert findings.fill(doc, {"summary": "new"}, "README.md") == (
        "<!-- findings:summary -->\nnew\n<!-- /findings:summary -->\n"
    )
    with pytest.raises(SystemExit, match="README.md has no block for"):
        findings.fill(doc, {"summary": "new", "results": "x"}, "README.md")
    with pytest.raises(SystemExit, match="README.md has a block findings.py doesn't write"):
        findings.fill(doc, {"headline": "new"}, "README.md")


def test_a_reused_variant_must_be_the_one_it_should_be(tmp_path: Path, monkeypatch) -> None:
    import gzip
    import json

    path = tmp_path / "oltp-seed-11.trace.json.gz"
    monkeypatch.setattr(findings, "trace_path", lambda trace, label: path)
    path.write_bytes(gzip.compress(json.dumps({"variant": {"seed": 23}}).encode()))
    with pytest.raises(SystemExit, match="records {'seed': 23}; run with --recapture"):
        findings.capture("oltp", "seed 11")
    path.write_bytes(gzip.compress(json.dumps({"variant": {"seed": 11}}).encode()))
    assert findings.capture("oltp", "seed 11") == path
    skew = {"params": ["user=zipf(0.9, 1..100000)"]}
    path.write_bytes(gzip.compress(json.dumps({"variant": skew}).encode()))
    assert findings.capture("oltp", "skew 0.9") == path
    path.write_bytes(gzip.compress(json.dumps({}).encode()))
    assert findings.capture("analytics", "seed 7") == path, "uncommitted: captured, no variant"


def test_only_committed_traces_are_read_from_traces() -> None:
    assert findings.trace_path("oltp", "seed 7") == findings.REPO / "traces/oltp.trace.json.gz"
    assert findings.trace_path("analytics", "seed 7").parent == findings.VARIANT_DIR
    assert ("warehouse", "skew 0.9") not in findings.variants()
    assert len(findings.variants()) == 3 * 5 + 2 * 3


def test_the_saving_share_says_all_rather_than_over_100_percent() -> None:
    text = findings.warm_text({("oltp", "seed 7"): 1.15, ("ranges", "skew 1.3"): 0.46})
    assert text == (
        "pages used by 10 or more queries account for 46% to all of the saving "
        "(the least on `ranges` at skew 1.3)."
    )
    text = findings.warm_text({("oltp", "seed 7"): 0.9, ("ranges", "seed 7"): 0.75})
    assert "75% to 90% of the saving (the least on `ranges` at seed 7)" in text
