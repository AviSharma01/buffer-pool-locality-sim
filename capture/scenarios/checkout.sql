-- A shop's checkout: customers place orders while stock is read and updated.
-- Unlike shop.sql this is mostly writes: pages are changed in memory and have
-- to be written back when a pool evicts them. A restock changes rows on every
-- product page; some rows grow, a full page splits and the file gains a page.
-- Rows are generated with arithmetic so every capture builds the same file.

-- setup
PRAGMA page_size = 4096;

CREATE TABLE products(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    price REAL NOT NULL,
    stock INTEGER NOT NULL
);

CREATE TABLE orders(
    id INTEGER PRIMARY KEY,
    customer TEXT NOT NULL,
    placed_at TEXT NOT NULL,
    status TEXT NOT NULL
);

CREATE TABLE order_items(
    id INTEGER PRIMARY KEY,
    order_id INTEGER NOT NULL,
    product_id INTEGER NOT NULL,
    quantity INTEGER NOT NULL
);
CREATE INDEX order_items_order ON order_items(order_id);

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 900)
INSERT INTO products
SELECT i,
       'Product ' || i || ' ' || substr('abcdefghijklmnopqrstuvwxyz', i % 20 + 1, 6),
       round(3 + (i * 53 % 9000) / 100.0, 2),
       i * 7 % 40
FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 600)
INSERT INTO orders
SELECT i,
       printf('buyer%03d@example.com', i * 31 % 400),
       date('2025-03-01', '+' || (i % 90) || ' days'),
       CASE i % 4 WHEN 0 THEN 'shipped' ELSE 'paid' END
FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1500)
INSERT INTO order_items
SELECT i, (i * 2 + 4) / 5, i * 131 % 900 + 1, i % 3 + 1
FROM n;

-- query: Check product 17
SELECT name, price, stock FROM products WHERE id = 17;

-- query: Place order 601
INSERT INTO orders VALUES(601, 'buyer042@example.com', '2025-06-02', 'paid');

-- query: Add its items
INSERT INTO order_items(order_id, product_id, quantity) VALUES(601, 17, 2), (601, 480, 1);

-- query: Take stock for product 17
UPDATE products SET stock = stock - 2 WHERE id = 17;

-- query: Take stock for product 480
UPDATE products SET stock = stock - 1 WHERE id = 480;

-- query: Restock everything that ran low
UPDATE products SET stock = stock + 40 WHERE stock < 5;

-- query: Check product 17
SELECT name, price, stock FROM products WHERE id = 17;

-- query: Place order 602
INSERT INTO orders VALUES(602, 'buyer007@example.com', '2025-06-02', 'paid');

-- query: Add a large order's items
INSERT INTO order_items(order_id, product_id, quantity)
SELECT 602, i * 37 % 900 + 1, i % 4 + 1 FROM (
    WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 120) SELECT i FROM n
);

-- query: Show order 601
SELECT product_id, quantity FROM order_items WHERE order_id = 601;

-- query: Take stock for product 480
UPDATE products SET stock = stock - 1 WHERE id = 480;
