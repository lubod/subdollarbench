#!/usr/bin/env python3
"""Ordering probes: within a row, does the reference interleave coercion and
constraint checks column by column (and in which order)?"""
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
    S = "od9_"
    print("=== P1: type in col0 vs pk-dup in col1 (SET order: pk first)")
    for stmt in [
        "CREATE TABLE %sa (x INTEGER, y INTEGER PRIMARY KEY)" % S,
        "INSERT INTO %sa VALUES (1, 1), (2, 2)" % S,
        "UPDATE %sa SET y = 99, x = 'txt'" % S,
        "UPDATE %sa SET x = 'txt', y = 99" % S,
        "UPDATE %sa SET y = 99, x = 'ok'" % S,
        "INSERT INTO %sa VALUES ('txt', 99)" % S,
        "INSERT INTO %sa VALUES (99, 'txt')" % S,
        "SELECT * FROM %sa" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))
    print("=== P2: type in col2 vs unique-dup in col1 (SET: type first)")
    for stmt in [
        "CREATE TABLE %sb (a INTEGER, b INTEGER UNIQUE, c INTEGER)" % S,
        "INSERT INTO %sb VALUES (1, 1, 1), (2, 2, 2)" % S,
        "UPDATE %sb SET c = 'txt', b = 1" % S,
        "UPDATE %sb SET b = 1, c = 'txt'" % S,
        "INSERT INTO %sb VALUES (9, 1, 'txt')" % S,
        "INSERT INTO %sb VALUES (9, 'txt', 1)" % S,
        "SELECT * FROM %sb" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))
    print("=== P3: not-null vs type ordering")
    for stmt in [
        "CREATE TABLE %sc (a INTEGER, n INTEGER NOT NULL)" % S,
        "INSERT INTO %sc VALUES (1, 1)" % S,
        "UPDATE %sc SET n = NULL, a = 'txt'" % S,
        "UPDATE %sc SET a = 'txt', n = NULL" % S,
        "UPDATE %sc SET n = NULL, a = 5" % S,
        "INSERT INTO %sc VALUES ('txt', NULL)" % S,
        "INSERT INTO %sc VALUES (NULL, 'txt')" % S,
        "SELECT * FROM %sc" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))
    print("=== P4: fk-value vs type ordering (child insert/update)")
    for stmt in [
        "CREATE TABLE %sp (id INTEGER PRIMARY KEY)" % S,
        "INSERT INTO %sp VALUES (1)" % S,
        "CREATE TABLE %sq (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES %sp(id), n INTEGER)" % (S, S),
        "INSERT INTO %sq VALUES (1, 99, 'txt')" % S,
        "INSERT INTO %sq VALUES (1, 1, 'txt')" % S,
        "INSERT INTO %sq VALUES ('txt', 99, 5)" % S,
        "INSERT INTO %sq VALUES (2, 99, 5)" % S,
        "UPDATE %sq SET pid = 99, n = 'txt' WHERE id = 1" % S,
        "UPDATE %sq SET n = 'txt', pid = 99 WHERE id = 1" % S,
        "SELECT * FROM %sq" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))
    print("=== P5: unique vs type in INSERT, dup in col0 type in col1")
    for stmt in [
        "CREATE TABLE %sd (a INTEGER UNIQUE, b INTEGER)" % S,
        "INSERT INTO %sd VALUES (1, 1)" % S,
        "INSERT INTO %sd VALUES (1, 'txt')" % S,
        "INSERT INTO %sd VALUES ('txt', 1)" % S,
        "INSERT INTO %sd VALUES (1, 1), (9, 'txt')" % S,
        "SELECT * FROM %sd" % S,
    ]:
        print("  %-58s => %s" % (stmt, c.ask(stmt)))


if __name__ == "__main__":
    main()
