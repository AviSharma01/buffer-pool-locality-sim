-- Two tables that grew together: every order also writes two rows to an event
-- log, the way an app records what happened to each order. SQLite gives each
-- table a new page at the end of the file when it fills one, so orders,
-- events and the events index end up interleaved. Capture it twice to compare
-- layouts, as built and with --vacuum, which rebuilds each one in a run of
-- pages. Rows are generated with arithmetic so every capture builds the same file.

-- setup
PRAGMA page_size = 4096;

CREATE TABLE orders(
    id INTEGER PRIMARY KEY,
    customer TEXT NOT NULL,
    amount REAL NOT NULL,
    placed_at TEXT NOT NULL
);

CREATE TABLE events(
    id INTEGER PRIMARY KEY,
    order_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    at TEXT NOT NULL
);
CREATE INDEX events_order ON events(order_id);

CREATE TRIGGER log_order AFTER INSERT ON orders BEGIN
    INSERT INTO events(order_id, kind, at) VALUES (new.id, 'placed', new.placed_at);
    INSERT INTO events(order_id, kind, at) VALUES (new.id, 'paid', new.placed_at);
END;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2500)
INSERT INTO orders
SELECT i,
       printf('customer%04d', (i * 7919) % 1500 + 1),
       round(5 + (i * 37 % 49500) / 100.0, 2),
       date('2025-01-01', '+' || (i / 10) || ' days')
FROM n;

DROP TRIGGER log_order;

-- query: Total of all orders
SELECT SUM(amount) FROM orders;

-- query: Look up order 1200
SELECT * FROM orders WHERE id = 1200;

-- query: Events for order 1200
SELECT * FROM events WHERE order_id = 1200;

-- query: Count paid events
SELECT COUNT(*) FROM events WHERE kind = 'paid';

-- query: Total of all orders again
SELECT SUM(amount) FROM orders;
