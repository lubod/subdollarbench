CREATE TABLE i_t (id INTEGER, email TEXT, n INTEGER)
INSERT INTO i_t VALUES (1, 'a@x', 1), (2, 'b@x', 2), (3, NULL, 3), (4, 'c@x', NULL)
CREATE INDEX i_email ON i_t (email)
CREATE INDEX i_email ON i_t (n)
CREATE INDEX i_zz ON i_nope (id)
CREATE INDEX i_zz ON i_t (zz)
CREATE UNIQUE INDEX i_uid ON i_t (id)
INSERT INTO i_t VALUES (1, 'dup', 9)
UPDATE i_t SET id = 2 WHERE id = 4
UPDATE i_t SET id = 40 WHERE id = 4
CREATE UNIQUE INDEX i_uemail ON i_t (email)
INSERT INTO i_t VALUES (5, NULL, 5)
INSERT INTO i_t VALUES (6, 'a@x', 6)
CREATE UNIQUE INDEX i_un ON i_t (n)
CREATE TABLE i_d (v INTEGER)
INSERT INTO i_d VALUES (1), (1)
CREATE UNIQUE INDEX i_dv ON i_d (v)
INSERT INTO i_d VALUES (1)
DROP INDEX i_email
DROP INDEX i_email
DROP INDEX i_nope
SELECT id AS id, email AS email FROM i_t WHERE email = 'b@x'
SELECT id AS id FROM i_t WHERE id = 40
SELECT id AS id FROM i_t WHERE email IS NULL ORDER BY id
SELECT id AS id FROM i_t WHERE n >= 2 AND n < 6 ORDER BY id
BEGIN
CREATE INDEX i_tx ON i_t (n)
ROLLBACK
CREATE INDEX i_tx ON i_t (n)
DROP INDEX i_tx
SELECT id AS id, n AS n FROM i_t ORDER BY id
