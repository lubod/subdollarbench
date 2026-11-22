CREATE TABLE b_t (id INTEGER, s TEXT, n INTEGER)
CREATE TABLE b_t (x INTEGER)
CREATE TABLE b_u (a INTEGER, b TEXT, c INTEGER, d TEXT, e INTEGER, f INTEGER, g INTEGER)
CREATE TABLE b_v (a VARCHAR)
INSERT INTO b_t VALUES (1, 'alpha', 10), (2, 'be''ta', NULL), (-3, '', 0)
INSERT INTO b_t VALUES (4, 'x\y', 5)
INSERT INTO b_t VALUES (5, 'tab	here', 6)
SELECT * FROM b_t ORDER BY id
SELECT s, id FROM b_t ORDER BY id
SELECT id, n FROM b_t WHERE id >= 4 ORDER BY id
INSERT INTO b_t (s, id) VALUES ('col', 9)
INSERT INTO b_t (n) VALUES (77)
SELECT * FROM b_t WHERE id IS NULL
SELECT id, s FROM b_t WHERE id = 9
select  id from b_t where id = 1 ;
SeLeCt	id	FROM	b_t	WHERE	s	=	'alpha'
SELECT * FROM b_nope
SELECT b_zz FROM b_t
SELECT id FROM b_t WHERE zz = 1
INSERT INTO b_t VALUES (1, 'a')
INSERT INTO b_t VALUES (1, 'a', 2, 3)
INSERT INTO b_t VALUES (1, 'a', 'zz')
INSERT INTO b_t (id, zz) VALUES (1, 2)
INSERT INTO b_nope VALUES (1)
INSERT INTO b_t VALUES (100, 'ok', 1), (101, 'bad', 'zz')
SELECT id FROM b_t WHERE id >= 100 ORDER BY id
SELECT * FROM
SELECT * FROM b_t WHERE
SELEC * FROM b_t
INSERT INTO b_t VALUES (
CREATE TABLE b_bad (a INTEGER
CREATE TABLE 1bad (a INTEGER)
INSERT INTO b_t VALUES (7, 'unterminated)
SELECT 1 2
SELECT (1
SELECT 1 +
SELECT -(3) AS neg, 2 - -2 AS two
SELECT 'it''s' AS q, 'back\slash' AS b
INSERT INTO b_u (a, b) VALUES (1, 'tab\there'), (2, '')
SELECT a AS a, b AS b FROM b_u WHERE b = '' ORDER BY a
SELECT b AS b FROM b_u ORDER BY a
INSERT INTO b_u VALUES (9223372036854775807, 'max', -9223372036854775808, NULL, 0, 0, 0)
SELECT a AS a, c AS c FROM b_u WHERE a > 1 ORDER BY a
SELECT b AS b FROM b_u WHERE a IS NOT NULL AND c IS NULL ORDER BY b
