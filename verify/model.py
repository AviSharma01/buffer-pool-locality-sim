"""An independent model of the engine, for checking it.

Written from the rules in CLAUDE.md and the step plans, never from the engine's
code, so a mistake in the engine shows up as a disagreement instead of being
copied. The six policies were written by a separate reviewer that had not seen
how the engine was built (2026-11-18). The pricing (3.4b) and read-ahead (3.4c)
rules were added afterwards from the approved plans. So this model checks that
the engine does what the plans say; it cannot show that the plans match real
systems. That is what the sources in CLAUDE.md and linux_readahead.py are for.

Rules, in brief:

- Accesses come from the captured trace. A write folds into the latest read of
  the same page in its query; a write with no earlier read is "fresh": a miss on
  it fills a frame with no disk read.
- Disk time: a read request costs a fixed part plus one transfer per page. The
  fixed part is a full request after a jump (backwards, or the first read), or,
  further on from the last page read, an overhead plus the skipped pages'
  transfer (never more than a full request). A dirty eviction is a separate
  write request and does not move the read stream.
- Read-ahead: a miss that continues its table or index (that object's previous
  access was the page before) reads the following pages of the same object, up
  to min(8, frames // 4), stopping at another object, a free page or a cached
  page, in the same request. PostgreSQL-style triggers on any scan miss and caps
  a big scan at its ring size minus one. Optimal never reads ahead.
- While a request reads ahead, the page it was made for is pinned: no policy
  evicts it (Clock and the PostgreSQL sweep pass over it untouched). LRU then
  counts that page as used, so the pages read ahead with it are older; FIFO
  keeps arrival order. A frame sits in PostgreSQL's ring at most once (3.5b).
- Scans come from the trace's `scans` list, which the capture works out from
  SQLite's bytecode; the plan text is not read.
"""

from __future__ import annotations

import functools
import gzip
import json
from pathlib import Path

# Milliseconds: hit, read request (R), near request (O), write request (W),
# one page's transfer (T). From the datasheets cited in CLAUDE.md (3.4b).
PROFILES = {
    "ssd": {"hit": 0.0001, "R": 0.044, "O": 0.044, "W": 0.012, "T": 0.0006},
    "hdd": {"hit": 0.0001, "R": 12.7, "O": 0.2, "W": 13.2, "T": 0.02},
}


def load(path: Path | str) -> tuple[dict, list[list[list]], dict[int, str]]:
    """The trace, its queries as [page, dirty, scan_size, fresh] accesses, and
    each page's owning table or index. Kept per file and modification time,
    since a sweep check replays one trace many times; nothing changes them."""
    path = Path(path)
    return _load(path, path.stat().st_mtime_ns)


@functools.lru_cache(maxsize=4)
def _load(path: Path, _mtime: int) -> tuple[dict, list[list[list]], dict[int, str]]:
    raw = gzip.decompress(path.read_bytes()) if path.suffix == ".gz" else path.read_bytes()
    t = json.loads(raw.decode("utf-8"))
    owner = {p["page"]: p["object"] for p in t["pages"] if p["object"] is not None}
    size: dict[str, int] = {}
    for o in owner.values():
        size[o] = size.get(o, 0) + 1
    queries = []
    for q in t["queries"]:
        scanned = q["scans"]
        accesses: list[list] = []
        for op, p in q["io"]:
            earlier = [a for a in accesses if a[0] == p]
            if op == "w" and earlier:
                earlier[-1][1] = True
            else:
                o = owner.get(p)
                accesses.append([p, op == "w", size[o] if o in scanned else None, op == "w"])
        queries.append(accesses)
    return t, queries, owner


# Each policy: load / hit / victim, plus read-ahead hooks (ra, triggers, cap,
# done). Arguments: p page, q query index, pos access position, scan scan size
# or None. victim never returns `pinned`: the page whose read is in progress.


class LRU:
    def __init__(s, n):
        s.order = []

    def ra(s, p, q, pos, scan):
        s.load(p, q, pos, scan)

    def triggers(s, scan, cont):
        return cont

    def cap(s, scan, k):
        return k

    def hit(s, p, q, pos, scan):
        s.order.remove(p)
        s.order.append(p)

    def load(s, p, q, pos, scan):
        s.order.append(p)

    def done(s, p):
        # The page asked for is used once its read finishes; pages read
        # ahead with it have not been used, so they stay older.
        s.order.remove(p)
        s.order.append(p)

    def victim(s, q, pos, scan, pinned=None):
        p = next(x for x in s.order if x != pinned)
        s.order.remove(p)
        return p


class FIFO(LRU):
    def hit(s, p, q, pos, scan):
        pass

    def done(s, p):
        pass


class Clock:
    def __init__(s, n):
        s.n = n
        s.slots = [None] * n
        s.ref = {}
        s.hand = 0

    def ra(s, p, q, pos, scan):
        s.load(p, q, pos, scan)
        s.ref[p] = 0

    def triggers(s, scan, cont):
        return cont

    def cap(s, scan, k):
        return k

    def hit(s, p, q, pos, scan):
        s.ref[p] = 1

    def load(s, p, q, pos, scan):
        s.slots[s._slot] = p
        s.ref[p] = 1

    def place(s, slot):
        s._slot = slot

    def victim(s, q, pos, scan, pinned=None):
        while True:
            p = s.slots[s.hand]
            slot = s.hand
            s.hand = (s.hand + 1) % s.n
            if p == pinned:
                continue
            if s.ref[p]:
                s.ref[p] = 0
            else:
                s._slot = slot
                del s.ref[p]
                return p


class PG:
    """Usage 1 on load, +1 per hit up to 5; clock sweep; a scan bigger than a
    quarter of the pool cycles through a ring of max(1, n // 4) frames."""

    def __init__(s, n):
        s.n = n
        s.slots = [None] * n
        s.u = {}
        s.hand = 0
        s.ringlen = max(1, n // 4)
        s.ring = []
        s.rq = None
        s.rpos = -1

    def ra(s, p, q, pos, scan):
        s.load(p, q, pos, scan)

    def triggers(s, scan, cont):
        return scan is not None

    def cap(s, scan, k):
        return min(k, s.ringlen - 1) if (scan is not None and scan > s.n // 4) else k

    def _ring(s, q, scan):
        if s.rq is not None and s.rq != q:
            s.ring = [None] * s.ringlen
            s.rpos = -1
            s.rq = None
        if not s.ring:
            s.ring = [None] * s.ringlen
        big = scan is not None and scan > s.n // 4
        if big:
            s.rq = q
        return big

    def hit(s, p, q, pos, scan):
        if s._ring(q, scan):
            s.u[p] = max(s.u[p], 1)
        else:
            s.u[p] = min(5, s.u[p] + 1)

    def place(s, slot):
        s._slot = slot

    def load(s, p, q, pos, scan):
        s.slots[s._slot] = p
        s.u[p] = 1
        if s._ring(q, scan):
            s.rpos = (s.rpos + 1) % s.ringlen
            if s.ring[s.rpos] != s._slot:
                # One slot per frame: a frame the sweep found moves here.
                s.ring = [None if x == s._slot else x for x in s.ring]
                s.ring[s.rpos] = s._slot

    def victim(s, q, pos, scan, pinned=None):
        if s._ring(q, scan):
            slot = s.ring[(s.rpos + 1) % s.ringlen]
            if slot is not None and s.slots[slot] != pinned and s.u[s.slots[slot]] <= 1:
                p = s.slots[slot]
                del s.u[p]
                s._slot = slot
                return p
        while True:
            slot = s.hand
            p = s.slots[slot]
            s.hand = (s.hand + 1) % s.n
            if p == pinned:
                continue
            if s.u[p] == 0:
                del s.u[p]
                s._slot = slot
                return p
            s.u[p] -= 1


class InnoDB:
    """One list, head to tail; new pages enter at the head of the old part
    (about 3/8 of the list); a hit in a later query promotes an old page.
    A page read ahead counts as not used until its first hit."""

    def __init__(s, n):
        s.l = []
        s.lq = {}

    def oldlen(s, L):
        return min(L, max(min(L, 1), (L * 3 + 4) // 8))

    def triggers(s, scan, cont):
        return cont

    def cap(s, scan, k):
        return k

    def ra(s, p, q, pos, scan):
        s.load(p, q, pos, scan)
        s.lq[p] = None

    def hit(s, p, q, pos, scan):
        L = len(s.l)
        boundary = L - s.oldlen(L)
        i = s.l.index(p)
        if i >= boundary and s.lq[p] is None:
            s.lq[p] = q
            return
        if i < boundary or q != s.lq[p]:
            s.l.remove(p)
            s.l.insert(0, p)

    def load(s, p, q, pos, scan):
        L = len(s.l) + 1
        s.l.insert(L - s.oldlen(L), p)
        s.lq[p] = q

    def victim(s, q, pos, scan, pinned=None):
        i = max(i for i, x in enumerate(s.l) if x != pinned)
        return s.l.pop(i)


class OPT:
    def __init__(s, n, nxt):
        s.nxt = nxt
        s.f = {}

    def triggers(s, scan, cont):
        return False

    def cap(s, scan, k):
        return 0

    def hit(s, p, q, pos, scan):
        s.f[p] = s.nxt[pos]

    def load(s, p, q, pos, scan):
        s.f[p] = s.nxt[pos]

    def victim(s, q, pos, scan, pinned=None):
        p = max((k for k in s.f if k != pinned), key=lambda k: s.f[k])
        del s.f[p]
        return p


POLICIES = {"lru": LRU, "fifo": FIFO, "clock": Clock, "pg": PG, "innodb": InnoDB}


def read_ahead_cap(setting: str, frames: int) -> int:
    """auto: min(8, frames // 4); off: 0; a number N: min(N, frames // 4)."""
    if setting == "off":
        return 0
    return min(8 if setting == "auto" else int(setting), frames // 4)


def run(
    path: Path | str, policy: str, frames: int, storage: str = "ssd", read_ahead: str = "auto"
) -> dict:
    """Replay a trace; returns counts and time named as in the engine's summary."""
    _, queries, owner = load(path)
    c = PROFILES[storage]
    flat = [(qi, a) for qi, q in enumerate(queries) for a in q]
    nxt = [float("inf")] * len(flat)
    seen: dict[int, int] = {}
    for i in range(len(flat) - 1, -1, -1):
        p = flat[i][1][0]
        nxt[i] = seen.get(p, float("inf"))
        seen[p] = i
    P = POLICIES[policy](frames) if policy in POLICIES else OPT(frames, nxt)
    K = read_ahead_cap(read_ahead, frames)
    res: dict[int, int] = {}
    dirty: dict[int, bool] = {}
    last = None
    streams: dict[str, int] = {}
    waiting: set[int] = set()
    m = dict.fromkeys(
        [
            "hits",
            "next_page_reads",
            "ahead_reads",
            "jump_reads",
            "fresh_pages",
            "dirty_evictions",
            "pages_read_ahead",
            "read_ahead_hits",
            "read_ahead_unused",
            "read_requests",
        ],
        0,
    )
    m["total_ms"] = 0.0
    used = [0]

    def make_room(q, pos, scan, pinned=None):
        if used[0] < frames:
            if hasattr(P, "place"):
                P.place(used[0])
            used[0] += 1
        else:
            v = P.victim(q, pos, scan, pinned)
            del res[v]
            if dirty.pop(v):
                m["dirty_evictions"] += 1
                m["total_ms"] += c["W"] + c["T"]
            if v in waiting:
                waiting.discard(v)
                m["read_ahead_unused"] += 1

    for pos, (qi, (p, w, scan, fresh)) in enumerate(flat):
        o = owner.get(p)
        cont = o is not None and streams.get(o) == p - 1
        if o is not None:
            streams[o] = p
        if p in res:
            m["hits"] += 1
            m["total_ms"] += c["hit"]
            dirty[p] = dirty[p] or w
            if p in waiting:
                waiting.discard(p)
                m["read_ahead_hits"] += 1
            P.hit(p, qi, pos, scan)
            continue
        make_room(qi, pos, scan)
        res[p] = 1
        dirty[p] = w
        P.load(p, qi, pos, scan)
        if fresh:
            m["fresh_pages"] += 1
            continue
        if last is not None and p == last + 1:
            m["next_page_reads"] += 1
            fixed = min(c["O"], c["R"])
        elif last is not None and p > last:
            m["ahead_reads"] += 1
            fixed = min(c["O"] + (p - last - 1) * c["T"], c["R"])
        else:
            m["jump_reads"] += 1
            fixed = c["R"]
        extra = []
        k = P.cap(scan, K)
        if k > 0 and o is not None and P.triggers(scan, cont):
            r = p + 1
            while len(extra) < k and owner.get(r) == o and r not in res:
                make_room(qi, pos, scan, pinned=p)
                res[r] = 1
                dirty[r] = False
                P.ra(r, qi, pos, scan)
                waiting.add(r)
                extra.append(r)
                r += 1
            assert p in res, (
                f"read-ahead evicted the page it read for: {path} {policy} {frames} {pos}"
            )
            if hasattr(P, "done"):
                P.done(p)
        m["pages_read_ahead"] += len(extra)
        m["read_requests"] += 1
        m["total_ms"] += fixed + (1 + len(extra)) * c["T"]
        last = p + len(extra)
    m["dirty_at_end"] = sum(1 for v in dirty.values() if v)
    return m
