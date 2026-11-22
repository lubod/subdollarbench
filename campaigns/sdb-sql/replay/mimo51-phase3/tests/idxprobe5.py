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
  # DESC baseline WITHOUT index: rowid table, out-of-seq rowids, dup keys
  "CREATE TABLE d1 (id INTEGER PRIMARY KEY, k INTEGER, v TEXT)",
  "INSERT INTO d1 VALUES (10, 5, 'a'), (3, 5, 'b'), (7, 5, 'c')",
  "SELECT v FROM d1 ORDER BY k DESC",
  "SELECT v FROM d1 ORDER BY k ASC",
  "CREATE INDEX d1k ON d1 (k)",
  "SELECT v FROM d1 ORDER BY k DESC",         # with index
  "SELECT v FROM d1 ORDER BY k ASC",          # with index
  # non-rowid table (implicit rowid = insertion order), dup keys
  "CREATE TABLE d2 (k INTEGER, v TEXT)",
  "INSERT INTO d2 VALUES (5, 'a'), (5, 'b'), (5, 'c')",
  "SELECT v FROM d2 ORDER BY k DESC",         # no index
  "SELECT v FROM d2 ORDER BY k ASC",
  "CREATE INDEX d2k ON d2 (k)",
  "SELECT v FROM d2 ORDER BY k DESC",         # with index
  "SELECT v FROM d2 ORDER BY k ASC",
  # multi-term ties with index on first term
  "SELECT v FROM d1 ORDER BY k ASC, id ASC",  # index on k exists
  "SELECT v FROM d1 ORDER BY k ASC, id DESC",
  # ORDER BY id (rowid col) with index on k present
  "SELECT v FROM d1 ORDER BY id ASC",
  # NULLs placement ASC/DESC
  "CREATE TABLE d3 (k INTEGER, v TEXT)",
  "INSERT INTO d3 VALUES (NULL, 'n1'), (1, 'x'), (NULL, 'n2'), (2, 'y')",
  "SELECT v FROM d3 ORDER BY k",
  "SELECT v FROM d3 ORDER BY k DESC",
  "CREATE INDEX d3k ON d3 (k)",
  "SELECT v FROM d3 ORDER BY k",
  "SELECT v FROM d3 ORDER BY k DESC",
  # explicit NULLS FIRST/LAST
  "SELECT v FROM d3 ORDER BY k NULLS LAST",
  "SELECT v FROM d3 ORDER BY k DESC NULLS FIRST",
]
for st, ans in run(stmts):
    print(repr(st), "=>", repr(ans))
