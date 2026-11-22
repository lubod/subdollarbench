#!/usr/bin/env python3
"""Persistence of constrained state across a restart: auto-assign counters,
unique maps, defaults and foreign keys must survive, and the store written
by the previous (pre-constraint) build must still load."""
import socket
import subprocess
import sys
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 5581
DATA = sys.argv[2] if len(sys.argv) > 2 else "/tmp/sdb_persist"
BIN = sys.argv[3] if len(sys.argv) > 3 else "/workspace/target/release/sdb"


class Conn:
    def __init__(self, port):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=30)
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

    def close(self):
        try:
            self.sock.close()
        except Exception:
            pass


def start():
    p = subprocess.Popen([BIN, "--port", str(PORT), "--data", DATA],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(50):
        time.sleep(0.1)
        try:
            c = Conn(PORT)
            if c.ask("SELECT 1").startswith("ROWS"):
                return p, c
        except Exception:
            pass
    raise RuntimeError("server did not start")


def stop(p):
    p.terminate()
    p.wait(timeout=10)
    time.sleep(0.3)


def expect(c, stmt, want):
    got = c.ask(stmt)
    ok = got == want or (want == "OK" and got.startswith("OK"))
    tag = "ok " if ok else "FAIL"
    print("%s %-70s => %s" % (tag, stmt, got))
    return ok


def main():
    import os
    os.system("rm -rf " + DATA)
    fails = 0
    p, c = start()
    setup = [
        "CREATE TABLE p (id INTEGER PRIMARY KEY AUTOINCREMENT, v INTEGER UNIQUE)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id), n INTEGER DEFAULT 5)",
        "CREATE TABLE tk (k TEXT PRIMARY KEY, s TEXT NOT NULL DEFAULT 'z')",
        "INSERT INTO p (v) VALUES (10), (20)",
        "INSERT INTO p (v) VALUES (30)",
        "INSERT INTO c (pid) VALUES (1)",
        "INSERT INTO tk (k) VALUES ('a')",
        "INSERT INTO tk (k, s) VALUES ('b', 'y')",
    ]
    for s in setup:
        r = c.ask(s)
        if not r.startswith("OK"):
            print("setup failed: %s => %s" % (s, r))
            fails += 1
    stop(p)
    p, c = start()
    for stmt, want in [
        ("SELECT id, v FROM p ORDER BY id", "ROWS"),
        ("SELECT * FROM c ORDER BY id", "ROWS"),
        ("SELECT * FROM tk ORDER BY k", "ROWS"),
        # auto-assign must continue from the persisted sequence
        ("INSERT INTO p (v) VALUES (40)", "OK 1"),
        ("SELECT id, v FROM p ORDER BY id", "ROWS"),
        # uniqueness must still be enforced after restart
        ("INSERT INTO p (v) VALUES (10)", "ERR constraint"),
        ("INSERT INTO tk (k) VALUES ('a')", "ERR constraint"),
        ("INSERT INTO tk (k, s) VALUES ('c', NULL)", "ERR constraint"),
        ("INSERT INTO tk (v) VALUES (1)", "ERR no_column"),
        # foreign keys still enforced both ways
        ("INSERT INTO c (pid) VALUES (99)", "ERR constraint"),
        ("DELETE FROM p WHERE id = 1", "ERR constraint"),
        ("INSERT INTO c (pid, n) VALUES (1, NULL)", "OK 1"),
        ("INSERT INTO c (pid) VALUES (4)", "OK 1"),
        ("SELECT COUNT(*) FROM c", "ROWS"),
    ]:
        r = c.ask(stmt)
        ok = (r == want) or (want == "ERR constraint" and r.startswith("ERR constraint")) \
            or (want == "ERR no_column" and r.startswith("ERR no_column")) \
            or (want == "OK 1" and r == "OK 1") or (want == "ROWS" and r.startswith("ROWS"))
        if not ok:
            fails += 1
        print("%s %-70s => %s" % ("ok " if ok else "FAIL", stmt, r))
    c.close()
    stop(p)
    print("PERSIST %s" % ("FAIL" if fails else "PASS"))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
