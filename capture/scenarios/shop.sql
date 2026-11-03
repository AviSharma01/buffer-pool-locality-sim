-- A small shop: everyday lookups, one report that scans every order,
-- then the same lookups again. Rows are generated with arithmetic rather
-- than random() so every capture builds a byte-identical database.

-- setup
PRAGMA page_size = 4096;

CREATE TABLE users(
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL,
    name TEXT NOT NULL,
    city TEXT NOT NULL,
    joined TEXT NOT NULL
);
CREATE INDEX users_email ON users(email);

CREATE TABLE orders(
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL,
    amount REAL NOT NULL,
    placed_at TEXT NOT NULL
);

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3000)
INSERT INTO users
SELECT i,
       printf('user%04d@mail.com', i),
       'User ' || i,
       CASE i % 5 WHEN 0 THEN 'Vancouver' WHEN 1 THEN 'Toronto' WHEN 2 THEN 'Delhi'
                  WHEN 3 THEN 'Mumbai' ELSE 'Calgary' END,
       date('2024-01-01', '+' || (i * 37 % 700) || ' days')
FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
INSERT INTO orders
SELECT i,
       (i * 7919) % 3000 + 1,
       round(5 + (i * 37 % 49500) / 100.0, 2),
       date('2025-01-01', '+' || (i * 13 % 300) || ' days')
FROM n;

-- query: Look up user 42
SELECT * FROM users WHERE id = 42;

-- query: Look up user 2077
SELECT * FROM users WHERE id = 2077;

-- query: Find user by email
SELECT * FROM users WHERE email = 'user1234@mail.com';

-- query: Update user 42
UPDATE users SET city = 'Toronto' WHERE id = 42;

-- query: Report on all orders
SELECT SUM(amount) FROM orders;

-- query: Look up user 42
SELECT * FROM users WHERE id = 42;

-- query: Look up user 2077
SELECT * FROM users WHERE id = 2077;

-- query: Find user by email
SELECT * FROM users WHERE email = 'user1234@mail.com';

-- query: Look up user 42
SELECT * FROM users WHERE id = 42;
