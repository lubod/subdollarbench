#!/usr/bin/env python3
"""Coercion edge cases: text -> INTEGER in the reference."""
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


def main():
    c = Conn(REF)
    S = "co9_"
    print("=== insert text into INTEGER")
    for stmt in [
        "CREATE TABLE %si (a INTEGER, b TEXT)" % S,
        "INSERT INTO %si VALUES ('', 1)" % S,
        "INSERT INTO %si VALUES ('5', 2)" % S,
        "INSERT INTO %si VALUES (' 5 ', 3)" % S,
        "INSERT INTO %si VALUES ('5.0', 4)" % S,
        "INSERT INTO %si VALUES ('5.5', 5)" % S,
        "INSERT INTO %si VALUES ('-3', 6)" % S,
        "INSERT INTO %si VALUES ('+3', 7)" % S,
        "INSERT INTO %si VALUES ('1e2', 8)" % S,
        "INSERT INTO %si VALUES ('0x10', 9)" % S,
        "INSERT INTO %si VALUES ('', 10)" % S,
        "INSERT INTO %si VALUES (' ', 11)" % S,
        "INSERT INTO %si VALUES ('12abc', 12)" % S,
        "SELECT a, b FROM %si ORDER BY b" % S,
        "SELECT a = 0, a = '' FROM %si WHERE b = 1" % S,
        "SELECT a = 5, a = '5' FROM %si WHERE b = 2" % S,
        "SELECT a = 5, a = '5.0' FROM %si WHERE b = 4" % S,
        "SELECT a = 5, a = '5.5' FROM %si WHERE b = 5" % S,
        "SELECT a = 100, a = '1e2' FROM %si WHERE b = 8" % S,
        "SELECT a = 16 FROM %si WHERE b = 9" % S,
        "SELECT a = 5 FROM %si WHERE b = 12" % S,
        "UPDATE %si SET a = '' WHERE b = 3" % S,
        "UPDATE %si SET a = 'zz' WHERE b = 3" % S,
        "UPDATE %si SET b = 5 WHERE b = 3" % S,
        "SELECT a, b FROM %si ORDER BY b" % S,
        "CREATE TABLE %st (a TEXT)" % S,
        "INSERT INTO %st VALUES (5)" % S,
        "INSERT INTO %st VALUES ('')" % S,
        "SELECT a = 5, a = '5' FROM %st" % S,
        "CREATE TABLE %sn (a INTEGER NOT NULL)" % S,
        "INSERT INTO %sn VALUES ('')" % S,
        "INSERT INTO %sn VALUES (NULL)" % S,
        "CREATE TABLE %su (a INTEGER UNIQUE)" % S,
        "INSERT INTO %su VALUES (''), ('')" % S,
        "SELECT a = 0 FROM %su" % S,
        "INSERT INTO %su VALUES (0)" % S,
    ]:
        print("  %-64s => %s" % (stmt, c.ask(stmt)))


if __name__ == "__main__":
    main()
