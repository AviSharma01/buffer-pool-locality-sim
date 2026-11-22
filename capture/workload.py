"""Workloads: many queries made from templates with seeded random parameters.

A scenario's last section may be a workload:

    -- workload: 20000 queries, seed 7
    -- param user = zipf(1.1, 1..100000)
    -- 60% Look up a user
    SELECT * FROM users WHERE id = {user};
    -- 40% A user's orders
    SELECT id, amount FROM orders WHERE user_id = {user};
    -- every 4000 Monthly report
    SELECT sum(amount) FROM orders;

Weighted templates (their percentages add up to 100) are drawn at random.
An `every N` template runs instead at queries N, 2N, 3N, ...; if two fall on
the same query, the one listed first runs. Each run is its own query in the
trace, titled like its template.

Parameters are drawn afresh for every query; one used twice in a statement
gets one value. They go into the SQL only as numbers or quoted strings,
never as raw text. Parameters are declared before the first template, and
every line starting with `--` in this section must be a parameter or a
template heading, so a typo cannot quietly become a comment. The same seed
gives the same queries, byte for byte.

    zipf(s, a..b)          a few hot values and many cold ones: rank r has
                           weight 1/r^s. Ranks are spread over a..b by a
                           fixed shuffle, so hot rows sit anywhere (popular
                           users).
    zipf(s, a..b, recent)  the same, hottest at b and cooling towards a
                           (recent orders).
    uniform(a..b)          every value equally likely.
    choice(x, y, ...)      numbers or 'quoted strings', equally likely.
    seq(a..b)              a, a+1, ...: one per draw (new ids for inserts).
"""

from __future__ import annotations

import bisect
import random
import re
from dataclasses import dataclass, field

MAX_QUERIES = 1_000_000
MAX_VALUES = 5_000_000  # the most values a zipf, uniform or seq range may span

_HEADER = re.compile(r"^-- workload:\s*(\d+)\s+quer(?:y|ies)\s*,\s*seed\s+(\d+)\s*$")
_PARAM = re.compile(r"^-- param\s+([a-z_][a-z0-9_]*)\s*=\s*([a-z]+)\((.*)\)\s*$")
_WEIGHTED = re.compile(r"^--\s+(\d+(?:\.\d+)?)%\s+(\S.*)$")
_EVERY = re.compile(r"^--\s+every\s+(\d+)\s+(\S.*)$")
_RANGE = re.compile(r"^\s*(-?\d+)\s*\.\.\s*(-?\d+)\s*$")
_NUMBER = re.compile(r"^-?\d+(?:\.\d+)?$")
_PLACEHOLDER = re.compile(r"\{([a-z_][a-z0-9_]*)\}")


def is_header(line: str) -> bool:
    return line.strip().startswith("-- workload:")


@dataclass
class Param:
    name: str
    kind: str
    lo: int = 0
    hi: int = 0
    s: float = 0.0
    recent: bool = False
    choices: list[str] = field(default_factory=list)  # SQL literals
    _cdf: list[float] | None = None
    _order: list[int] | None = None
    _next: int = 0

    def draw(self, rng: random.Random, seed: int) -> str:
        """One value, as an SQL literal."""
        if self.kind == "uniform":
            return str(rng.randint(self.lo, self.hi))
        if self.kind == "choice":
            return rng.choice(self.choices)
        if self.kind == "seq":
            value = self.lo + self._next
            if value > self.hi:
                raise ValueError(f"parameter {self.name}: seq({self.lo}..{self.hi}) ran out")
            self._next += 1
            return str(value)
        n = self.hi - self.lo + 1
        if self._cdf is None:
            weights = [1.0 / k**self.s for k in range(1, n + 1)]
            total, acc, cdf = sum(weights), 0.0, []
            for w in weights:
                acc += w / total
                cdf.append(acc)
            self._cdf = cdf
        rank = min(bisect.bisect_left(self._cdf, rng.random()), n - 1)  # 0 = hottest
        if self.recent:
            return str(self.hi - rank)
        if self._order is None:
            self._order = list(range(self.lo, self.hi + 1))
            random.Random(f"{seed}:{self.name}").shuffle(self._order)
        return str(self._order[rank])


@dataclass
class Template:
    title: str
    sql: str
    percent: float | None = None
    every: int | None = None


@dataclass
class Workload:
    count: int
    seed: int
    params: dict[str, Param]
    templates: list[Template]


def _range(text: str, where: str) -> tuple[int, int]:
    m = _RANGE.match(text)
    if not m:
        raise ValueError(f"{where}: expected a range like 1..1000, got {text.strip()!r}")
    lo, hi = int(m[1]), int(m[2])
    if lo > hi:
        raise ValueError(f"{where}: range {lo}..{hi} is empty")
    if hi - lo + 1 > MAX_VALUES:
        raise ValueError(f"{where}: a range may span at most {MAX_VALUES:,} values")
    return lo, hi


def _choices(text: str, where: str) -> list[str]:
    """Numbers or 'quoted strings' ('' inside quotes is a quote), as SQL literals."""
    out, i = [], 0
    while i < len(text):
        if text[i] in " ,":
            i += 1
        elif text[i] == "'":
            j, chars = i + 1, []
            while True:
                if j >= len(text):
                    raise ValueError(f"{where}: unterminated string in choice(...)")
                if text[j] == "'":
                    if text[j + 1 : j + 2] == "'":
                        chars.append("'")
                        j += 2
                        continue
                    break
                chars.append(text[j])
                j += 1
            out.append("'" + "".join(chars).replace("'", "''") + "'")
            i = j + 1
        else:
            j = i
            while j < len(text) and text[j] not in " ,":
                j += 1
            token = text[i:j]
            if not _NUMBER.match(token):
                raise ValueError(
                    f"{where}: choice values are numbers or 'quoted strings', not {token!r}"
                )
            out.append(token)
            i = j
    if not out:
        raise ValueError(f"{where}: choice(...) needs at least one value")
    return out


def _param(name: str, kind: str, args: str, where: str) -> Param:
    if kind == "zipf":
        parts = [p.strip() for p in args.split(",")]
        if len(parts) not in (2, 3) or (len(parts) == 3 and parts[2] != "recent"):
            raise ValueError(f"{where}: zipf takes (s, a..b) or (s, a..b, recent)")
        try:
            s = float(parts[0])
        except ValueError:
            raise ValueError(f"{where}: zipf's skew must be a number, not {parts[0]!r}") from None
        if not 0 < s <= 5:
            raise ValueError(f"{where}: zipf's skew must be between 0 and 5")
        lo, hi = _range(parts[1], where)
        return Param(name, kind, lo, hi, s=s, recent=len(parts) == 3)
    if kind in ("uniform", "seq"):
        lo, hi = _range(args, where)
        return Param(name, kind, lo, hi)
    if kind == "choice":
        return Param(name, kind, choices=_choices(args, where))
    raise ValueError(f"{where}: unknown distribution {kind!r} (zipf, uniform, choice, seq)")


def parse(lines: list[tuple[int, str]], single_statement) -> Workload:
    """`lines` are (line number, text) from the `-- workload:` line to the
    end of the file. `single_statement(title, sql)` checks a template."""
    lineno, header = lines[0]
    m = _HEADER.match(header.strip())
    if not m:
        raise ValueError(f"line {lineno}: expected '-- workload: N queries, seed S'")
    count, seed = int(m[1]), int(m[2])
    if not 1 <= count <= MAX_QUERIES:
        raise ValueError(f"line {lineno}: a workload has 1 to {MAX_QUERIES:,} queries")
    params: dict[str, Param] = {}
    templates: list[Template] = []
    bodies: list[list[str]] = []
    for lineno, line in lines[1:]:
        stripped = line.strip()
        where = f"line {lineno}"
        if stripped.startswith("--"):
            if p := _PARAM.match(stripped):
                if templates:
                    raise ValueError(f"{where}: declare parameters before the first template")
                if p[1] in params:
                    raise ValueError(f"{where}: parameter {p[1]} is declared twice")
                params[p[1]] = _param(p[1], p[2], p[3], where)
            elif w := _WEIGHTED.match(stripped):
                templates.append(Template(w[2].strip(), "", percent=float(w[1])))
                bodies.append([])
            elif e := _EVERY.match(stripped):
                every = int(e[1])
                if not 1 <= every <= count:
                    raise ValueError(f"{where}: 'every {every}' must be between 1 and {count}")
                templates.append(Template(e[2].strip(), "", every=every))
                bodies.append([])
            else:
                raise ValueError(
                    f"{where}: in a workload, '--' lines are '-- param', '-- N% title' "
                    f"or '-- every N title', not {stripped!r}"
                )
            continue
        if not templates:
            if stripped:
                raise ValueError(f"{where}: SQL before the first template")
            continue
        bodies[-1].append(line)
    weighted = [t for t in templates if t.percent is not None]
    if not weighted:
        raise ValueError("a workload needs at least one 'N%' template")
    total = sum(t.percent for t in weighted)  # type: ignore[misc]
    if abs(total - 100) > 1e-9:
        raise ValueError(f"the workload's percentages add up to {total:g}, not 100")
    used: set[str] = set()
    for t, body in zip(templates, bodies, strict=True):
        t.sql = single_statement(t.title, "\n".join(body))
        names = set(_PLACEHOLDER.findall(t.sql))
        for name in names - params.keys():
            raise ValueError(f"template {t.title!r} uses {{{name}}}, which is not declared")
        used |= names
    for name in params.keys() - used:
        raise ValueError(f"parameter {name} is declared but no template uses it")
    return Workload(count, seed, params, templates)


def generate(w: Workload) -> list[tuple[str, str]]:
    """The workload's queries, in order: (title, SQL with values filled in)."""
    rng = random.Random(w.seed)
    weighted = [t for t in w.templates if t.percent is not None]
    cumulative, acc = [], 0.0
    for t in weighted:
        acc += t.percent / 100  # type: ignore[operator]
        cumulative.append(acc)
    everys = [t for t in w.templates if t.every is not None]
    out = []
    for q in range(1, w.count + 1):
        due = next((t for t in everys if q % t.every == 0), None)  # type: ignore[operator]
        t = due or weighted[min(bisect.bisect_left(cumulative, rng.random()), len(weighted) - 1)]
        out.append((t.title, _fill(t.sql, w, rng)))
    return out


def _fill(sql: str, w: Workload, rng: random.Random) -> str:
    """Placeholders replaced by drawn values; one value per name per query."""
    values: dict[str, str] = {}

    def value(m: re.Match[str]) -> str:
        if m[1] not in values:
            values[m[1]] = w.params[m[1]].draw(rng, w.seed)
        return values[m[1]]

    return _PLACEHOLDER.sub(value, sql)
