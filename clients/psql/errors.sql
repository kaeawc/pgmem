-- Error positions: psql prints the statement line an error points into, with a caret.
CREATE TABLE d (
    id nosuchtype
);
CREATE TABLE t (a int, b text);
SELECT nope FROM t;
SELECT a,
       b
FROM t
WHERE a + 'x'::text > 1;
SELECT a, count(*) FROM t;
SELECT * FROM t WHERE count(*) > 1;
SELEC 1;
SELECT 1 FROM;
INSERT INTO t (a, a) VALUES (1, 2);
SELECT -b FROM t;
ALTER TABLE t ALTER COLUMN a SET DEFAULT 'x';
