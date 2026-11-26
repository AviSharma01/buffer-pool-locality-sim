# Findings

What changes when the same SQLite workloads run through different buffer-pool
policies. Numbers are pages read from disk, compared with LRU at the same
memory. `python verify/findings.py --check` recomputes every number here and
fails if a claim stops holding.

<!-- findings:summary -->
1. **Full scans are where the policies differ most.** On `reports` with half the database in memory, InnoDB-style and PostgreSQL-style read 16–30% fewer pages than LRU, and LRU needs 1.4–1.6× the memory to match. Without the full scans (`oltp` is the same queries minus the reports, `ranges` has only short ones), PostgreSQL-style is within 3% of LRU.
2. **InnoDB-style helps without full scans too:** 6–9% fewer reads than LRU with a quarter of the database in memory, on `oltp` and `ranges`. Pages used by 10 or more queries account for all of the saving on `oltp` and 75% of it on `ranges`. On `oltp`, InnoDB-style reads pages used fewer times a little more than LRU does. A page enters InnoDB-style's list partway down, at the head of its old sublist, and moves up only when a later query uses it, so pages used a few times don't push out pages used often, as they do under LRU.
3. Optimal reads 33–45% less than LRU with a quarter of the database in memory, so most of the possible saving is still unclaimed.
<!-- /findings:summary -->

## The numbers

Pages read from disk, % fewer than LRU, lowest to highest across the five
variants. "LRU needs" is the memory LRU needs to match the better of
InnoDB-style and PostgreSQL-style.

<!-- findings:results -->
| Trace | Pool | InnoDB-style | PostgreSQL-style | Optimal | LRU needs |
|---|---|---|---|---|---|
| oltp | 1/8 | 7–10% | 1–2% | 39–40% | 1.3× |
| oltp | 1/4 | 6–8% | 1–2% | 35–45% | 1.1–1.2× |
| oltp | 1/2 | 1–4% | 1–2% | 12–46% | 1.0× |
| reports | 1/8 | 6–9% | 5–8% | 24–28% | 1.6–2.7× |
| reports | 1/4 | 9–15% | 9–13% | 33–39% | 1.5–2.7× |
| reports | 1/2 | 16–30% | 16–27% | 53–56% | 1.4–1.6× |
| ranges | 1/8 | 10–11% | 1–3% | 38–41% | 1.3–1.4× |
| ranges | 1/4 | 7–9% | 2–3% | 36–41% | 1.1–1.2× |
| ranges | 1/2 | 1–4% | 1–2% | 6–33% | 1.0× |
<!-- /findings:results -->

## Where InnoDB-style saves

Disk reads with a quarter of the database in memory (seed 7), by how many
queries use each page.

<!-- findings:uses -->
| Trace | Queries using the page | LRU | InnoDB-style | Change |
|---|---|---|---|---|
| oltp | 1 | 1,334 | 1,334 | +0% |
| oltp | 2–9 | 14,778 | 15,012 | +2% |
| oltp | 10–99 | 3,582 | 1,840 | -49% |
| oltp | 100+ | 65 | 66 | +2% |
| ranges | 1 | 3,680 | 3,680 | +0% |
| ranges | 2–9 | 17,319 | 16,782 | -3% |
| ranges | 10–99 | 3,228 | 1,587 | -51% |
| ranges | 100+ | 146 | 146 | +0% |
<!-- /findings:uses -->

## How it was measured

- Traces: `oltp`, `reports` and `ranges` as committed (seed 7), plus local
  variants with seeds 11 and 23 and with the user skew at 0.9 and 1.3
  (`capture.py --seed`, `--param`). `findings.py` captures them.
- Pools of an eighth, a quarter and half of each database, read-ahead off, so
  each number is the policy alone.
- "LRU needs" is exact: the smallest pool in which LRU reads no more, from one
  pass over the trace, checked against the engine at every size.
- The engine's LRU reads within 0.1% of SQLite's own page cache from 256 pages
  up (`capture/tests/test_sqlite_cache.py`).

## Limits

- The workloads were written for this project. The findings hold across their
  seeds and skews, not for databases in general.
- InnoDB-style and PostgreSQL-style are models of those systems' eviction, not
  the systems themselves.
- Disk reads only. The time model is a simple drive model, so no claims rest
  on it.
