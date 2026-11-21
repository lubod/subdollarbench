#!/usr/bin/env python3
"""Probe: does the reference dialect support joins at all (W3 question)?"""
import socket, sys

HOST = sys.argv[1] if len(sys.argv) > 1 else "ref-sql"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 5480

def run(stmts):
    s = socket.create_connection((HOST, PORT), timeout=10)
    f = s.makefile("rwb")
    out = []
    for st in stmts:
        f.write(st.encode() + b"\n")
        f.flush()
        while True:
            line = f.readline()
            if not line:
                out.append("<EOF>")
                break
            l = line.decode().rstrip("\n")
            if l.startswith("ROWS"):
                out.append(l)
                while True:
                    nxt = f.readline().decode().rstrip("\n")
                    out.append(nxt)
                    if nxt.startswith("END") or not nxt:
                        break
                break
            out.append(l)
            if l.startswith(("OK", "ERR", "END")):
                break
    s.close()
    return out

suffix = "jx1"
setup = [
    "CREATE TABLE %s (a INTEGER, b TEXT)" % suffix,
    "INSERT INTO %s VALUES (1, 'x'), (2, 'y'), (1, 'z')" % suffix,
    "CREATE TABLE %sy (a INTEGER, c INTEGER)" % suffix,
    "INSERT INTO %sy VALUES (1, 10), (2, 20), (3, 30)" % suffix,
]

probes = [
    "SELECT COUNT(*) FROM %s, %sy" % (suffix, suffix),
    "SELECT %s.a, %sy.c FROM %s, %sy WHERE %s.a = %sy.a ORDER BY %s.a, %sy.c" % (suffix, suffix, suffix, suffix, suffix, suffix, suffix, suffix),
    "SELECT a, COUNT(*) FROM %s, %sy WHERE %s.a = %sy.a GROUP BY a ORDER BY a" % (suffix, suffix, suffix, suffix),
    "SELECT * FROM %s JOIN %sy ON %s.a = %sy.a" % (suffix, suffix, suffix, suffix),
    "SELECT * FROM %s, %sy" % (suffix, suffix),
    "SELECT COUNT(*) FROM %s, %sy GROUP BY %s.a" % (suffix, suffix, suffix),
]

print("\n".join(run(setup + probes)))
