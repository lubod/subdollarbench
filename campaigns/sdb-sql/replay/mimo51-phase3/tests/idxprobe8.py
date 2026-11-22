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
  "CREATE TABLE w1 (k INTEGER, v TEXT)",
  "INSERT INTO w1 VALUES (1, 'a')",
  "CREATE INDEX w1d ON w1 (k, k)",          # duplicate column in list
  "CREATE INDEX IF NOT EXISTS w1e ON w1 (k)",# IF NOT EXISTS form?
  "CREATE TABLE w1e (a INTEGER REFERENCES nope(x))",  # index name collides + FK target missing
  "CREATE TABLE w1f (a INTEGER REFERENCES w1(k))",    # FK to non-unique col?
  "CREATE UNIQUE INDEX w1u ON w1 (k)",
  "CREATE TABLE w1g (a INTEGER REFERENCES w1(k))",    # FK to unique-index col
  # partial unique index enforcement scope
  "CREATE TABLE w2 (k INTEGER, v TEXT)",
  "INSERT INTO w2 VALUES (1, 'x'), (2, 'x')",
  "CREATE UNIQUE INDEX w2p ON w2 (v) WHERE k > 5",
  "INSERT INTO w2 VALUES (3, 'x')",          # dup v outside predicate -> ok?
  "INSERT INTO w2 VALUES (9, 'x')",          # dup v inside predicate -> constraint?
  "INSERT INTO w2 VALUES (9, 'y'), (10, 'y')",  # hmm y dup inside
  # ORDER BY with LIMIT + index: still index order?
  "CREATE TABLE w3 (id INTEGER PRIMARY KEY, k INTEGER, v TEXT)",
  "INSERT INTO w3 VALUES (10, 5, 'a'), (3, 5, 'b'), (7, 5, 'c')",
  "CREATE INDEX w3k ON w3 (k)",
  "SELECT v FROM w3 ORDER BY k DESC LIMIT 2", # index reverse, first 2?
  "SELECT v FROM w3 ORDER BY k LIMIT 2",
  # WHERE on indexed col with LIMIT, index order of output? (no ORDER BY)
  "SELECT v FROM w3 WHERE k = 5 LIMIT 2",
]
for st, ans in run(stmts):
    print(repr(st), "=>", repr(ans))
