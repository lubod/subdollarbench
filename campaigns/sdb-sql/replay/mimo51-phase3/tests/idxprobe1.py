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
  "CREATE INDEX i1 ON t (k)",
  "CREATE INDEX i1 ON t (k)",
  "CREATE INDEX i1 ON t (v)",
  "CREATE TABLE i1 (a INTEGER)",
  "CREATE INDEX t ON t (k)",
  "CREATE INDEX i2 ON nope (k)",
  "CREATE INDEX i3 ON t (nope)",
  "DROP INDEX nope",
  "DROP INDEX t",
  "DROP INDEX i1",
  "DROP INDEX i1",
]
for st, ans in run(stmts):
    print(repr(st), "=>", repr(ans))
