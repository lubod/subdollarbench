#!/usr/bin/env python3
"""Probe the reference server for transaction semantics (ticket 6)."""
import socket, sys, time

HOST = sys.argv[1] if len(sys.argv) > 1 else "ref-sql"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 5480


class Conn:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT), timeout=10)
        self.buf = b""

    def send(self, line):
        self.s.sendall(line.encode() + b"\n")

    def sendq(self, q):
        self.s.sendall(q.encode())

    def readline(self, timeout=10):
        self.s.settimeout(timeout)
        while b"\n" not in self.buf:
            d = self.s.recv(65536)
            if not d:
                raise EOFError("closed")
            self.buf += d
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode("utf-8", "replace")

    def ask(self, line, timeout=10):
        self.send(line)
        return self.readline(timeout)

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


def show(tag, resp):
    print(f"  {tag!r:60} -> {resp!r}")


print("=== P1: basic control statements, one connection ===")
c = Conn()
for q in [
    "CREATE TABLE t (id INTEGER, s TEXT)",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "BEGIN",
    "BEGIN",
    "ROLLBACK",
    "ROLLBACK",
    "BEGIN",
    "INSERT INTO t VALUES (1, 'a')",
    "INSERT INTO t VALUES (2, 'zz')",       # type error inside txn
    "SELECT * FROM t",                       # sees own changes
    "ROLLBACK",
    "SELECT * FROM t",                       # rolled back
    "BEGIN",
    "INSERT INTO t VALUES (3, 'c')",
    "COMMIT",
    "SELECT * FROM t",
    "COMMIT",                                # no txn now -> ERR txn
    "ROLLBACK",                              # ERR txn
    "BEGIN TRANSACTION",
    "COMMIT",
    "START TRANSACTION",
    "END",
    "bEgIn",
    "cOmMiT",
]:
    show(q, c.ask(q))
c.close()

print("=== P2: junk after control words ===")
c = Conn()
for q in [
    "CREATE TABLE t2 (id INTEGER)",
    "BEGIN FOO",
    "COMMIT FOO",
    "ROLLBACK FOO",
    "BEGIN TRANSACTION",
    "ROLLBACK TRANSACTION",
    "COMMIT TRANSACTION",
    "BEGIN WORK",
    "COMMIT WORK",
    "ROLLBACK WORK",
    "BEGIN;;",
    "COMMIT ;",
]:
    show(q, c.ask(q))
c.close()

print("=== P3: begin/commit/rollback as identifiers ===")
c = Conn()
for q in [
    "CREATE TABLE begin (id INTEGER)",
    "CREATE TABLE t3 (begin INTEGER, commit TEXT)",
    "INSERT INTO t3 VALUES (1, 'x')",
    "SELECT begin FROM t3",
    "SELECT commit FROM t3",
    "SELECT * FROM begin",
    "UPDATE t3 SET begin = 2",
    "DELETE FROM t3 WHERE begin = 2",
    "CREATE TABLE ok (begin INTEGER)",
]:
    show(q, c.ask(q))
c.close()

print("=== P4: isolation, two connections ===")
a, b = Conn(), Conn()
show("A: create", a.ask("CREATE TABLE iso (id INTEGER, s TEXT)"))
show("A: begin", a.ask("BEGIN"))
show("A: insert", a.ask("INSERT INTO iso VALUES (1, 'uncommitted')"))
show("B: select (must not see)", b.ask("SELECT * FROM iso"))
show("A: commit", a.ask("COMMIT"))
show("B: select (must see)", b.ask("SELECT * FROM iso"))
show("A: begin2", a.ask("BEGIN"))
show("A: insert2", a.ask("INSERT INTO iso VALUES (2, 'doomed')"))
show("A: rollback", a.ask("ROLLBACK"))
show("B: select (never)", b.ask("SELECT * FROM iso"))
a.close()
b.close()

print("=== P5: concurrent write while txn open ===")
a, b = Conn(), Conn()
show("A: create", a.ask("CREATE TABLE cw (id INTEGER, s TEXT)"))
show("A: begin", a.ask("BEGIN"))
show("A: insert", a.ask("INSERT INTO cw VALUES (1, 'a')"))
show("B: insert (autocommit)", b.ask("INSERT INTO cw VALUES (90, 'b')", timeout=6))
show("B: select", b.ask("SELECT * FROM cw", timeout=6))
show("B: begin", b.ask("BEGIN", timeout=6))
show("B: insert in txn", b.ask("INSERT INTO cw VALUES (91, 'c')", timeout=6))
show("A: rollback", a.ask("ROLLBACK", timeout=6))
show("B: select after A rollback", b.ask("SELECT * FROM cw", timeout=6))
show("B: commit", b.ask("COMMIT", timeout=6))
show("A: select", a.ask("SELECT * FROM cw", timeout=6))
a.close()
b.close()

print("=== P6: errors inside txn keep it open; DDL in txn ===")
c = Conn()
for q in [
    "CREATE TABLE e (id INTEGER, s TEXT)",
    "BEGIN",
    "INSERT INTO e VALUES (1, 'a')",
    "INSERT INTO e VALUES (2, 3, 4)",        # arity error
    "SELECT * FROM e",
    "CREATE TABLE e2 (x INTEGER)",
    "INSERT INTO e2 VALUES (5)",
    "DROP TABLE e",                           # maybe unsupported
]:
    show(q, c.ask(q))
show("ROLLBACK", c.ask("ROLLBACK"))
for q in ["SELECT * FROM e", "SELECT * FROM e2"]:
    show(q, c.ask(q))
c.close()

print("=== P7: connection drops with open txn ===")
c = Conn()
show("create", c.ask("CREATE TABLE dr (id INTEGER)"))
show("begin", c.ask("BEGIN"))
show("insert", c.ask("INSERT INTO dr VALUES (1)"))
c.close()
time.sleep(0.3)
d = Conn()
show("new conn select", d.ask("SELECT * FROM dr"))
show("new conn commit (no txn)", d.ask("COMMIT"))
d.close()

print("=== P8: pipelined transaction ===")
c = Conn()
c.sendq(
    "CREATE TABLE pl (id INTEGER)\nBEGIN\nINSERT INTO pl VALUES (1)\nINSERT INTO pl VALUES (2)\nCOMMIT\nSELECT * FROM pl\n"
)
for _ in range(6):
    show("pipelined", c.readline())
c.close()

print("=== P9: SELECT with no changes in txn, commit ok ===")
c = Conn()
show("begin", c.ask("BEGIN"))
show("select", c.ask("SELECT 1 AS x"))
show("commit", c.ask("COMMIT"))
c.close()
