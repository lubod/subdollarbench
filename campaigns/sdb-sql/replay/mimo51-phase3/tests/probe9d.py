#!/usr/bin/env python3
"""Ticket 9 probes, round 4: update row-ordering, structural scope."""
import socket

REF = ("ref-sql", 5480)


class Conn:
    def __init__(self, addr):
        self.sock = socket.create_connection(addr, timeout=30)
        self.file = self.sock.makefile("rb")

    def ask(self, stmt):
        self.sock.sendall((stmt + "\n").encode())
        first = self.file.readline().decode()
        if first.startswith("ROWS "):
            hdr = self.file.readline().decode().rstrip("\n")
            rows = []
            while True:
                line = self.file.readline().decode()
                if line.startswith("END "):
                    n = int(line.split()[1])
                    break
                rows.append(line.rstrip("\n"))
            return "ROWS[%s] n=%d %s" % (hdr, n, rows[:8])
        return first.rstrip("\n")


def run(title, stmts, c=None):
    print("=== %s" % title)
    for s in stmts:
        print("  %-84s => %s" % (s, c.ask(s)))


def main():
    c = Conn(REF)
    S = "s9_"

    run("A. multi-row UPDATE: constraint in row1 vs type in row2", [
        f"CREATE TABLE {S}t (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER NOT NULL)",
        f"INSERT INTO {S}t VALUES (1, 'a', 1), (2, 'b', 2)",
        f"UPDATE {S}t SET u = CASE WHEN id = 1 THEN 'b' ELSE 'z' END, n = CASE WHEN id = 1 THEN 1 ELSE 'txt' END",
        f"UPDATE {S}t SET u = CASE WHEN id = 1 THEN 'zz' ELSE 'b' END, n = CASE WHEN id = 1 THEN 'txt' ELSE 2 END",
        f"SELECT * FROM {S}t ORDER BY id",
        f"UPDATE {S}t SET u = 'q', n = CASE WHEN id = 1 THEN 1 ELSE 'txt' END",
        f"SELECT * FROM {S}t ORDER BY id",
    ], c)

    run("B. INSERT structural fires even when fk col omitted from list", [
        f"CREATE TABLE {S}p1 (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}c1 (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}p1(id), s TEXT)",
        f"INSERT INTO {S}c1 (id, s) VALUES (1, 'x')",
        f"CREATE TABLE {S}p2 (v INTEGER)",
        f"CREATE TABLE {S}c2 (id INTEGER PRIMARY KEY, pv INTEGER REFERENCES {S}p2(v), s TEXT)",
        f"INSERT INTO {S}c2 (id, s) VALUES (1, 'x')",
        f"INSERT INTO {S}c2 (s) VALUES ('y')",
    ], c)

    run("C. autoinc batch seq", [
        f"CREATE TABLE {S}a (id INTEGER PRIMARY KEY AUTOINCREMENT, s TEXT)",
        f"INSERT INTO {S}a (id, s) VALUES (NULL, 'a'), (100, 'b'), (NULL, 'c')",
        f"SELECT id, s FROM {S}a ORDER BY id",
        f"INSERT INTO {S}a (s) VALUES ('d')",
        f"UPDATE {S}a SET id = 200 WHERE id = 1",
        f"INSERT INTO {S}a (s) VALUES ('e')",
        f"SELECT id, s FROM {S}a ORDER BY id",
        f"DELETE FROM {S}a WHERE id = 200",
        f"INSERT INTO {S}a (s) VALUES ('f')",
        f"SELECT id, s FROM {S}a ORDER BY id",
    ], c)

    run("D. plain pk: max after update raising max", [
        f"CREATE TABLE {S}b (id INTEGER PRIMARY KEY, s TEXT)",
        f"INSERT INTO {S}b VALUES (5, 'a')",
        f"UPDATE {S}b SET id = 50 WHERE id = 5",
        f"INSERT INTO {S}b (s) VALUES ('b')",
        f"SELECT id, s FROM {S}b ORDER BY id",
        f"DELETE FROM {S}b WHERE id = 50",
        f"INSERT INTO {S}b (s) VALUES ('c')",
        f"SELECT id, s FROM {S}b ORDER BY id",
    ], c)

    run("E. delete parent: multiple children, one ok one not", [
        f"CREATE TABLE {S}p (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}p(id))",
        f"INSERT INTO {S}p VALUES (1), (2)",
        f"INSERT INTO {S}c VALUES (1, 1), (2, 2)",
        f"DELETE FROM {S}p WHERE id IN (1, 2)" if False else f"DELETE FROM {S}p",
        f"SELECT * FROM {S}c",
        f"DELETE FROM {S}c",
        f"DELETE FROM {S}p",
        f"SELECT COUNT(*) FROM {S}p",
    ], c)

    run("F. update child fk to same value / update parent no-change", [
        f"CREATE TABLE {S}p2 (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}c2 (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}p2(id))",
        f"INSERT INTO {S}p2 VALUES (1)",
        f"INSERT INTO {S}c2 VALUES (1, 1)",
        f"UPDATE {S}c2 SET pid = 1",
        f"UPDATE {S}p2 SET id = 1",
        f"UPDATE {S}p2 SET id = 1 WHERE id = 1",
        f"SELECT * FROM {S}c2",
    ], c)

    run("G. not-null pk text + defaults in same insert batch", [
        f"CREATE TABLE {S}g (k TEXT PRIMARY KEY, v INTEGER NOT NULL DEFAULT 7)",
        f"INSERT INTO {S}g (k) VALUES ('a')",
        f"INSERT INTO {S}g (k, v) VALUES ('b', NULL)",
        f"INSERT INTO {S}g (v) VALUES (1)",
        f"SELECT * FROM {S}g",
        f"INSERT INTO {S}g (k, v) VALUES ('a', 1), ('c', 2)",
        f"SELECT * FROM {S}g",
    ], c)


if __name__ == "__main__":
    main()
