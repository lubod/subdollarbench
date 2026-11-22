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
  "CREATE TABLE t (k INTEGER, v TEXT)",
  "CREATE INDEX i1 ON t (k)", # i1 exists (from probe1? fresh conn - no)
  "CREATE INDEX t ON nope (k)",        # name is a table, table missing
  "CREATE INDEX i1 ON nope (k)",       # name exists, table missing
  "CREATE TABLE x (k INTEGER UNIQUE, v TEXT)",
  "INSERT INTO x VALUES (1, 'a'), (1, 'b')",
  "CREATE UNIQUE INDEX ux ON x (v)",
  "INSERT INTO x VALUES (2, 'a'), (3, 'a')",
  "CREATE UNIQUE INDEX ux2 ON x (k)",  # k already unique (declared)
  "CREATE UNIQUE INDEX ux3 ON x (v)",  # fresh col with dups? v has 'a' twice now
  "CREATE INDEX",
  "DROP INDEX",
  "CREATE INDEX ix ON x (k, v)",
  "CREATE UNIQUE INDEX ux4 ON x (v)",
  "INSERT INTO x VALUES (9, NULL), (10, NULL)",
  "CREATE UNIQUE INDEX ux5 ON x (v)",
  "SELECT * FROM x",
  "UPDATE x SET v = 'a' WHERE k = 9",
  "UPDATE x SET v = 'zz' WHERE k = 1",
  "SELECT v FROM x WHERE k = 1",
]
for st, ans in run(stmts):
    print(repr(st), "=>", repr(ans))
