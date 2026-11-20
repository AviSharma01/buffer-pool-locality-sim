-- Free pages and pages reused without a read: a table loses half its rows
-- (their pages go on the freelist), then an insert reuses that space. Used by
-- compare.py so the model and the engine are also checked on free pages.

-- setup
CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 400)
INSERT INTO t SELECT i, printf('%0200d', i) FROM n;
DELETE FROM t WHERE id > 200;

-- query: Count
SELECT count(*) FROM t;

-- query: Insert into freed space
INSERT INTO t SELECT id + 1000, v FROM t WHERE id <= 50;

-- query: Count again
SELECT count(*) FROM t;
