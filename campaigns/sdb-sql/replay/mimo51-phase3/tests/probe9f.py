#!/usr/bin/env python3
"""Ticket 9 probes, round 6: FK check timing (deferred vs immediate)."""
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
    S = "v9_"

    run("F1. self-FK insert: row1 references later row", [
        f"CREATE TABLE {S}a (id INTEGER PRIMARY KEY, mgr INTEGER REFERENCES {S}a(id))",
        f"INSERT INTO {S}a (id, mgr) VALUES (10, 11), (11, NULL)",
        f"SELECT * FROM {S}a ORDER BY id",
        f"INSERT INTO {S}a (id, mgr) VALUES (12, 13), (13, 12)",
        f"SELECT * FROM {S}a ORDER BY id",
        f"INSERT INTO {S}a (id, mgr) VALUES (14, 99), (15, NULL)",
        f"SELECT * FROM {S}a ORDER BY id",
        f"INSERT INTO {S}a (id, mgr) VALUES (16, 99), (17, 'txt')",
        f"SELECT * FROM {S}a ORDER BY id",
    ], c)

    run("F2. child insert: fk violation row1 vs type row2", [
        f"CREATE TABLE {S}p (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}p(id), n INTEGER)",
        f"INSERT INTO {S}p VALUES (1)",
        f"INSERT INTO {S}c VALUES (1, 99, 5), (2, 1, 'txt')",
        f"SELECT * FROM {S}c",
        f"INSERT INTO {S}c VALUES (1, 1, 'txt'), (2, 99, 5)",
        f"SELECT * FROM {S}c",
        f"INSERT INTO {S}c VALUES (3, 99, 5), (4, 1, 7)",
        f"SELECT * FROM {S}c",
    ], c)

    run("F3. update: parent-side violation vs type", [
        f"CREATE TABLE {S}d (k INTEGER PRIMARY KEY, n INTEGER)",
        f"CREATE TABLE {S}dc (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}d(k))",
        f"INSERT INTO {S}d VALUES (1, 1), (2, 2)",
        f"INSERT INTO {S}dc VALUES (1, 1)",
        f"UPDATE {S}d SET k = CASE WHEN k = 1 THEN 3 ELSE 'txt' END",
        f"SELECT * FROM {S}d ORDER BY k",
        f"UPDATE {S}d SET k = CASE WHEN k = 1 THEN 3 ELSE 2 END, n = CASE WHEN k = 1 THEN 1 ELSE 'txt' END",
        f"SELECT * FROM {S}d ORDER BY k",
        f"UPDATE {S}d SET n = 'txt', k = CASE WHEN k = 1 THEN 3 ELSE 9 END",
        f"SELECT * FROM {S}d ORDER BY k",
    ], c)

    run("F4. update child: fk violation vs type in same/later row", [
        f"CREATE TABLE {S}e (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}ec (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}e(id), n INTEGER)",
        f"INSERT INTO {S}e VALUES (1)",
        f"INSERT INTO {S}ec VALUES (1, 1, 1), (2, 1, 2)",
        f"UPDATE {S}ec SET pid = CASE WHEN id = 1 THEN 99 ELSE 'txt' END",
        f"SELECT * FROM {S}ec ORDER BY id",
        f"UPDATE {S}ec SET pid = CASE WHEN id = 1 THEN 99 ELSE 1 END, n = CASE WHEN id = 1 THEN 1 ELSE 'txt' END",
        f"SELECT * FROM {S}ec ORDER BY id",
    ], c)

    run("F5. delete: parent-side violation ordering + self delete", [
        f"CREATE TABLE {S}f (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}fc (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}f(id))",
        f"INSERT INTO {S}f VALUES (1), (2)",
        f"INSERT INTO {S}fc VALUES (1, 1)",
        f"DELETE FROM {S}f WHERE id = 1",
        f"SELECT * FROM {S}f",
        f"CREATE TABLE {S}g (id INTEGER PRIMARY KEY, mgr INTEGER REFERENCES {S}g(id))",
        f"INSERT INTO {S}g VALUES (1, NULL), (2, 1)",
        f"DELETE FROM {S}g WHERE id = 1",
        f"SELECT * FROM {S}g",
        f"UPDATE {S}g SET id = 5 WHERE id = 2",
        f"SELECT * FROM {S}g",
        f"UPDATE {S}g SET id = 2 WHERE id = 5",
        f"UPDATE {S}g SET mgr = id",
        f"SELECT * FROM {S}g",
    ], c)


if __name__ == "__main__":
    main()
