CREATE TABLE s_a (id INTEGER, v INTEGER)
CREATE TABLE s_b (aid INTEGER, w TEXT)
INSERT INTO s_a VALUES (1, 10), (2, 20), (3, NULL), (4, 40)
INSERT INTO s_b VALUES (1, 'x'), (1, 'y'), (3, NULL), (9, 'z')
SELECT id AS id, (SELECT COUNT(*) AS c FROM s_b) AS total FROM s_a ORDER BY id
SELECT id AS id, (SELECT MAX(x.v) AS m FROM s_a x WHERE x.id < s_a.id) AS prevmax FROM s_a ORDER BY id
SELECT id AS id FROM s_a WHERE v = (SELECT MAX(v) AS m FROM s_a) ORDER BY id
SELECT id AS id FROM s_a WHERE v > (SELECT MIN(aid) AS m FROM s_b) ORDER BY id
SELECT (SELECT SUM(id) AS s FROM s_a WHERE id > 100) AS none
SELECT (SELECT id AS id FROM s_a WHERE id > 100) AS none2
SELECT (SELECT id AS id, v AS v FROM s_a WHERE id = 1) AS bad
SELECT id AS id FROM s_a WHERE id IN (SELECT aid AS aid FROM s_b) ORDER BY id
SELECT id AS id FROM s_a WHERE id NOT IN (SELECT aid AS aid FROM s_b) ORDER BY id
INSERT INTO s_b VALUES (NULL, 'n')
SELECT id AS id FROM s_a WHERE id NOT IN (SELECT aid AS aid FROM s_b) ORDER BY id
SELECT id AS id FROM s_a WHERE id IN (SELECT aid AS aid FROM s_b) ORDER BY id
SELECT id AS id FROM s_a WHERE id IN (1, 2, NULL) ORDER BY id
SELECT id AS id FROM s_a WHERE id NOT IN (1, 2) ORDER BY id
SELECT NULL IN (1, 2) AS a, NULL IN (SELECT aid AS aid FROM s_b WHERE aid > 100) AS b, 5 IN (SELECT aid AS aid FROM s_b WHERE aid > 100) AS c, NULL IN (SELECT aid AS aid FROM s_b) AS d
SELECT id AS id FROM s_a a WHERE EXISTS (SELECT 1 AS one FROM s_b b WHERE b.aid = a.id) ORDER BY id
SELECT id AS id FROM s_a a WHERE NOT EXISTS (SELECT w AS w FROM s_b b WHERE b.aid = a.id) ORDER BY id
SELECT id AS id FROM s_a WHERE EXISTS (SELECT 1 AS one FROM s_b WHERE aid = id) ORDER BY id
CREATE TABLE s_copy (id INTEGER, v INTEGER)
INSERT INTO s_copy SELECT id AS id, v AS v FROM s_a WHERE id < 3
INSERT INTO s_copy SELECT aid AS id, aid AS v FROM s_b WHERE w IS NULL
INSERT INTO s_copy SELECT aid AS id, w AS v FROM s_b
SELECT * FROM s_copy ORDER BY id
SELECT (SELECT COUNT(*) AS c FROM s_copy) AS c
SELECT zz AS x FROM s_a WHERE id IN (SELECT zz AS zz FROM s_b)
SELECT id AS id FROM s_a WHERE id IN (SELECT aid AS aid FROM nope)
SELECT id AS id FROM s_a WHERE EXISTS (SELECT 1 AS one FROM s_b WHERE zz = 1)
SELECT id AS id, (SELECT MAX(w) AS m FROM s_b WHERE aid = s_a.id) AS mw FROM s_a ORDER BY id
