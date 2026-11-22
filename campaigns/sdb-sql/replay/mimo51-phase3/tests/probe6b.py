#!/usr/bin/env python3
"""Multi-line-aware probe of the reference's transaction semantics."""
import socket, sys, threading, time

HOST = sys.argv[1] if len(sys.argv) > 1 else "ref-sql"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 5480


class Conn:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT), timeout=20)
        self.buf = b""

    def sendq(self, q):
        self.s.sendall(q.encode())

    def readline(self, timeout=20):
        self.s.settimeout(timeout)
        while b"\n" not in self.buf:
            d = self.s.recv(65536)
            if not d:
                raise EOFError("closed")
            self.buf += d
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode("utf-8", "replace")

    def ask(self, line, timeout=20):
        self.sendq(line + "\n")
        return self.recv_one(timeout)

    def recv_one(self, timeout=20):
        """Read one complete answer: ROWS..END or OK/ERR line."""
        first = self.readline(timeout)
        if first.startswith("ROWS "):
            out = [first]
            while True:
                l = self.readline(timeout)
                out.append(l)
                if l.startswith("END "):
                    return out
        return [first]

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


def show(tag, resp):
    print(f"  {tag!r:58} -> {resp}")


print("=== A: control statements, clean ===")
c = Conn()
for q in [
    "CREATE TABLE t (id INTEGER, s TEXT)",
    "BEGIN", "COMMIT",
    "ROLLBACK",
    "BEGIN", "BEGIN", "ROLLBACK", "ROLLBACK",
    "BEGIN", "INSERT INTO t VALUES (1, 'a')",
    "INSERT INTO t VALUES (2, 999)",          # valid: int into TEXT? no -> probe type err
    "SELECT * FROM t",
    "ROLLBACK", "SELECT * FROM t",
    "BEGIN", "INSERT INTO t VALUES (3, 'c')", "COMMIT", "SELECT * FROM t",
    "COMMIT", "ROLLBACK",
    "BEGIN TRANSACTION", "COMMIT TRANSACTION",
    "ROLLBACK TRANSACTION",
    "START TRANSACTION", "END", "END TRANSACTION",
    "START", "TRANSACTION",
    "BEGIN TRANSACTION TRANSACTION",
    "SAVEPOINT sp",
]:
    show(q, c.ask(q))
c.close()

print("=== A2: type error inside txn keeps it open ===")
c = Conn()
for q in [
    "CREATE TABLE t2 (id INTEGER, s TEXT)",
    "BEGIN",
    "INSERT INTO t2 VALUES (1, 'a')",
    "INSERT INTO t2 VALUES ('zz', 'b')",       # type error
    "INSERT INTO t2 VALUES (2, 'b', 'c')",     # arity error
    "SELECT * FROM t2",
    "COMMIT",
    "SELECT * FROM t2",
]:
    show(q, c.ask(q))
c.close()

print("=== A3: DDL inside txn + rollback ===")
c = Conn()
show("create", c.ask("CREATE TABLE base (id INTEGER)"))
show("insert", c.ask("INSERT INTO base VALUES (1)"))
show("begin", c.ask("BEGIN"))
show("create in txn", c.ask("CREATE TABLE inner1 (x INTEGER)"))
show("insert in txn", c.ask("INSERT INTO inner1 VALUES (7)"))
show("select base", c.ask("SELECT * FROM base"))
show("rollback", c.ask("ROLLBACK"))
show("select inner1", c.ask("SELECT * FROM inner1"))
show("select base", c.ask("SELECT * FROM base"))
c.close()

print("=== B: isolation (clean) ===")
a, b = Conn(), Conn()
show("A create", a.ask("CREATE TABLE iso (id INTEGER, s TEXT)"))
show("A begin", a.ask("BEGIN"))
show("A insert", a.ask("INSERT INTO iso VALUES (1, 'u')"))
show("B select (old)", b.ask("SELECT * FROM iso"))
show("A commit", a.ask("COMMIT"))
show("B select (new)", b.ask("SELECT * FROM iso"))
show("A begin2", a.ask("BEGIN"))
show("A insert2", a.ask("INSERT INTO iso VALUES (2, 'd')"))
show("A rollback", a.ask("ROLLBACK"))
show("B select (never)", b.ask("SELECT * FROM iso"))
a.close(); b.close()

print("=== C: concurrent write blocks (with rescue thread) ===")
a, b = Conn(), Conn()
show("A create", a.ask("CREATE TABLE cw (id INTEGER)"))
show("A begin", a.ask("BEGIN"))
show("A insert", a.ask("INSERT INTO cw VALUES (1)"))
res = {}

def do_b():
    try:
        res["b"] = b.ask("INSERT INTO cw VALUES (90)", timeout=15)
    except Exception as e:
        res["b"] = f"EXC {e}"

t = threading.Thread(target=do_b)
t.start()
time.sleep(1.0)
show("B select during txn (old?)", b.ask("SELECT * FROM cw") if False else "skipped (b busy)")
# a second reader connection
r = Conn()
show("R select during txn", r.ask("SELECT * FROM cw"))
show("A commit", a.ask("COMMIT"))
t.join(20)
show("B insert result", res.get("b"))
show("R select after", r.ask("SELECT * FROM cw"))
a.close(); b.close(); r.close()

print("=== C2: writer blocked, writer connection dies ===")
a, b = Conn(), Conn()
show("A create", a.ask("CREATE TABLE cw2 (id INTEGER)"))
show("A begin", a.ask("BEGIN"))
show("A insert", a.ask("INSERT INTO cw2 VALUES (1)"))
res = {}

def do_b2():
    try:
        res["b"] = b.ask("INSERT INTO cw2 VALUES (90)", timeout=15)
    except Exception as e:
        res["b"] = f"EXC {e}"

t = threading.Thread(target=do_b2)
t.start()
time.sleep(1.0)
show("A closes with open txn", "closing")
a.close()
t.join(20)
show("B insert result after A died", res.get("b"))
r = Conn()
show("R select", r.ask("SELECT * FROM cw2"))
r.close(); b.close()

print("=== D: lock taken at BEGIN or first write? ===")
a, b = Conn(), Conn()
show("A create", a.ask("CREATE TABLE lk (id INTEGER)"))
show("A begin (no writes)", a.ask("BEGIN"))
res = {}

def do_b3():
    try:
        res["b"] = b.ask("INSERT INTO lk VALUES (5)", timeout=5)
    except Exception as e:
        res["b"] = f"EXC {e}"

t = threading.Thread(target=do_b3)
t.start()
t.join(8)
show("B insert while A txn idle (no writes)", res.get("b", "STILL BLOCKED"))
show("A select", a.ask("SELECT * FROM lk", timeout=3) if "b" in res else "skip")
show("A rollback", a.ask("ROLLBACK", timeout=3) if "b" in res else "skip")
t.join(5)
show("B result final", res.get("b"))
show("A select final", a.ask("SELECT * FROM lk", timeout=3) if "b" in res else "skip")
a.close(); b.close()

print("=== D2: lock with A doing reads in txn ===")
a, b = Conn(), Conn()
show("A create", a.ask("CREATE TABLE lk2 (id INTEGER)"))
show("A insert", a.ask("INSERT INTO lk2 VALUES (1)"))
show("A begin", a.ask("BEGIN"))
show("A select", a.ask("SELECT * FROM lk2"))
res = {}

def do_b4():
    try:
        res["b"] = b.ask("INSERT INTO lk2 VALUES (5)", timeout=5)
    except Exception as e:
        res["b"] = f"EXC {e}"

t = threading.Thread(target=do_b4)
t.start()
t.join(8)
show("B insert while A txn read-only", res.get("b", "STILL BLOCKED"))
show("A rollback", a.ask("ROLLBACK", timeout=3) if "b" in res else "A still holds")
t.join(5)
show("B final", res.get("b"))
a.close(); b.close()

print("=== E: drop connection with open txn ===")
c = Conn()
show("create", c.ask("CREATE TABLE dr (id INTEGER)"))
show("begin", c.ask("BEGIN"))
show("insert", c.ask("INSERT INTO dr VALUES (1)"))
c.close()
time.sleep(0.3)
d = Conn()
show("new conn select", d.ask("SELECT * FROM dr"))
show("new conn commit", d.ask("COMMIT"))
show("new conn rollback", d.ask("ROLLBACK"))
d.close()

print("=== F: identifiers ===")
c = Conn()
for q in [
    "CREATE TABLE kw (begin INTEGER, end INTEGER, start INTEGER, transaction INTEGER, work INTEGER, rollback INTEGER)",
    "CREATE TABLE kw2 (begin INTEGER, end INTEGER, start INTEGER, work INTEGER, rollback INTEGER)",
    "CREATE TABLE kw3 (begin INTEGER, end INTEGER, start INTEGER, rollback INTEGER)",
    "CREATE TABLE kw4 (begin INTEGER, end INTEGER, rollback INTEGER)",
    "CREATE TABLE kw5 (begin INTEGER, rollback INTEGER)",
    "CREATE TABLE kw6 (begin INTEGER)",
    "INSERT INTO kw6 VALUES (1)",
    "SELECT begin FROM kw6",
    "SELECT begin AS end FROM kw6",
    "SELECT 1 AS commit",
    "SELECT 1 AS begin",
    "SELECT 1 AS rollback",
    "UPDATE kw6 SET begin = 2",
    "SELECT * FROM kw6 ORDER BY begin",
]:
    show(q, c.ask(q))
c.close()

print("=== G: pipelined transaction ===")
c = Conn()
c.sendq("CREATE TABLE pl (id INTEGER)\nBEGIN\nINSERT INTO pl VALUES (1)\nINSERT INTO pl VALUES (2)\nCOMMIT\nSELECT * FROM pl\n")
for _ in range(6):
    show("pipelined", c.recv_one())
c.close()

print("=== H: two connections both BEGIN, both write ===")
a, b = Conn(), Conn()
show("A create", a.ask("CREATE TABLE two (id INTEGER)"))
show("A begin", a.ask("BEGIN"))
show("B begin", b.ask("BEGIN", timeout=4))
show("A insert", a.ask("INSERT INTO two VALUES (1)", timeout=4))
res = {}

def do_bi():
    try:
        res["b"] = b.ask("INSERT INTO two VALUES (2)", timeout=8)
    except Exception as e:
        res["b"] = f"EXC {e}"

t = threading.Thread(target=do_bi)
t.start()
time.sleep(1)
show("A commit", a.ask("COMMIT", timeout=4))
t.join(10)
show("B insert", res.get("b"))
show("B commit", b.ask("COMMIT", timeout=4))
r = Conn()
show("final", r.ask("SELECT * FROM two"))
r.close(); a.close(); b.close()
print("DONE")
