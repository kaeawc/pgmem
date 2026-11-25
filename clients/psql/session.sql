-- psql variables, output formats, \copy, transactions and error handling.
CREATE TABLE kv (k text PRIMARY KEY, v int);
INSERT INTO kv VALUES ('a', 1), ('b', 2), ('c', NULL);
SELECT count(*) AS n FROM kv \gset
\echo rows: :n
SELECT * FROM kv ORDER BY k;
\x on
SELECT * FROM kv WHERE k = 'a';
\x off
\pset format csv
SELECT * FROM kv ORDER BY k;
\pset format aligned
\pset null '(null)'
SELECT * FROM kv ORDER BY k;
\copy kv TO STDOUT WITH (FORMAT csv, HEADER)
\copy kv FROM STDIN
d	4
e	5
\.
SELECT k, v FROM kv WHERE v > 3 ORDER BY k;
BEGIN;
INSERT INTO kv VALUES ('f', 6);
SAVEPOINT s;
INSERT INTO kv VALUES ('a', 9);
ROLLBACK TO s;
COMMIT;
SELECT count(*) FROM kv;
SELECT 1/0;
\set ON_ERROR_STOP 0
SELECT 'still running' AS status;
