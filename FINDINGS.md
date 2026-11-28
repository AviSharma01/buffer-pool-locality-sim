# Findings

What changes when the same SQLite workloads run through different buffer-pool
policies. Numbers are pages read from disk, compared with LRU at the same
memory. `python verify/findings.py --check` recomputes every number here and
fails if a claim stops holding.

<!-- findings:summary -->
1. **Full scans are where the policies differ most.** On `reports` (an online shop's queries plus periodic full-table reports) and `analytics` (decision-support queries shaped like TPC-H's) with half the database in memory, InnoDB-style and PostgreSQL-style read 16–35% fewer pages than LRU, and LRU needs 1.4–1.6× the memory to match. PostgreSQL-style's edge there is its scan ring (without it, it reads within 4% of LRU), and InnoDB-style's is where new pages enter (finding 2). Without full scans (`oltp` is the shop's queries without the reports, `ranges` has only short ones, `warehouse` is order processing shaped like TPC-C), PostgreSQL-style is within 3% of LRU.
2. **InnoDB-style helps without full scans too:** 3–9% fewer reads than LRU with a quarter of the database in memory, on `oltp`, `ranges` and `warehouse`. The cause is where a new page enters: partway down the list, at the head of its old sublist, so a page that isn't used again leaves from there instead of pushing out pages that are. With new pages entering at the head (old sublist 95%), it saves at most 2% on any trace. On `oltp` and `ranges`, pages used by 10 or more queries account for 46% to all of the saving (the least on `ranges` at skew 1.3). On `warehouse`, whose saving is smaller, 26–44% of it is.
3. Optimal reads 22–45% less than LRU with a quarter of the database in memory, so most of the possible saving is still unclaimed.
<!-- /findings:summary -->

## The numbers

Pages read from disk, % fewer than LRU, lowest to highest across each
workload's variants. "LRU needs" is the memory LRU needs to match the better of
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
| warehouse | 1/8 | 8–9% | 3% | 42–43% | 1.1× |
| warehouse | 1/4 | 3% | 1% | 22–23% | 1.0× |
| warehouse | 1/2 | 0% | 0% | 0% | 0.8× |
| analytics | 1/8 | 1–4% | 5–6% | 17% | 4.5–4.6× |
| analytics | 1/4 | 12–15% | 11–12% | 33–34% | 2.7–2.9× |
| analytics | 1/2 | 27–35% | 27–28% | 63% | 1.5× |
<!-- /findings:results -->

## What the savings come from

Each policy with one rule changed (`--policy innodb:old=20` and so on), % fewer
pages read than LRU, lowest to highest across each workload's variants.

<!-- findings:causes -->
| Policy | oltp (1/4) | ranges (1/4) | warehouse (1/4) | reports (1/2) | analytics (1/2) |
|---|---|---|---|---|---|
| InnoDB-style, old sublist 5% | 2–4% | 3–6% | 0–2% | 28–44% | 36–51% |
| InnoDB-style, old sublist 20% | 6–8% | 8–10% | 3–4% | 26–39% | 35–45% |
| InnoDB-style (old sublist 3/8) | 6–8% | 7–9% | 3% | 16–30% | 27–35% |
| InnoDB-style, old sublist 50% | 5–6% | 4–7% | 2–3% | 11–26% | 22–27% |
| InnoDB-style, old sublist 75% | 2–3% | 2–3% | 0–1% | 3–13% | 10–13% |
| InnoDB-style, old sublist 95% | 0% | 0% | 0% | 0–2% | 1% |
| InnoDB-style, promoted on any re-use | 6–8% | 7–9% | 3% | 16–30% | 30% |
| PostgreSQL-style | 1–2% | 2–3% | 1% | 16–27% | 27–28% |
| PostgreSQL-style without its ring | 1–2% | 2–3% | 1% | 3–4% | 2% |
<!-- /findings:causes -->

## Where InnoDB-style saves

InnoDB-style's disk reads against LRU's with a quarter of the database in
memory, by how many queries use each page, lowest to highest across each
workload's variants.

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
| warehouse | 1 | +0% |
| warehouse | 2–9 | -4% to -3% |
| warehouse | 10–99 | -8% to -5% |
| warehouse | 100+ | -4% to +0% |
<!-- /findings:uses -->

## How it was measured

- Traces: `oltp`, `reports`, `ranges` and `warehouse` as committed (seed 7),
  and `analytics` captured locally (its trace is 3 MB). Variants with seeds 11
  and 23 for all five, and with the user skew at 0.9 and 1.3 for the first
  three (`capture.py --seed`, `--param`). `findings.py` captures them.
- Pools of an eighth, a quarter and half of each database, read-ahead off, so
  each number is the policy alone.
- The changed policies are engine settings, checked against the independent
  model like the policies themselves (`verify/compare.py`, `verify/fuzz.py`).
  `findings.py` also checks that the model reads exactly what the engine does
  on `analytics`, whose long joins the other checks' traces don't have.
- "LRU needs" is exact: the smallest pool in which LRU reads no more, from one
  pass over the trace, checked against the engine at every size.
- The engine's LRU reads within 0.1% of SQLite's own page cache from 256 pages
  up (`capture/tests/test_sqlite_cache.py`).

## Limits

- The workloads were written for this project. `warehouse` and `analytics` are
  shaped like TPC-C and TPC-H but are neither (each scenario's header says how
  they differ). The findings hold across these workloads and their variants,
  not for databases in general.
- InnoDB-style and PostgreSQL-style are models of those systems' eviction, not
  the systems themselves.
<!-- findings:limit -->
- InnoDB-style's rule that only a later query moves an old page up changes no read on `oltp`, `reports` and `ranges` (`innodb:promote=any` reads the same), and under 0.1% of LRU's reads on `warehouse`: within a query, SQLite re-reads at most 45 pages on the first three and 351 on `warehouse`. On `analytics`, whose joins re-read 4,210 pages within a query, the rule helps with a quarter of the database in memory (12–15% fewer reads than LRU, 9–10% without it) but not consistently with half (27–35%, 30% without it). Neither is MySQL's `innodb_old_blocks_time`, which counts a second, not a query.
<!-- /findings:limit -->
- Disk reads only. The time model is a simple drive model, so no claims rest
  on it.
