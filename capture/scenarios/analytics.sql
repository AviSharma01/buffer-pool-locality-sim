-- Decision-support queries shaped like TPC-H: pricing summary (Q1), shipping
-- priority (Q3), order priority checking (Q4), forecasting revenue change (Q6)
-- and promotion effect (Q14), over customers, parts, orders and their line
-- items, about TPC-H's scale factor 0.04. TPC-H's date ranges; rows come from
-- arithmetic, so every capture builds a byte-identical database.
--
-- Not TPC-H: the queries are simplified to the tables they read (no nation,
-- region, supplier or partsupp), the five query types are drawn at random
-- with random parameters, and the columns hold made-up values.
--
-- The trace is about 2.5 MB, so it isn't committed; verify/findings.py
-- captures it into traces/local/:
--
-- python capture/capture.py capture/scenarios/analytics.sql --no-rows \
--   --max-pages 20000 --max-io 200000 -o traces/local/analytics.trace.json.gz

-- setup
PRAGMA page_size = 4096;

CREATE TABLE customer(
    c_custkey INTEGER PRIMARY KEY,
    c_name TEXT NOT NULL,
    c_mktsegment TEXT NOT NULL,
    c_acctbal REAL NOT NULL,
    c_comment TEXT NOT NULL
);
CREATE TABLE part(
    p_partkey INTEGER PRIMARY KEY,
    p_name TEXT NOT NULL,
    p_type TEXT NOT NULL,
    p_retailprice REAL NOT NULL,
    p_comment TEXT NOT NULL
);
CREATE TABLE orders(
    o_orderkey INTEGER PRIMARY KEY,
    o_custkey INTEGER NOT NULL,
    o_orderdate TEXT NOT NULL,
    o_orderpriority TEXT NOT NULL,
    o_totalprice REAL NOT NULL,
    o_comment TEXT NOT NULL
);
CREATE TABLE lineitem(
    l_id INTEGER PRIMARY KEY,
    l_orderkey INTEGER NOT NULL,
    l_partkey INTEGER NOT NULL,
    l_quantity INTEGER NOT NULL,
    l_extendedprice REAL NOT NULL,
    l_discount REAL NOT NULL,
    l_returnflag TEXT NOT NULL,
    l_shipdate TEXT NOT NULL,
    l_commitdate TEXT NOT NULL,
    l_receiptdate TEXT NOT NULL,
    l_comment TEXT NOT NULL
);

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 6000)
INSERT INTO customer
SELECT i, printf('Customer#%09d', i),
       CASE i % 5 WHEN 0 THEN 'AUTOMOBILE' WHEN 1 THEN 'BUILDING' WHEN 2 THEN 'FURNITURE'
                  WHEN 3 THEN 'HOUSEHOLD' ELSE 'MACHINERY' END,
       (i % 10000) - 999.99, printf('%.60c', 'c')
FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 8000)
INSERT INTO part
SELECT i, printf('part %06d', i),
       CASE i % 3 WHEN 0 THEN 'PROMO BRUSHED' WHEN 1 THEN 'STANDARD POLISHED'
                  ELSE 'ECONOMY PLATED' END,
       900 + (i % 200), printf('%.20c', 'p')
FROM n;

-- order dates spread over 1992-01-01 to 1998-08-02, not in key order
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 60000)
INSERT INTO orders
SELECT i, (i * 7919) % 6000 + 1, date('1992-01-01', '+' || ((i * 7919) % 2405) || ' days'),
       CASE i % 5 WHEN 0 THEN '1-URGENT' WHEN 1 THEN '2-HIGH' WHEN 2 THEN '3-MEDIUM'
                  WHEN 3 THEN '4-NOT SPECIFIED' ELSE '5-LOW' END,
       (i % 50000) * 3.0, printf('%.40c', 'o')
FROM n;

-- four lines per order: shipped 1-121 days after the order, committed for
-- 30-90 days after it, received 1-30 days after shipping
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 240000)
INSERT INTO lineitem
SELECT i, k, (i * 104729) % 8000 + 1, 1 + i % 50, (i % 9000) * 1.1, (i % 11) / 100.0,
       CASE i % 3 WHEN 0 THEN 'R' WHEN 1 THEN 'A' ELSE 'N' END,
       date(d, '+' || (1 + (i * 31) % 121) || ' days'),
       date(d, '+' || (30 + (i * 17) % 61) || ' days'),
       date(d, '+' || (1 + (i * 31) % 121 + 1 + (i * 13) % 30) || ' days'),
       printf('%.30c', 'l')
FROM (SELECT i, (i - 1) / 4 + 1 AS k,
             date('1992-01-01', '+' || (((i - 1) / 4 + 1) * 7919 % 2405) || ' days') AS d
      FROM n);

CREATE INDEX orders_customer ON orders(o_custkey);
CREATE INDEX lineitem_order ON lineitem(l_orderkey);

-- workload: 150 queries, seed 7
-- param delta = uniform(60..120)
-- param day = uniform(0..30)
-- param month = uniform(0..57)
-- param year = uniform(1993..1997)
-- param discount = choice(0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08, 0.09)
-- param quantity = uniform(24..25)
-- param segment = choice('AUTOMOBILE', 'BUILDING', 'FURNITURE', 'HOUSEHOLD', 'MACHINERY')
-- 20% Pricing summary report
SELECT l_returnflag, sum(l_quantity), sum(l_extendedprice),
       sum(l_extendedprice * (1 - l_discount)), avg(l_discount), count(*)
FROM lineitem WHERE l_shipdate <= date('1998-12-01', '-' || {delta} || ' days')
GROUP BY l_returnflag ORDER BY l_returnflag;
-- 20% Shipping priority
SELECT o_orderkey, sum(l_extendedprice * (1 - l_discount)) AS revenue, o_orderdate
FROM customer JOIN orders ON o_custkey = c_custkey JOIN lineitem ON l_orderkey = o_orderkey
WHERE c_mktsegment = {segment}
  AND o_orderdate < date('1995-03-01', '+' || {day} || ' days')
  AND l_shipdate > date('1995-03-01', '+' || {day} || ' days')
GROUP BY o_orderkey, o_orderdate ORDER BY revenue DESC, o_orderdate LIMIT 10;
-- 20% Order priority checking
SELECT o_orderpriority, count(*) FROM orders
WHERE o_orderdate >= date('1993-01-01', '+' || {month} || ' months')
  AND o_orderdate < date('1993-01-01', '+' || ({month} + 3) || ' months')
  AND EXISTS (SELECT 1 FROM lineitem
              WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate)
GROUP BY o_orderpriority ORDER BY o_orderpriority;
-- 20% Forecasting revenue change
SELECT sum(l_extendedprice * l_discount) FROM lineitem
WHERE l_shipdate >= {year} || '-01-01' AND l_shipdate < ({year} + 1) || '-01-01'
  AND l_discount BETWEEN {discount} - 0.01 AND {discount} + 0.01 AND l_quantity < {quantity};
-- 20% Promotion effect
SELECT 100.0 * sum(CASE WHEN p_type LIKE 'PROMO%' THEN l_extendedprice * (1 - l_discount)
                        ELSE 0 END) / sum(l_extendedprice * (1 - l_discount))
FROM lineitem JOIN part ON p_partkey = l_partkey
WHERE l_shipdate >= date('1993-01-01', '+' || {month} || ' months')
  AND l_shipdate < date('1993-01-01', '+' || ({month} + 1) || ' months');
