-- Order processing shaped like TPC-C, for one warehouse: New-Order, Payment,
-- Order-Status, Delivery and Stock-Level. TPC-C's tables, its starting row
-- counts for one warehouse, and its non-uniform random ids (NURand). Rows come
-- from arithmetic, so every capture builds a byte-identical database.
--
-- Not TPC-C: each statement is drawn on its own, weighted by its share of the
-- statements TPC-C's mix of transactions runs (a New-Order runs about 30), so
-- the statements of one transaction don't share their rows. No think time,
-- no keying time, one connection.
--
-- python capture/capture.py capture/scenarios/warehouse.sql --no-rows \
--   --max-pages 30000 -o traces/warehouse.trace.json.gz

-- setup
PRAGMA page_size = 4096;

CREATE TABLE warehouse(w_id INTEGER PRIMARY KEY, w_ytd REAL NOT NULL, w_info TEXT NOT NULL);
CREATE TABLE district(
    d_id INTEGER PRIMARY KEY,
    d_next_o_id INTEGER NOT NULL,
    d_ytd REAL NOT NULL,
    d_info TEXT NOT NULL
);
CREATE TABLE customer(
    c_id INTEGER PRIMARY KEY,
    c_d_id INTEGER NOT NULL,
    c_last TEXT NOT NULL,
    c_first TEXT NOT NULL,
    c_balance REAL NOT NULL,
    c_payment_cnt INTEGER NOT NULL,
    c_delivery_cnt INTEGER NOT NULL,
    c_data TEXT NOT NULL
);
CREATE TABLE history(
    h_id INTEGER PRIMARY KEY,
    h_c_id INTEGER NOT NULL,
    h_amount REAL NOT NULL,
    h_data TEXT NOT NULL
);
CREATE TABLE item(i_id INTEGER PRIMARY KEY, i_name TEXT NOT NULL, i_price REAL NOT NULL,
    i_data TEXT NOT NULL);
CREATE TABLE stock(
    s_i_id INTEGER PRIMARY KEY,
    s_quantity INTEGER NOT NULL,
    s_ytd INTEGER NOT NULL,
    s_order_cnt INTEGER NOT NULL,
    s_data TEXT NOT NULL
);
CREATE TABLE orders(
    o_id INTEGER PRIMARY KEY,
    o_c_id INTEGER NOT NULL,
    o_d_id INTEGER NOT NULL,
    o_entry_d TEXT NOT NULL,
    o_carrier_id INTEGER,
    o_ol_cnt INTEGER NOT NULL
);
CREATE TABLE new_order(no_o_id INTEGER PRIMARY KEY, no_d_id INTEGER NOT NULL);
CREATE TABLE order_line(
    ol_id INTEGER PRIMARY KEY,
    ol_o_id INTEGER NOT NULL,
    ol_i_id INTEGER NOT NULL,
    ol_quantity INTEGER NOT NULL,
    ol_amount REAL NOT NULL,
    ol_delivery_d TEXT,
    ol_dist_info TEXT NOT NULL
);

INSERT INTO warehouse VALUES (1, 300000, printf('%.80c', 'w'));

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10)
INSERT INTO district SELECT i, 3001, 30000, printf('%.80c', 'd') FROM n;

-- 3,000 customers per district; last names from 1,000 syllable combinations
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 30000)
INSERT INTO customer
SELECT i, (i - 1) / 3000 + 1, 'NAME' || ((i * 7919) % 1000), printf('first%05d', i),
       -10, 1, 0, printf('%.300c', 'c')
FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 30000)
INSERT INTO history SELECT i, i, 10, printf('%.24c', 'h') FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000)
INSERT INTO item SELECT i, printf('item%06d', i), 1 + (i % 9999) / 100.0, printf('%.40c', 'i')
FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000)
INSERT INTO stock SELECT i, 10 + (i % 91), 0, 0, printf('%.60c', 's') FROM n;

-- 3,000 orders per district, the last 900 not yet delivered; 10 lines each
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 30000)
INSERT INTO orders
SELECT i, (i * 7919) % 30000 + 1, (i - 1) % 10 + 1, '2025-01-01',
       CASE WHEN i <= 21000 THEN 1 + i % 10 END, 10
FROM n;

INSERT INTO new_order SELECT o_id, o_d_id FROM orders WHERE o_id > 21000;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300000)
INSERT INTO order_line
SELECT i, (i - 1) / 10 + 1, (i * 104729) % 100000 + 1, 5, (i % 9999) / 100.0,
       CASE WHEN i <= 210000 THEN '2025-01-02' END, printf('%.24c', 'o')
FROM n;

CREATE INDEX customer_last ON customer(c_d_id, c_last, c_first);
CREATE INDEX orders_customer ON orders(o_c_id, o_id);
CREATE INDEX orders_undelivered ON orders(o_d_id, o_carrier_id, o_id);
CREATE INDEX new_order_district ON new_order(no_d_id, no_o_id);
CREATE INDEX order_line_order ON order_line(ol_o_id);

-- workload: 30000 queries, seed 7
-- param item = nurand(8191, 1..100000)
-- param customer = nurand(1023, 1..3000)
-- param last = nurand(255, 0..999)
-- param district = uniform(1..10)
-- param quantity = uniform(1..10)
-- param amount = uniform(1..5000)
-- param carrier = uniform(1..10)
-- param threshold = uniform(10..20)
-- param line = seq(300001..1300000)
-- param order = seq(30001..130000)
-- param payment = seq(30001..130000)
-- 24.9% New-Order: read an item
SELECT i_price, i_name, i_data FROM item WHERE i_id = {item};
-- 24.9% New-Order: take it from stock
UPDATE stock SET s_quantity = s_quantity - {quantity}, s_ytd = s_ytd + {quantity},
    s_order_cnt = s_order_cnt + 1 WHERE s_i_id = {item};
-- 24.9% New-Order: add an order line
INSERT INTO order_line
SELECT {line}, max(o_id), {item}, {quantity}, {quantity} * 12.5, NULL,
       'oooooooooooooooooooooooo'
FROM orders;
-- 2.5% New-Order: next order id
UPDATE district SET d_next_o_id = d_next_o_id + 1 WHERE d_id = {district};
-- 2.5% New-Order: the order
INSERT INTO orders VALUES ({order}, ({district} - 1) * 3000 + {customer}, {district},
    '2026-01-01', NULL, 10);
-- 2.4% Payment: warehouse
UPDATE warehouse SET w_ytd = w_ytd + {amount} WHERE w_id = 1;
-- 2.4% Payment: district
UPDATE district SET d_ytd = d_ytd + {amount} WHERE d_id = {district};
-- 1.4% Payment: find the customer by last name
SELECT c_id, c_first, c_balance FROM customer
WHERE c_d_id = {district} AND c_last = 'NAME' || {last} ORDER BY c_first;
-- 2.4% Payment: the customer
UPDATE customer SET c_balance = c_balance - {amount}, c_payment_cnt = c_payment_cnt + 1
WHERE c_id = ({district} - 1) * 3000 + {customer};
-- 2.4% Payment: history
INSERT INTO history VALUES ({payment}, ({district} - 1) * 3000 + {customer}, {amount},
    'hhhhhhhhhhhhhhhhhhhhhhhh');
-- 0.4% Order-Status: the customer's last order
SELECT ol_i_id, ol_quantity, ol_amount, ol_delivery_d FROM order_line
WHERE ol_o_id = (SELECT max(o_id) FROM orders WHERE o_c_id = ({district} - 1) * 3000 + {customer});
-- 2.2% Delivery: the district's oldest new order
DELETE FROM new_order
WHERE no_o_id = (SELECT min(no_o_id) FROM new_order WHERE no_d_id = {district});
-- 2.2% Delivery: mark the order
UPDATE orders SET o_carrier_id = {carrier}
WHERE o_id = (SELECT min(o_id) FROM orders WHERE o_d_id = {district} AND o_carrier_id IS NULL);
-- 2.2% Delivery: date its lines
UPDATE order_line SET ol_delivery_d = '2026-01-02'
WHERE ol_o_id = (SELECT min(o_id) FROM orders WHERE o_d_id = {district} AND o_carrier_id IS NULL);
-- 2.1% Delivery: the customer
UPDATE customer SET c_balance = c_balance + {amount}, c_delivery_cnt = c_delivery_cnt + 1
WHERE c_id = ({district} - 1) * 3000 + {customer};
-- 0.2% Stock-Level: items of the last 20 orders running low
SELECT count(DISTINCT s_i_id) FROM order_line JOIN stock ON s_i_id = ol_i_id
WHERE ol_o_id > (SELECT max(o_id) FROM orders) - 20 AND s_quantity < {threshold};
