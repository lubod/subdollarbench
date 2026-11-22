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
  "CREATE TABLE q1 (k INTEGER, v TEXT)",
  "INSERT INTO q1 VALUES (1, 'a'), (2, 'b')",
  "CREATE INDEX qx ON q1 (k)",                    # qx definitely exists now
  "CREATE INDEX qx ON nope (k)",                  # name taken + no table
  "CREATE INDEX qx ON q1 (nope)",                 # name taken + no column
  "CREATE TABLE q2 (k INTEGER, v TEXT)",
  "INSERT INTO q2 VALUES (1, 'x'), (1, 'y')",     # dups on k
  "CREATE UNIQUE INDEX q2u ON q2 (k)",            # q2u fresh, dups -> constraint
  "CREATE UNIQUE INDEX qx ON q2 (k)",             # qx exists + dups: exists or constraint?
  "CREATE UNIQUE INDEX qx ON q2 (v)",             # qx exists + data fine: exists
  # ORDER BY tie order with/without index
  "CREATE TABLE tb (id INTEGER PRIMARY KEY, k INTEGER, v TEXT)",
  "INSERT INTO tb VALUES (10, 5, 'a'), (3, 5, 'b'), (7, 5, 'c')",
  "SELECT v FROM tb ORDER BY k",
  "CREATE INDEX tbk ON tb (k)",
  "SELECT v FROM tb ORDER BY k",
  "SELECT v FROM tb ORDER BY k DESC",
  "DROP INDEX tbk",
  "SELECT v FROM tb ORDER BY k",
  # unique index tuple enforcement (multi-col)
  "CREATE TABLE mq (a INTEGER, b TEXT)",
  "INSERT INTO mq VALUES (1, 'x'), (1, 'y')",
  "CREATE UNIQUE INDEX mqu ON mq (a, b)",
  "INSERT INTO mq VALUES (1, 'x')",               # tuple dup -> constraint?
  "INSERT INTO mq VALUES (1, 'z')",               # tuple new -> ok
  # index on TEXT col range
  "SELECT * FROM q1 WHERE k >= 1 AND k < 2",
]
for st, ans in run(stmts):
    print(repr(st), "=>", repr(ans))
