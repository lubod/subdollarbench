CREATE TABLE j_a (id INTEGER, name TEXT)
CREATE TABLE j_b (id INTEGER, a_id INTEGER, amt INTEGER)
CREATE TABLE j_c (code TEXT, bid INTEGER)
INSERT INTO j_a VALUES (1, 'ann'), (2, 'bob'), (3, 'cy')
INSERT INTO j_b VALUES (10, 1, 100), (11, 1, 50), (12, 2, 70), (13, NULL, 5), (14, 9, 1)
INSERT INTO j_c VALUES ('x', 10), ('y', 12), ('z', 99)
SELECT j_a.name AS n, j_b.amt AS amt FROM j_a INNER JOIN j_b ON j_a.id = j_b.a_id ORDER BY n, amt
SELECT j_a.name AS n, j_b.amt AS amt FROM j_a, j_b WHERE j_a.id = j_b.a_id ORDER BY n, amt
SELECT j_a.name AS n, j_b.id AS bid FROM j_a LEFT JOIN j_b ON j_a.id = j_b.a_id ORDER BY n, bid
SELECT j_a.name AS n, j_b.id AS bid FROM j_a LEFT OUTER JOIN j_b ON j_a.id = j_b.a_id AND j_b.amt > 60 ORDER BY n, bid
SELECT id AS i FROM j_a INNER JOIN j_b ON j_a.id = j_b.a_id
SELECT j_a.id AS ai, j_b.id AS bi, code AS code FROM j_a INNER JOIN j_b ON j_a.id = j_b.a_id INNER JOIN j_c ON j_c.bid = j_b.id ORDER BY ai, bi
SELECT j_a.name AS n, j_c.code AS code FROM j_a LEFT JOIN j_b ON j_a.id = j_b.a_id LEFT JOIN j_c ON j_c.bid = j_b.id ORDER BY n, code
SELECT x.name AS n, y.name AS m FROM j_a x, j_a y WHERE x.id < y.id ORDER BY n, m
SELECT p.id AS pid, q.id AS qid FROM j_a p INNER JOIN j_a q ON p.id = q.id ORDER BY pid
SELECT id AS i FROM j_a x, j_a y
SELECT zz.id AS i FROM j_a INNER JOIN j_b ON j_a.id = j_b.a_id
SELECT j_a.zz AS i FROM j_a INNER JOIN j_b ON j_a.id = j_b.a_id
SELECT j_a.id AS i FROM j_a INNER JOIN j_b ON j_a.zz = j_b.a_id
SELECT j_a.id AS i FROM j_a INNER JOIN j_nope ON j_a.id = j_nope.a_id
SELECT * FROM j_a, j_c ORDER BY name, code
SELECT * FROM j_a INNER JOIN j_b ON j_a.id = j_b.a_id ORDER BY j_b.id
SELECT j_b.amt AS amt, COUNT(*) AS c FROM j_a INNER JOIN j_b ON j_a.id = j_b.a_id GROUP BY j_b.amt ORDER BY amt
SELECT DISTINCT j_a.name AS n FROM j_a INNER JOIN j_b ON j_a.id = j_b.a_id ORDER BY n LIMIT 1
SELECT j_a.name AS n FROM j_a, j_b WHERE j_a.id = j_b.a_id AND j_b.amt >= 70 ORDER BY n
SELECT j_a.name AS n, j_b.amt AS amt FROM j_a JOIN j_b ON j_a.id = j_b.a_id ORDER BY amt DESC LIMIT 2
SELECT a.name AS n, b.amt AS amt FROM j_a AS a INNER JOIN j_b AS b ON a.id = b.a_id WHERE b.amt < 60 ORDER BY n
