#!/usr/bin/env python3
"""Probe the reference's join grammar surface (W3 question, ticket 7)."""
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

sfx = "gw1"
setup = [
    "CREATE TABLE %s (a INTEGER, b TEXT)" % sfx,
    "INSERT INTO %s VALUES (1, 'x'), (2, 'y'), (1, 'z')" % sfx,
    "CREATE TABLE %sy (a INTEGER, c INTEGER)" % sfx,
    "INSERT INTO %sy VALUES (1, 10), (2, 20), (3, 30)" % sfx,
    "CREATE TABLE %sz (k INTEGER, d TEXT)" % sfx,
    "INSERT INTO %sz VALUES (1, 'p'), (1, 'q'), (2, 'r')" % sfx,
]

probes = [
    # aliases: AS form and bare form
    "SELECT p.a, q.c FROM %s AS p, %sy AS q WHERE p.a = q.a ORDER BY p.a, q.c" % (sfx, sfx),
    "SELECT p.a, q.c FROM %s p, %sy q WHERE p.a = q.a ORDER BY p.a, q.c" % (sfx, sfx),
    # join variants
    "SELECT COUNT(*) FROM %s INNER JOIN %sy ON %s.a = %sy.a" % (sfx, sfx, sfx, sfx),
    "SELECT COUNT(*) FROM %s LEFT JOIN %sy ON %s.a = %sy.a" % (sfx, sfx, sfx, sfx),
    "SELECT COUNT(*) FROM %s LEFT OUTER JOIN %sy ON %s.a = %sy.a" % (sfx, sfx, sfx, sfx),
    "SELECT COUNT(*) FROM %s CROSS JOIN %sy" % (sfx, sfx),
    "SELECT COUNT(*) FROM %s JOIN %sy" % (sfx, sfx),
    "SELECT COUNT(*) FROM %s JOIN %sy USING (a)" % (sfx, sfx),
    "SELECT COUNT(*) FROM %s, %sy, %sz" % (sfx, sfx, sfx),
    "SELECT COUNT(*) FROM %s JOIN %sy ON %s.a = %sy.a JOIN %sz ON %s.a = %sz.k" % (sfx, sfx, sfx, sfx, sfx, sfx, sfx),
    # qualified star and headers
    "SELECT %s.b, %sy.c FROM %s, %sy WHERE %s.a = %sy.a ORDER BY %s.b, %sy.c" % (sfx, sfx, sfx, sfx, sfx, sfx, sfx, sfx),
    "SELECT %s.* FROM %s, %sy WHERE %s.a = %sy.a" % (sfx, sfx, sfx, sfx, sfx),
    # error surfaces
    "SELECT zz FROM %s, %sy" % (sfx, sfx),
    "SELECT %s.zz FROM %s, %sy" % (sfx, sfx, sfx),
    "SELECT %sx.a FROM %s" % (sfx, sfx),
    "SELECT a FROM %s, %sy" % (sfx, sfx),
    "SELECT a FROM %s, %sy WHERE %s.a = %sy.a GROUP BY a" % (sfx, sfx, sfx, sfx),
    "SELECT %s.a, COUNT(*) FROM %s, %sy WHERE %s.a = %sy.a GROUP BY %s.a ORDER BY %s.a" % (sfx, sfx, sfx, sfx, sfx, sfx, sfx),
    "SELECT COUNT(*) FROM %s, %sy WHERE a = 1" % (sfx, sfx),
    "SELECT b FROM %s, %sy" % (sfx, sfx),
    # where referencing one side only; group by qualified with count
    "SELECT %sy.c, COUNT(*) FROM %s, %sz, %sy WHERE %s.a = %sz.k AND %sy.a = 1 GROUP BY %sy.c ORDER BY %sy.c" % (sfx, sfx, sfx, sfx, sfx, sfx, sfx, sfx, sfx),
    # scalar aggregate over join; empty join input
    "SELECT COUNT(*), SUM(%sy.c) FROM %s, %sy WHERE %s.a = %sy.a" % (sfx, sfx, sfx, sfx, sfx),
    "SELECT COUNT(*) FROM %s, %sy WHERE %s.a = %sy.a AND %s.a > 100" % (sfx, sfx, sfx, sfx, sfx),
    # order by qualified not in select
    "SELECT %s.a FROM %s, %sy WHERE %s.a = %sy.a ORDER BY %sy.c" % (sfx, sfx, sfx, sfx, sfx, sfx),
    # having on join
    "SELECT %s.a AS k, COUNT(*) AS n FROM %s, %sy WHERE %s.a = %sy.a GROUP BY %s.a HAVING COUNT(*) > 2 ORDER BY k" % (sfx, sfx, sfx, sfx, sfx, sfx),
]

print("\n".join(run(setup + probes)))
