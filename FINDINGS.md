# Findings

What changes when the same SQLite workloads run through different buffer-pool
policies. Numbers are pages read from disk, compared with LRU at the same
memory. `python verify/findings.py --check` recomputes every number here and
fails if a claim stops holding.

<!-- findings:summary -->
1. **Full scans are where the policies differ most.** On `reports` with half the database in memory, InnoDB-style and PostgreSQL-style read 16–30% fewer pages than LRU, and LRU needs 1.4–1.6× the memory to match. PostgreSQL-style's edge there is its scan ring (without it, it reads within 4% of LRU), and InnoDB-style's is where new pages enter (finding 2). Without the full scans (`oltp` is the same queries minus the reports, `ranges` has only short ones), PostgreSQL-style is within 3% of LRU.
2. **InnoDB-style helps without full scans too:** 6–9% fewer reads than LRU with a quarter of the database in memory, on `oltp` and `ranges`. The cause is where a new page enters: partway down the list, at the head of its old sublist, so a page that isn't used again leaves from there instead of pushing out pages that are. With new pages entering at the head (old sublist 95%), it reads within 2% of LRU. Pages used by 10 or more queries account for 46% to all of the saving, the least on `ranges` at skew 1.3.
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

## What the savings come from

Each policy with one rule changed (`--policy innodb:old=20` and so on), % fewer
pages read than LRU, lowest to highest across the five variants.

<!-- findings:causes -->
| Policy | oltp (1/4) | ranges (1/4) | reports (1/2) |
|---|---|---|---|
| InnoDB-style, old sublist 5% | 2–4% | 3–6% | 28–44% |
| InnoDB-style, old sublist 20% | 6–8% | 8–10% | 26–39% |
| InnoDB-style (old sublist 3/8) | 6–8% | 7–9% | 16–30% |
| InnoDB-style, old sublist 50% | 5–6% | 4–7% | 11–26% |
| InnoDB-style, old sublist 75% | 2–3% | 2–3% | 3–13% |
| InnoDB-style, old sublist 95% | 0% | 0% | 0–2% |
| InnoDB-style, promoted on any re-use | 6–8% | 7–9% | 16–30% |
| PostgreSQL-style | 1–2% | 2–3% | 16–27% |
| PostgreSQL-style without its ring | 1–2% | 2–3% | 3–4% |
<!-- /findings:causes -->

## Where InnoDB-style saves

InnoDB-style's disk reads against LRU's with a quarter of the database in
memory, by how many queries use each page, lowest to highest across the five
variants.

<!-- findings:uses -->
| Trace | Queries using the page | InnoDB-style vs LRU |
|---|---|---|
| oltp | 1 | +0% |
| oltp | 2–9 | -5% to +3% |
| oltp | 10–99 | -49% to -30% |
| oltp | 100+ | -9% to +2% |
| ranges | 1 | +0% |
| ranges | 2–9 | -6% to -1% |
| ranges | 10–99 | -52% to -33% |
| ranges | 100+ | +0% |
<!-- /findings:uses -->

## How it was measured

- Traces: `oltp`, `reports` and `ranges` as committed (seed 7), plus local
  variants with seeds 11 and 23 and with the user skew at 0.9 and 1.3
  (`capture.py --seed`, `--param`). `findings.py` captures them.
- Pools of an eighth, a quarter and half of each database, read-ahead off, so
  each number is the policy alone.
- The changed policies are engine settings, checked against the independent
  model like the policies themselves (`verify/compare.py`, `verify/fuzz.py`).
- "LRU needs" is exact: the smallest pool in which LRU reads no more, from one
  pass over the trace, checked against the engine at every size.
- The engine's LRU reads within 0.1% of SQLite's own page cache from 256 pages
  up (`capture/tests/test_sqlite_cache.py`).

## Limits

- The workloads were written for this project. The findings hold across their
  seeds and skews, not for databases in general.
- InnoDB-style and PostgreSQL-style are models of those systems' eviction, not
  the systems themselves.
<!-- findings:limit -->
- InnoDB-style's rule that only a later query moves an old page up changes no read on any trace (`innodb:promote=any` reads the same): within a query, SQLite re-reads at most 45 pages. So these traces can't test what MySQL's `innodb_old_blocks_time` is for, a scan touching each page several times in quick succession.
<!-- /findings:limit -->
- Disk reads only. The time model is a simple drive model, so no claims rest
  on it.
