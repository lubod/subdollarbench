SELECT 7 / 2 AS q, -7 / 2 AS nq, 7 % 3 AS r, -7 % 3 AS nr, 7 % -3 AS r2
SELECT 5 / 0 AS dz, 5 % 0 AS mz
SELECT 1 + 2 * 3 AS p1, (1 + 2) * 3 AS p2, 10 - 4 - 3 AS p3, 2 * 3 % 4 AS p4
SELECT -2 * -3 AS neg, - 4 + 1 AS n2
SELECT 1 < 2 AS a, 2 <= 2 AS b, 3 > 4 AS c, 3 >= 3 AS d, 1 = 1 AS e, 1 <> 1 AS f, 1 != 2 AS g
SELECT NULL = NULL AS a, NULL IS NULL AS b, 1 IS NOT NULL AS c, NULL IS NOT NULL AS d
SELECT 1 AND NULL AS a, 0 AND NULL AS b, NULL AND 0 AS c, NULL AND 1 AS d
SELECT 1 OR NULL AS a, 0 OR NULL AS b, NULL OR 1 AS c, NULL OR 0 AS d
SELECT NOT 0 AS a, NOT 5 AS b, NOT NULL AS c
SELECT NOT 1 = 2 AS p, NOT (1 = 2) AS q, 1 OR 0 AND 0 AS r, (1 OR 0) AND 0 AS s
SELECT 'a' || 'b' AS c1, 'a' || NULL AS c2, NULL || 'x' AS c3, 'x' || 'y' || 'z' AS c4
SELECT length('abc') AS l, upper('aBc') AS u, lower('AbC') AS lo, abs(-5) AS a, abs(3) AS a2
SELECT coalesce(NULL, 4) AS c, coalesce(NULL, NULL) AS c2, length(NULL) AS ln, upper(NULL) AS un
SELECT 1 AS one
SELECT 1 + 1 AS two, 'x' AS s
SELECT 1 + 1 * 2 = 3 AND 'a' || 'b' = 'ab' AS chk
CREATE TABLE e_t (a INTEGER, b TEXT)
INSERT INTO e_t VALUES (1, 'x'), (2, NULL), (NULL, 'y'), (4, 'xy'), (-5, 'Z')
SELECT a AS a FROM e_t WHERE a > 1 ORDER BY a
SELECT a AS a, b AS b FROM e_t WHERE b IS NULL ORDER BY a
SELECT a AS a FROM e_t WHERE a = NULL ORDER BY a
SELECT a AS a FROM e_t WHERE NOT a > 1 ORDER BY a
SELECT b AS b FROM e_t WHERE b || 'z' = 'xz' ORDER BY b
SELECT a + 1 AS a1 FROM e_t WHERE a * 2 < 5 AND b IS NOT NULL ORDER BY a1
SELECT a AS a FROM e_t WHERE a % 2 = 0 OR b = 'Z' ORDER BY a
SELECT upper(b) AS u FROM e_t WHERE length(b) = 2 ORDER BY u
SELECT coalesce(b, 'none') AS c FROM e_t ORDER BY c
SELECT a AS a FROM e_t WHERE a < 2 OR a > 3 AND b IS NOT NULL ORDER BY a
SELECT a AS a FROM e_t WHERE (a < 2 OR a > 3) AND b IS NOT NULL ORDER BY a
SELECT a AS a FROM e_t WHERE a - 1 = 0 AND NOT b = 'x' ORDER BY a
SELECT a AS a, a / 2 AS h, a % 2 AS m FROM e_t WHERE a IS NOT NULL ORDER BY a
SELECT a AS a FROM e_t WHERE 1 = 1 ORDER BY a
