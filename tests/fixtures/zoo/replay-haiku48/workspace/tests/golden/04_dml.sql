CREATE TABLE d_t (a INTEGER, b INTEGER, s TEXT)
INSERT INTO d_t VALUES (1, 2, 'p'), (3, 4, 'q'), (5, 6, NULL)
UPDATE d_t SET a = b, b = a WHERE a = 1
SELECT a AS a, b AS b FROM d_t ORDER BY a
UPDATE d_t SET a = 0 WHERE a = 1000
UPDATE d_t SET a = a + 10, s = 'z' WHERE s IS NULL
UPDATE d_t SET s = 'same' WHERE a > 0
SELECT a AS a, b AS b, s AS s FROM d_t ORDER BY a
UPDATE d_t SET b = a / 0 WHERE a = 3
SELECT a AS a, b AS b FROM d_t ORDER BY a
UPDATE d_t SET s = NULL
DELETE FROM d_t WHERE a > 100
DELETE FROM d_t WHERE b = 2
SELECT * FROM d_t ORDER BY a
CREATE TABLE d_other (x INTEGER)
DELETE FROM d_t
SELECT * FROM d_t
UPDATE d_t SET zz = 1
UPDATE d_nope SET a = 1
DELETE FROM d_nope
INSERT INTO d_t VALUES (1, 1, 'r'), (2, 2, 'r')
UPDATE d_t SET a = 'x' WHERE a = 1
UPDATE d_t SET a = a + 1, b = 'bad'
SELECT a AS a, b AS b FROM d_t ORDER BY a
UPDATE d_t SET a = 1, b = NULL WHERE a = 2
UPDATE d_t SET s = s || 'x'
SELECT * FROM d_t ORDER BY a
DELETE FROM d_t WHERE s IS NULL
SELECT COUNT(*) AS c FROM d_t
