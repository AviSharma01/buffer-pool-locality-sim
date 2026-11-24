-- reports.sql at twelve times the size: 1.2 million users, 4.8 million
-- orders, about 100,000 pages. Queries scale with the data, and the reports
-- run twelve times less often, so scans take about the same share of the
-- page requests as in reports.sql. A scale check for findings made on the
-- smaller workloads; its trace stays local (traces/local/ is not committed).
-- The capture takes a few minutes.
--
-- python capture/capture.py capture/scenarios/reports-big.sql --no-rows \
--   --max-pages 150000 --max-io 200000 --sqlite-cache 1024,4096,16384 \
--   -o traces/local/reports-big.trace.json.gz

-- setup
PRAGMA page_size = 4096;

CREATE TABLE users(
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL,
    city TEXT NOT NULL,
    joined TEXT NOT NULL
);
CREATE TABLE orders(
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL,
    amount REAL NOT NULL,
    placed_at TEXT NOT NULL,
    status TEXT NOT NULL,
    note TEXT NOT NULL
);

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1200000)
INSERT INTO users
SELECT i, printf('user%06d@example.com', i), 'city' || (i % 50),
       date('2020-01-01', '+' || (i % 2000) || ' days')
FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 4800000)
INSERT INTO orders
SELECT i, (i * 7919) % 1200000 + 1, (i % 997) * 1.25,
       date('2025-01-01', '+' || (i * 365 / 4800000) || ' days'),
       CASE i % 4 WHEN 0 THEN 'new' WHEN 1 THEN 'paid' WHEN 2 THEN 'shipped' ELSE 'done' END,
       printf('%.20c', 'x')
FROM n;

CREATE INDEX orders_user ON orders(user_id);

-- workload: 150000 queries, seed 7
-- param user = zipf(1.1, 1..1200000)
-- param order = zipf(1.0, 1..4800000, recent)
-- param new = seq(4800001..4900000)
-- param amount = uniform(1..500)
-- 50% Look up a user
SELECT id, email, city FROM users WHERE id = {user};
-- 25% A user's latest orders
SELECT id, amount, placed_at FROM orders WHERE user_id = {user} ORDER BY id DESC LIMIT 20;
-- 15% Update a recent order
UPDATE orders SET amount = amount + 1 WHERE id = {order};
-- 10% Place an order
INSERT INTO orders VALUES ({new}, {user}, {amount}, '2026-01-01', 'new', 'xxxxxxxxxxxxxxxxxxxx');
-- every 36000 Sales report
SELECT status, count(*), sum(amount) FROM orders GROUP BY status;
-- every 84000 Users by city
SELECT city, count(*) FROM users GROUP BY city;
