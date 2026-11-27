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

LABELS = [label for label, _, _ in findings.VARIANTS]


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


def good() -> tuple[dict, dict]:
    m = {}
    for label in LABELS:
        m["reports", label] = run(1000, 760, 780, 470)
        m["oltp", label] = run(1000, 920, 990, 600)
        m["ranges", label] = run(1000, 910, 980, 600)
    uses = {
        (t, label): {
            "lru": {"1": 10, "2–9": 500, "10–99": 300, "100+": 5},
            "innodb": {"1": 10, "2–9": 510, "10–99": 150, "100+": 5},
        }
        for t in ("oltp", "ranges")
        for label in LABELS
    }
    return m, uses


def test_good_numbers_pass() -> None:
    m, uses = good()
    texts, failed = findings.claims(m, uses, 45)
    assert failed == []
    assert "22–24% fewer pages" in texts["summary"]
    assert "within 0% of LRU), and InnoDB-style's" in texts["summary"]
    assert "22–24% fewer pages from disk than LRU" in texts["headline"]
    assert "re-reads at most 45 pages" in texts["limit"]


@pytest.mark.parametrize(
    ("change", "claim"),
    [
        (lambda m, u: m.__setitem__(("reports", "seed 11"), run(1000, 900, 780, 470)), "15%+"),
        (lambda m, u: m.__setitem__(("oltp", "skew 0.9"), run(1000, 920, 950, 600)), "within 3%"),
        (lambda m, u: m.__setitem__(("ranges", "seed 23"), run(1000, 990, 980, 600)), "3%+ less"),
        # saves plenty, but on pages few queries use
        (
            lambda m, u: u["oltp", "skew 1.3"]["innodb"].update({"2–9": 300, "10–99": 290}),
            "pages 10+ queries use carry 40%+",
        ),
        # the ring is not what helps on reports
        (
            lambda m, u: m.__setitem__(
                ("reports", "seed 23"), run(1000, 760, 780, 470, {"pg:ring=off": 850})
            ),
            "without its ring",
        ),
        # the later-query rule changes a read somewhere
        (
            lambda m, u: m.__setitem__(
                ("ranges", "skew 0.9"), run(1000, 910, 980, 600, {"innodb:promote=any": 911})
            ),
            "any re-use reads exactly",
        ),
        # entering at the head still saves
        (
            lambda m, u: m.__setitem__(
                ("oltp", "seed 11"), run(1000, 920, 990, 600, {"innodb:old=95": 970})
            ),
            "entering at the head",
        ),
        (lambda m, u: m.__setitem__(("oltp", "seed 7"), run(1000, 920, 990, 800)), "30%+"),
    ],
)
def test_numbers_that_contradict_a_claim_fail_it(change, claim: str) -> None:
    m, uses = good()
    change(m, uses)
    _, failed = findings.claims(m, uses, 45)
    assert any(claim in f for f in failed), failed


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
        findings.capture("oltp", "seed 11", 11, None)
    path.write_bytes(gzip.compress(json.dumps({"variant": {"seed": 11}}).encode()))
    assert findings.capture("oltp", "seed 11", 11, None) == path
    skew = {"params": ["user=zipf(0.9, 1..100000)"]}
    path.write_bytes(gzip.compress(json.dumps({"variant": skew}).encode()))
    assert findings.capture("oltp", "skew 0.9", None, "user=zipf(0.9,  1..100000)") == path


def test_the_saving_share_says_all_rather_than_over_100_percent() -> None:
    text = findings.warm_text({("oltp", "seed 7"): 1.15, ("ranges", "skew 1.3"): 0.46})
    assert text == (
        "Pages used by 10 or more queries account for 46% to all of the saving, "
        "the least on `ranges` at skew 1.3."
    )
    text = findings.warm_text({("oltp", "seed 7"): 0.9, ("ranges", "seed 7"): 0.75})
    assert "75% to 90% of the saving, the least on `ranges` at seed 7" in text
