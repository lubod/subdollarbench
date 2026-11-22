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
  # two indexes: WHERE on w, ORDER BY k DESC, dup k
  "CREATE TABLE e1 (k INTEGER, w INTEGER, v TEXT)",
  "INSERT INTO e1 VALUES (5,1,'a'), (5,2,'b'), (5,3,'c'), (9,1,'d')",
  "CREATE INDEX e1w ON e1 (w)",
  "SELECT v FROM e1 WHERE w >= 1 ORDER BY k DESC",   # index on w serves WHERE; order on k
  "CREATE INDEX e1k ON e1 (k)",
  "SELECT v FROM e1 WHERE w >= 1 ORDER BY k DESC",   # both indexes now
  "SELECT v FROM e1 WHERE w = 1 ORDER BY k DESC",
  "SELECT v FROM e1 ORDER BY k DESC",                # no WHERE
  "SELECT v FROM e1 ORDER BY k DESC LIMIT 2",
  # index exists but ORDER BY on non-indexed col
  "SELECT v FROM e1 ORDER BY w DESC",                # w indexed
  # ORDER BY expression on indexed col (k+0) with index
  "SELECT v FROM e1 ORDER BY k + 0 DESC",
  # DESC ties on seq non-rowid table, index present (baseline recheck on fresh table)
  "CREATE TABLE e2 (k INTEGER, v TEXT)",
  "INSERT INTO e2 VALUES (5,'a'), (5,'b'), (5,'c')",
  "CREATE INDEX e2k ON e2 (k)",
  "SELECT v FROM e2 ORDER BY k DESC",
  "SELECT v FROM e2 ORDER BY k",
  # ORDER BY k with WHERE that excludes some ties (index + filter)
  "SELECT v FROM e2 WHERE k >= 5 ORDER BY k DESC",
  # GROUP BY with dup keys + ORDER BY agg DESC (index on k exists)
  "SELECT k, COUNT(*) FROM e2 GROUP BY k ORDER BY COUNT(*) DESC",
  # INSERT..SELECT / subquery paths unaffected? just sanity selects
  "SELECT * FROM e1 WHERE k = 5",
]
for st, ans in run(stmts):
    print(repr(st), "=>", repr(ans))
