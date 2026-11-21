#!/usr/bin/env python3
"""Ordering probes round 2: where the primary-key block sits relative to
NOT NULL and ordinary coercion, in INSERT and UPDATE."""
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
    S = "oe9_"
    print("=== A: INSERT pk-dup (col1) vs other-col type (col0)")
    for stmt in [
        "CREATE TABLE %sa (x INTEGER, y INTEGER PRIMARY KEY)" % S,
        "INSERT INTO %sa VALUES (1, 1), (2, 2)" % S,
        "INSERT INTO %sa VALUES ('txt', 1)" % S,
        "INSERT INTO %sa VALUES ('txt', 9)" % S,
        "INSERT INTO %sa VALUES (NULL, 1)" % S,
        "INSERT INTO %sa VALUES (NULL, 9)" % S,
        "SELECT * FROM %sa" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))
    print("=== B: UPDATE pk-type vs NOT NULL")
    for stmt in [
        "CREATE TABLE %sb (x INTEGER, n INTEGER NOT NULL, y INTEGER PRIMARY KEY)" % S,
        "INSERT INTO %sb VALUES (1, 1, 1)" % S,
        "UPDATE %sb SET y = 'txt', n = NULL" % S,
        "UPDATE %sb SET n = NULL, y = 'txt'" % S,
        "UPDATE %sb SET y = 'txt', n = 5" % S,
        "SELECT * FROM %sb" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))
    print("=== C: INSERT pk-type vs NOT NULL")
    for stmt in [
        "CREATE TABLE %sc (x INTEGER, n INTEGER NOT NULL, y INTEGER PRIMARY KEY)" % S,
        "INSERT INTO %sc VALUES (1, 1, 1)" % S,
        "INSERT INTO %sc VALUES (5, NULL, 'txt')" % S,
        "INSERT INTO %sc VALUES (5, 'txt2', 'txt')" % S,
        "SELECT * FROM %sc" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))
    print("=== D: composite pk dup vs other-col type")
    for stmt in [
        "CREATE TABLE %sd (h INTEGER, a INTEGER, b INTEGER, PRIMARY KEY (a, b))" % S,
        "INSERT INTO %sd VALUES (1, 1, 1), (2, 2, 2)" % S,
        "UPDATE %sd SET h = 'txt', a = 1, b = 1" % S,
        "UPDATE %sd SET a = 1, b = 1, h = 'txt'" % S,
        "UPDATE %sd SET h = 'txt', a = 7, b = 7" % S,
        "INSERT INTO %sd VALUES ('txt', 1, 1)" % S,
        "INSERT INTO %sd VALUES ('txt', 7, 7)" % S,
        "INSERT INTO %sd VALUES (5, NULL, 7)" % S,
        "SELECT * FROM %sd" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))
    print("=== E: unique-col dup vs other type (regular unique, both orders)")
    for stmt in [
        "CREATE TABLE %se (u INTEGER UNIQUE, t INTEGER)" % S,
        "INSERT INTO %se VALUES (1, 1)" % S,
        "UPDATE %se SET u = 1, t = 'txt'" % S,
        "UPDATE %se SET t = 'txt', u = 1" % S,
        "INSERT INTO %se VALUES (1, 'txt')" % S,
        "SELECT * FROM %se" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))
    print("=== F: autoincrement pk assign vs type/notnull in same row")
    for stmt in [
        "CREATE TABLE %sf (x INTEGER, n INTEGER NOT NULL, y INTEGER PRIMARY KEY AUTOINCREMENT)" % S,
        "INSERT INTO %sf VALUES (1, 1, 10)" % S,
        "INSERT INTO %sf VALUES ('txt', NULL, NULL)" % S,
        "INSERT INTO %sf VALUES (5, NULL, NULL)" % S,
        "INSERT INTO %sf VALUES ('txt', 5, NULL)" % S,
        "SELECT * FROM %sf" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))


if __name__ == "__main__":
    main()
