CREATE TABLE c_p (id INTEGER PRIMARY KEY, code TEXT UNIQUE, qty INTEGER NOT NULL DEFAULT 5, note TEXT DEFAULT 'n')
INSERT INTO c_p VALUES (NULL, 'a', 1, 'x')
INSERT INTO c_p (code) VALUES ('b')
INSERT INTO c_p VALUES (10, 'c', 3, NULL)
INSERT INTO c_p (id, code) VALUES (NULL, 'd')
INSERT INTO c_p VALUES (1, 'e', 1, 'x')
INSERT INTO c_p VALUES (20, 'a', 1, 'x')
INSERT INTO c_p (code) VALUES (NULL)
INSERT INTO c_p (code) VALUES ('f')
INSERT INTO c_p VALUES (21, 'g', NULL, 'x')
INSERT INTO c_p (id, code) VALUES (30, 'h')
INSERT INTO c_p VALUES (31, 'i')
SELECT * FROM c_p ORDER BY id
CREATE TABLE c_k (name TEXT PRIMARY KEY, v INTEGER)
INSERT INTO c_k VALUES ('k1', 1)
INSERT INTO c_k VALUES (NULL, 2)
INSERT INTO c_k VALUES ('k1', 3)
SELECT * FROM c_k ORDER BY name
CREATE TABLE c_par (id INTEGER PRIMARY KEY, label TEXT)
CREATE TABLE c_child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES c_par(id), tag TEXT)
INSERT INTO c_par VALUES (1, 'one'), (2, 'two')
INSERT INTO c_child VALUES (NULL, 1, 'a'), (NULL, NULL, 'b'), (NULL, 2, 'c')
INSERT INTO c_child VALUES (NULL, 9, 'orphan')
INSERT INTO c_child (pid) VALUES (2)
DELETE FROM c_par WHERE id = 1
DELETE FROM c_par WHERE id = 99
UPDATE c_child SET pid = 7 WHERE id = 1
UPDATE c_child SET pid = 1 WHERE id = 1
UPDATE c_par SET id = 5 WHERE id = 2
SELECT * FROM c_child ORDER BY id
SELECT * FROM c_par ORDER BY id
UPDATE c_p SET code = 'a' WHERE id = 2
UPDATE c_p SET qty = NULL WHERE id = 2
UPDATE c_p SET code = 'zz' WHERE id = 2
UPDATE c_p SET id = 1 WHERE id = 2
SELECT * FROM c_p ORDER BY id
