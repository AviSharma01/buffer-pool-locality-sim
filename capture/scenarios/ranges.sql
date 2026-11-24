-- Reading ranges: a user's order history, a day's sales, and runs of
-- consecutive orders to review. Orders are stored in the order they were
-- placed, so a run of orders (and a day's index entries) sit together on
-- disk, while one user's orders are scattered: this is the workload where
-- layout and read-ahead matter. Recent days and orders are the hot ones. Rows come from arithmetic, so every
-- capture builds a byte-identical database.
--
-- python capture/capture.py capture/scenarios/ranges.sql --no-rows \
--   --max-pages 30000 --sqlite-cache 64,128,256,512,1024,2048,4096 \
--   -o traces/ranges.trace.json.gz

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

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 600000)
INSERT INTO orders
SELECT i, (i * 7919) % 100000 + 1, (i % 997) * 1.25,
       date('2025-01-01', '+' || (i * 365 / 600000) || ' days'),
       CASE i % 4 WHEN 0 THEN 'new' WHEN 1 THEN 'paid' WHEN 2 THEN 'shipped' ELSE 'done' END,
       printf('%.20c', 'x')
FROM n;

CREATE INDEX orders_user ON orders(user_id);
CREATE INDEX orders_placed ON orders(placed_at, amount);

-- workload: 15000 queries, seed 7
-- param user = zipf(1.1, 1..100000)
-- param day = zipf(1.0, 0..364, recent)
-- param first = zipf(1.0, 1..599700, recent)
-- 35% Look up a user
SELECT id, email, city FROM users WHERE id = {user};
-- 25% A user's order history
SELECT id, amount, placed_at FROM orders WHERE user_id = {user} ORDER BY placed_at DESC LIMIT 20;
-- 25% A day's sales
SELECT count(*), sum(amount) FROM orders WHERE placed_at = date('2025-01-01', '+{day} days');
-- 15% Orders to review
SELECT id, status, amount FROM orders WHERE id BETWEEN {first} AND {first} + 300;
