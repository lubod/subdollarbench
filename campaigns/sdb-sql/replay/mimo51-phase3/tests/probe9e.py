#!/usr/bin/env python3
"""Ticket 9 probes, round 5: uniqueness timing in UPDATE, negative rowids."""
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
    S = "u9_"

    run("T1. UPDATE unique shift (incremental vs end-state)", [
        f"CREATE TABLE {S}a (id INTEGER PRIMARY KEY, u TEXT UNIQUE)",
        f"INSERT INTO {S}a VALUES (1, 'a'), (2, 'b')",
        f"UPDATE {S}a SET u = CASE WHEN u = 'a' THEN 'b' ELSE 'z' END",
        f"SELECT * FROM {S}a ORDER BY id",
        f"UPDATE {S}a SET u = CASE WHEN u = 'b' THEN 'z' ELSE 'b' END",
        f"SELECT * FROM {S}a ORDER BY id",
    ], c)

    run("T2. UPDATE rowid shift", [
        f"CREATE TABLE {S}b (id INTEGER PRIMARY KEY, s TEXT)",
        f"INSERT INTO {S}b VALUES (1, 'a'), (2, 'b')",
        f"UPDATE {S}b SET id = id + 1",
        f"SELECT * FROM {S}b ORDER BY id",
        f"UPDATE {S}b SET id = id - 1 ORDER BY id DESC",
        f"SELECT * FROM {S}b ORDER BY id",
    ], c)

    run("T3. negative rowid auto-assign", [
        f"CREATE TABLE {S}c (id INTEGER PRIMARY KEY, s TEXT)",
        f"INSERT INTO {S}c VALUES (-5, 'a')",
        f"INSERT INTO {S}c (s) VALUES ('b')",
        f"INSERT INTO {S}c VALUES (-100, 'c'), (NULL, 'd')",
        f"SELECT id, s FROM {S}c ORDER BY id",
        f"DELETE FROM {S}c",
        f"INSERT INTO {S}c (s) VALUES ('e')",
        f"SELECT id, s FROM {S}c ORDER BY id",
    ], c)

    run("T4. parent-side update: freed value retaken by later row", [
        f"CREATE TABLE {S}d (k INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}dc (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}d(k))",
        f"INSERT INTO {S}d VALUES (1), (2)",
        f"INSERT INTO {S}dc VALUES (1, 1)",
        f"UPDATE {S}d SET k = CASE WHEN k = 1 THEN 3 ELSE 1 END",
        f"SELECT * FROM {S}d ORDER BY k",
        f"SELECT * FROM {S}dc",
    ], c)

    run("T5. multi-row delete with limit + fk, update window fk", [
        f"CREATE TABLE {S}e (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}ec (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}e(id))",
        f"INSERT INTO {S}e VALUES (1), (2), (3)",
        f"INSERT INTO {S}ec VALUES (1, 1), (2, 2)",
        f"DELETE FROM {S}e ORDER BY id LIMIT 1",
        f"SELECT * FROM {S}e ORDER BY id",
        f"DELETE FROM {S}e ORDER BY id DESC LIMIT 1",
        f"SELECT * FROM {S}e ORDER BY id",
        f"UPDATE {S}e SET id = 9 ORDER BY id DESC LIMIT 1",
        f"SELECT * FROM {S}e ORDER BY id",
    ], c)

    run("T6. update child fk col + unique same row; delete zero rows fk", [
        f"CREATE TABLE {S}f (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}fc (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}f(id) UNIQUE)",
        f"INSERT INTO {S}f VALUES (1), (2)",
        f"INSERT INTO {S}fc VALUES (1, 1), (2, 2)",
        f"UPDATE {S}fc SET pid = 1 WHERE id = 2",
        f"UPDATE {S}fc SET pid = 5 WHERE id = 2",
        f"UPDATE {S}fc SET pid = NULL WHERE id = 2",
        f"SELECT * FROM {S}fc ORDER BY id",
        f"DELETE FROM {S}f WHERE id = 99",
        f"DELETE FROM {S}fc WHERE id = 99",
        f"DELETE FROM {S}f WHERE id = 99",
    ], c)


if __name__ == "__main__":
    main()
