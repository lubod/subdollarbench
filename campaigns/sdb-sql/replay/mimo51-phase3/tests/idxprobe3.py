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
  # fresh names
  "CREATE TABLE p3 (a INTEGER, b TEXT)",
  "INSERT INTO p3 VALUES (1, 'x'), (1, 'y')",       # dup on a
  "CREATE UNIQUE INDEX u3 ON p3 (a)",               # over dups -> constraint?
  "CREATE UNIQUE INDEX u3 ON p3 (a)",               # (if failed, retry)
  "CREATE INDEX u3 ON nope (a)",                    # name taken + no table
  "CREATE INDEX u3 ON p3 (nope)",                   # name taken + no column
  "CREATE UNIQUE INDEX u3 ON p3 (b)",               # name taken + would-pass data
  # FK to unique-index column
  "CREATE TABLE p4 (a INTEGER)",
  "INSERT INTO p4 VALUES (7)",
  "CREATE UNIQUE INDEX p4u ON p4 (a)",
  "CREATE TABLE c4 (id INTEGER, pa INTEGER REFERENCES p4(a))",
  # partial index?
  "CREATE INDEX pidx ON p3 (b) WHERE b > 'a'",
  # empty column list / junk
  "CREATE INDEX e1 ON p3 ()",
  "CREATE INDEX e2 ON p3 (a) extra",
  "DROP INDEX u3 extra",
  # name is table, column missing (order of checks)
  "CREATE INDEX p3 ON p3 (nope)",
  # index name equals table name where both exist? p3 is table; index named c4?
  "CREATE INDEX c4 ON p3 (a)",
  # create table named like an existing index
  "CREATE TABLE u3 (q INTEGER)",
  # unique index + update violation
  "CREATE TABLE p5 (k INTEGER, v TEXT)",
  "INSERT INTO p5 VALUES (1, 'a'), (2, 'b')",
  "CREATE UNIQUE INDEX p5u ON p5 (v)",
  "UPDATE p5 SET v = 'b' WHERE k = 1",
  "UPDATE p5 SET v = 'b' WHERE k = 2",   # same value, no violation
  "SELECT * FROM p5",
  # case/whitespace variants
  "create   unique\tindex   u6   on   p5   (   k   )",
  "DROP   INDEX   u6",
]
for st, ans in run(stmts):
    print(repr(st), "=>", repr(ans))
