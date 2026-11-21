#!/usr/bin/env python3
"""Ticket 4 persistence test: write -> SIGTERM -> restart -> byte-identical reads.

Covers:
  1. seeded dataset survives a clean stop/start (values, order, counts, errors)
  2. torn trailing line in the log is dropped, earlier rows survive, and new
     writes appended after recovery are visible on the NEXT restart
     (regression: write-cursor must be repositioned after truncation)
  3. a store written by the previous binary (legacy build) is readable
"""
import os
import shutil
import signal
import socket
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRV = os.path.join(ROOT, "target", "release", "sdb")
OLD = os.path.join(ROOT, ".tmpwork", "sdb_old")
PORT = 5591
SCRATCH = "/tmp/sdbtest"


class Client:
    def __init__(self, port=PORT):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.buf = b""

    def send(self, stmts):
        data = "".join(s if s.endswith("\n") else s + "\n" for s in stmts)
        self.sock.sendall(data.encode())

    def read_line(self):
        while b"\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("connection closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode()

    def ask(self, stmts):
        """Send statements, return the full raw answer for each."""
        self.send(stmts)
        out = []
        for _ in stmts:
            first = self.read_line()
            if first.startswith("ROWS "):
                frame = [first]
                while True:
                    l = self.read_line()
                    frame.append(l)
                    if l.startswith("END "):
                        break
                out.append("\n".join(frame) + "\n")
            else:
                out.append(first + "\n")
        return out

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def start(port, data):
    p = subprocess.Popen(
        [SRV, "--port", str(port), "--data", data],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    for _ in range(100):
        try:
            c = Client(port)
            c.close()
            return p
        except OSError:
            if p.poll() is not None:
                raise RuntimeError("server died: " + (p.stderr.read().decode() or ""))
            time.sleep(0.05)
    raise RuntimeError("server did not come up")


def stop(p, sig=signal.SIGTERM):
    p.send_signal(sig)
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()
        raise RuntimeError("server ignored SIGTERM")


def check(name, got, want):
    if got != want:
        print("FAIL", name)
        print(" got:", repr(got))
        print("want:", repr(want))
        sys.exit(1)
    print("ok  ", name)


STATEMENTS = [
    "CREATE TABLE t (id INTEGER, s TEXT)",
    "CREATE TABLE u (a INTEGER, b INTEGER)",
    "CREATE TABLE emptytab (x INTEGER, y TEXT)",
    "INSERT INTO t VALUES (1, 'a'), (2, NULL), (3, 'has\\ttab')",
    "INSERT INTO t (id, s) VALUES (4, 'back\\\\slash'), (5, ''), (6, 'quo''te')",
    "INSERT INTO t VALUES (-7, 'unicode ünïcødé')",
    "INSERT INTO u VALUES (1, 2), (3, 4), (5, 6)",
    "UPDATE u SET a = b, b = a WHERE a = 1",
    "UPDATE t SET s = 'x' WHERE id = 2",
    "UPDATE t SET id = id + 100 WHERE id > 4",
    "DELETE FROM u WHERE a = 999",
    "DELETE FROM u WHERE b = 6",
    "INSERT INTO t VALUES (10, 'after-delete')",
]

READS = [
    "SELECT * FROM t ORDER BY id",
    "SELECT * FROM u ORDER BY a, b",
    "SELECT * FROM emptytab",
    "SELECT count_probe FROM nope",  # error: no_table, stable after restart
    "SELECT id, s FROM t WHERE s IS NULL",
    "SELECT DISTINCT s FROM t",
    "SELECT id FROM t ORDER BY id DESC LIMIT 3",
    "SELECT * FROM t WHERE id = 107",
    "SELECT length(s) AS l FROM t ORDER BY l, id",
    "SELECT a + b AS sum, a - b AS dif FROM u ORDER BY a",
]


def main():
    # Only touch this test's own subdirectories, never sibling stores.
    for sub in ("store1", "frag", "legacy"):
        shutil.rmtree(os.path.join(SCRATCH, sub), ignore_errors=True)
    os.makedirs(os.path.join(SCRATCH, "store1"), exist_ok=True)
    data = os.path.join(SCRATCH, "store1")

    # --- 1. write seeded data, clean stop, restart, byte-identical reads ---
    p = start(PORT, data)
    c = Client()
    got = c.ask(STATEMENTS)
    for i, g in enumerate(got):
        if not (g.startswith("OK ")):
            print("FAIL write statement", i, repr(g))
            sys.exit(1)
    before = c.ask(READS)
    c.close()
    stop(p)

    p = start(PORT, data)
    c = Client()
    after = c.ask(READS)
    check("restart reproduces answers byte for byte", after, before)
    # DML counts still add up after restart: insert into surviving rows.
    cont = c.ask(["INSERT INTO t VALUES (11, 'post-restart')",
                  "SELECT id FROM t ORDER BY id"])
    check("post-restart insert counts", cont[0], "OK 1\n")
    if "11" not in cont[1]:
        print("FAIL post-restart insert visible:", cont[1])
        sys.exit(1)
    print("ok   post-restart insert visible")
    c.close()
    stop(p)

    # --- 2. torn trailing line: dropped, earlier rows survive, later writes
    #        appended after recovery show up on the next restart ---
    log = os.path.join(data, "sdb.log")
    with open(log, "ab") as f:
        f.write(b"INSERT INTO t VALUES (999, 'torn-but-never-terminated")
    p = start(PORT, data)
    c = Client()
    got = c.ask(["SELECT id FROM t ORDER BY id"])
    if "999" in got[0]:
        print("FAIL torn line was applied:", got[0])
        sys.exit(1)
    if "11" not in got[0]:
        print("FAIL earlier rows lost after torn line:", got[0])
        sys.exit(1)
    print("ok   torn line dropped, earlier rows survive")
    c.ask(["INSERT INTO t VALUES (12, 'after-recovery')"])
    c.close()
    stop(p)

    p = start(PORT, data)
    c = Client()
    got = c.ask(["SELECT id FROM t ORDER BY id"])
    # ids: 1,2,3,4 + (-7) + 5->105, 6->106 + 10 + 11 + 12 (999 never applied)
    want_ids = "ROWS 1\nid\n" + "\n".join(
        str(i) for i in
        [-7, 1, 2, 3, 4, 10, 11, 12, 105, 106]
    ) + "\nEND 10\n"
    check("row 12 appended after recovery survives next restart", got[0], want_ids)
    c.close()
    stop(p)

    # --- 2b. fresh dir whose log holds ONLY a torn fragment (no newline) ---
    frag_dir = os.path.join(SCRATCH, "frag")
    os.makedirs(frag_dir)
    with open(os.path.join(frag_dir, "sdb.log"), "wb") as f:
        f.write(b"INSERT INTO ghost VALUES (1, 'never-")
    p = start(PORT, frag_dir)
    c = Client()
    got = c.ask(["SELECT * FROM ghost", "CREATE TABLE real (id INTEGER)",
                 "INSERT INTO real VALUES (7)"])
    if not got[0].startswith("ERR no_table"):
        print("FAIL ghost table materialized from fragment:", got[0])
        sys.exit(1)
    print("ok   torn-only log ignored, fresh writes work")
    c.close()
    stop(p)
    p = start(PORT, frag_dir)
    c = Client()
    check("fresh writes after torn-only log survive restart",
          c.ask(["SELECT id FROM real"]), ["ROWS 1\nid\n7\nEND 1\n"])
    c.close()
    stop(p)

    # --- 3. legacy store: written by the previous binary, read by this one ---
    if os.path.isfile(OLD):
        legacy = os.path.join(SCRATCH, "legacy")
        env = dict(os.environ)
        proc = subprocess.Popen(
            [OLD, "--port", str(PORT + 1), "--data", legacy],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        time.sleep(0.3)
        try:
            lc = Client(PORT + 1)
            lc.ask(STATEMENTS[:8])
            legacy_before = lc.ask(READS)
            lc.close()
        finally:
            proc.send_signal(signal.SIGTERM)
            proc.wait(timeout=10)
        p = start(PORT, legacy)
        c = Client()
        legacy_after = c.ask(READS)
        check("legacy store readable byte for byte", legacy_after, legacy_before)
        c.close()
        stop(p)
    else:
        print("skip legacy test (old binary missing)")

    print("ALL PERSISTENCE TESTS PASSED")


if __name__ == "__main__":
    main()
