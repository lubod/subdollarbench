#!/usr/bin/env python3
"""Differential harness: run the ticket-10 probe corpus against the local
server and the reference; compare answers (ERR by class token only)."""
import random
import socket
import sys

CAND = ("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 5580)
REF = (sys.argv[2] if len(sys.argv) > 2 else "ref-sql",
       int(sys.argv[3]) if len(sys.argv) > 3 else 5480)


def run(host, port, stmts):
    s = socket.create_connection((host, port), timeout=20)
    f = s.makefile("rwb")
    out = []
    for st in stmts:
        f.write(st.encode() + b"\n")
        f.flush()
        line = f.readline().decode()
        if line.startswith("OK") or line.startswith("ERR"):
            out.append(line.rstrip("\n"))
        elif line.startswith("ROWS"):
            block = [line.rstrip("\n")]
            while True:
                l = f.readline().decode().rstrip("\n")
                block.append(l)
                if l.startswith("END"):
                    break
            out.append("\n".join(block))
        elif line == "":
            out.append("<EOF>")
            break
        else:
            out.append("? " + line)
    s.close()
    # pad if the connection died early
    while len(out) < len(stmts):
        out.append("<MISSING>")
    return out


def norm(a):
    if a.startswith("ERR"):
        parts = a.split(" ", 2)
        return "ERR " + parts[1]
    return a


def main():
    import refprobe10c as c
    import refprobe10d as d
    import refprobe10e as e

    # Each module draws a random suffix at import time, so every harness run
    # creates fresh tables on both servers.
    sets = [
        (c.SETUP, c.P),
        (d.SETUP, d.P),
        (e.SETUP, e.P),
    ]
    fails = 0
    total = 0
    for setup, probes in sets:
        pre_r = run(*REF, setup)
        pre_c = run(*CAND, setup)
        setup_ok = True
        for i, (x, y) in enumerate(zip(pre_r, pre_c)):
            if norm(x) != norm(y):
                print("SETUP DIFF", setup[i])
                print("  ref:", x)
                print("  cnd:", y)
                setup_ok = False
        if not setup_ok:
            fails += 1
            continue
        ans_r = run(*REF, probes)
        ans_c = run(*CAND, probes)
        for st, x, y in zip(probes, ans_r, ans_c):
            total += 1
            if norm(x) != norm(y):
                fails += 1
                print("DIFF:", st)
                print("  ref:", x.replace("\n", " | "))
                print("  cnd:", y.replace("\n", " | "))
    print(f"\n{total - fails}/{total} probes match")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
