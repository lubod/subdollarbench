#!/usr/bin/env python3
"""Send SQL lines to a reference/candidate server and print the raw answers."""
import socket
import sys
import time

host = sys.argv[1]
port = int(sys.argv[2])
stmts = sys.stdin.read().split("\n")
if stmts and stmts[-1] == "":
    stmts.pop()

s = socket.create_connection((host, port), timeout=10)
s.settimeout(2.0)
data = "".join(x + "\n" for x in stmts).encode()
s.sendall(data)

out = b""
want = len(stmts)
deadline = time.time() + 8
while time.time() < deadline:
    try:
        chunk = s.recv(65536)
    except socket.timeout:
        break
    if not chunk:
        break
    out += chunk
    done = 0
    for line in out.split(b"\n"):
        if line.startswith(b"OK ") or line.startswith(b"END ") or line.startswith(b"ERR "):
            done += 1
    if done >= want:
        break
s.close()
sys.stdout.write(out.decode("utf-8", "replace"))
