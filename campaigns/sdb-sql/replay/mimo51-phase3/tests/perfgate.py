#!/usr/bin/env python3
"""Ticket-11 index gate rehearsal: two identical 200k-row tables, an index
on one, alternating point lookups and range+ORDER BY queries. The indexed
median must be under 5 ms and at least 10x faster than the unindexed one.

usage: perfgate.py [port] [rows]
"""
import socket
import sys
import time
import statistics

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 5580
ROWS = int(sys.argv[2]) if len(sys.argv) > 2 else 200000


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=60)
        self.f = self.s.makefile("rwb")

    def ask(self, st):
        self.f.write(st.encode() + b"\n")
        self.f.flush()
        line = self.f.readline().decode()
        if line.startswith("ROWS"):
            while True:
                l = self.f.readline().decode()
                if l.startswith("END"):
                    break
            return "ROWS"
        return line.strip()

    def timed(self, st):
        t0 = time.perf_counter()
        r = self.ask(st)
        return (time.perf_counter() - t0) * 1000.0, r


def median(xs):
    return statistics.median(xs)


def fill(c, name):
    c.ask(f"DROP TABLE IF EXISTS {name}") if False else None
    c.ask(f"CREATE TABLE {name} (k INTEGER, v TEXT)")
    # batched inserts, 1000 rows per statement
    batch = []
    for i in range(1, ROWS + 1):
        batch.append(f"({i}, 'val{i % 977}')")
        if len(batch) == 1000:
            c.ask(f"INSERT INTO {name} VALUES {', '.join(batch)}")
            batch = []
    if batch:
        c.ask(f"INSERT INTO {name} VALUES {', '.join(batch)}")


def gate(c, label):
    ok = True
    # --- point lookups ---------------------------------------------------
    pu, pi = [], []
    for i in range(500):
        k = (i * 7919) % ROWS + 1
        t, r = c.timed(f"SELECT v FROM t_un WHERE k = {k}")
        pu.append(t)
        assert r == "ROWS", r
        t, r = c.timed(f"SELECT v FROM t_in WHERE k = {k}")
        pi.append(t)
        assert r == "ROWS", r
    mu, mi = median(pu), median(pi)
    ratio = mu / mi if mi > 0 else float("inf")
    p1 = mi < 5.0 and ratio >= 10
    ok &= p1
    print(f"[{label}] point   unindexed p50={mu:.3f} ms  indexed p50={mi:.3f} ms  "
          f"ratio={ratio:.1f}x  {'PASS' if p1 else 'FAIL'}")

    # --- range + ORDER BY ------------------------------------------------
    for win in (10, 1000, 20000):
        ru, ri = [], []
        for i in range(100):
            a = (i * 104729) % (ROWS - win)
            for tbl, acc in (("t_un", ru), ("t_in", ri)):
                t, r = c.timed(
                    f"SELECT v FROM {tbl} WHERE k >= {a} AND k < {a + win} ORDER BY k"
                )
                acc.append(t)
                assert r == "ROWS", r
        mu, mi = median(ru), median(ri)
        ratio = mu / mi if mi > 0 else float("inf")
        p = mi < 5.0 and ratio >= 10
        ok &= p
        print(f"[{label}] range{win:<6} unindexed p50={mu:.3f} ms  indexed p50={mi:.3f} ms  "
              f"ratio={ratio:.1f}x  {'PASS' if p else 'FAIL'}")

    # --- range + ORDER BY + LIMIT ---------------------------------------
    lu, li = [], []
    for i in range(100):
        a = (i * 104729) % (ROWS - 1000)
        for tbl, acc in (("t_un", lu), ("t_in", li)):
            t, r = c.timed(
                f"SELECT v FROM {tbl} WHERE k >= {a} AND k < {a + 1000} ORDER BY k LIMIT 50"
            )
            acc.append(t)
            assert r == "ROWS", r
    mu, mi = median(lu), median(li)
    ratio = mu / mi if mi > 0 else float("inf")
    p = mi < 5.0 and ratio >= 10
    ok &= p
    print(f"[{label}] ranglim  unindexed p50={mu:.3f} ms  indexed p50={mi:.3f} ms  "
          f"ratio={ratio:.1f}x  {'PASS' if p else 'FAIL'}")
    return ok


def main():
    c = Conn(PORT)
    print("filling...")
    t0 = time.perf_counter()
    fill(c, "t_un")
    fill(c, "t_in")
    print(f"filled in {time.perf_counter() - t0:.1f} s")
    c.ask("CREATE INDEX t_in_k ON t_in (k)")
    ok = gate(c, "run1")
    # restart is done by the caller; just report
    print("GATE", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
