-- The online shop of oltp.sql, plus reports: every 3,000 queries a sales
-- report reads every order, and every 7,000 a count of users by city reads
-- every user. Those long scans can push the hot pages out of a pool; this
-- is the workload where that shows. Rows come from arithmetic, so every
-- capture builds a byte-identical database.
--
-- python capture/capture.py capture/scenarios/reports.sql --no-rows \
--   --max-pages 20000 --sqlite-cache 64,128,256,512,1024,2048,4096 \
--   -o traces/reports.trace.json.gz

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

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000)
INSERT INTO users
SELECT i, printf('user%06d@example.com', i), 'city' || (i % 50),
       date('2020-01-01', '+' || (i % 2000) || ' days')
FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 400000)
INSERT INTO orders
SELECT i, (i * 7919) % 100000 + 1, (i % 997) * 1.25,
       date('2025-01-01', '+' || (i * 365 / 400000) || ' days'),
       CASE i % 4 WHEN 0 THEN 'new' WHEN 1 THEN 'paid' WHEN 2 THEN 'shipped' ELSE 'done' END,
       printf('%.20c', 'x')
FROM n;

CREATE INDEX orders_user ON orders(user_id);

-- workload: 30000 queries, seed 7
-- param user = zipf(1.1, 1..100000)
-- param order = zipf(1.0, 1..400000, recent)
-- param new = seq(400001..500000)
-- param amount = uniform(1..500)
-- 50% Look up a user
SELECT id, email, city FROM users WHERE id = {user};
-- 25% A user's latest orders
SELECT id, amount, placed_at FROM orders WHERE user_id = {user} ORDER BY id DESC LIMIT 20;
-- 15% Update a recent order
UPDATE orders SET amount = amount + 1 WHERE id = {order};
-- 10% Place an order
INSERT INTO orders VALUES ({new}, {user}, {amount}, '2026-01-01', 'new', 'xxxxxxxxxxxxxxxxxxxx');
-- every 3000 Sales report
SELECT status, count(*), sum(amount) FROM orders GROUP BY status;
-- every 7000 Users by city
SELECT city, count(*) FROM users GROUP BY city;
