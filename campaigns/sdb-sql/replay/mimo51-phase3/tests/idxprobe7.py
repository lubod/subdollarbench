import socket

def run(stmts, port=5480, host='ref-sql'):
    s = socket.create_connection((host, port), timeout=10)
    out = []
    for st in stmts:
        s.sendall((st + "\n").encode())
        buf = b""
        while True:
            chunk = s.recv(65536)
            if not chunk: break
            buf += chunk
            if buf.startswith(b"ROWS"):
                if b"\nEND " in buf:
                    break
                continue
            break
        out.append((st, buf.decode(errors='replace').strip()))
    s.close()
    return out

stmts = [
  # check-order probes with a definitely existing index name "zq"
  "CREATE TABLE z1 (k INTEGER, v TEXT)",
  "INSERT INTO z1 VALUES (1, 'a'), (1, 'b')",       # dup k
  "CREATE INDEX zq ON z1 (k)",                      # zq exists now
  "CREATE INDEX zq ON nope (k)",                    # exists + no_table  -> ?
  "CREATE INDEX zq ON z1 (nope)",                   # exists + no_column -> ?
  "CREATE UNIQUE INDEX zq ON z1 (k)",               # exists + constraint -> ?
  "CREATE TABLE z2 (k INTEGER, v TEXT)",
  "INSERT INTO z2 VALUES (1, 'x'), (1, 'y')",
  "CREATE UNIQUE INDEX zq2 ON z2 (k)",              # fresh name + dups -> constraint
  "CREATE UNIQUE INDEX zq2 ON z2 (k)",              # failed -> not registered, again constraint
  # alias / qualified ORDER BY with index
  "CREATE TABLE z3 (k INTEGER, v TEXT)",
  "INSERT INTO z3 VALUES (5, 'a'), (5, 'b'), (5, 'c')",
  "CREATE INDEX z3k ON z3 (k)",
  "SELECT v, k AS kk FROM z3 ORDER BY kk DESC",     # alias: reversed?
  "SELECT v FROM z3 ORDER BY z3.k DESC",            # qualified: reversed?
  "SELECT v FROM z3 ORDER BY v, k DESC",            # multi-term (v distinct) 
  "SELECT v FROM z3 ORDER BY k DESC, v ASC",        # multi-term first on k
  # ORDER BY on indexed col but query has GROUP BY (no ties possible here) sanity
  # UNIQUE index DESC ties (same as plain?)
  "CREATE UNIQUE INDEX z3u ON z3 (k)",             # z3 has dup k -> constraint expected
  "CREATE TABLE z4 (k INTEGER, v TEXT)",
  "INSERT INTO z4 VALUES (5, 'a'), (5, 'b')",
  "CREATE UNIQUE INDEX z4k ON z4 (k)",
  "SELECT v FROM z4 ORDER BY k DESC",               # can't dup with unique... skip idea
  # index name case: names are lowercased?
  "CREATE INDEX ZQ9 ON z3 (v)",
  "DROP INDEX zq9",
  "DROP INDEX ZQ9",
]
for st, ans in run(stmts):
    print(repr(st), "=>", repr(ans))
