#!/usr/bin/env python3
"""Replay the statement sequence dumped by fuzz9 on mismatch (/tmp/
fz9_sent.txt), renaming the table prefix so both servers see fresh tables."""
import re
import socket
import sys

CAND = ("127.0.0.1", 5580)
REF = ("ref-sql", 5480)


class Conn:
    def __init__(self, addr):
        self.sock = socket.create_connection(addr, timeout=30)
        self.file = self.sock.makefile("rb")

    def ask(self, stmt):
        self.sock.sendall((stmt + "\n").encode())
        first = self.file.readline().decode()
        if first.startswith("ROWS "):
            hdr = self.file.readline().decode().rstrip("\n")
            rows = []
            while True:
                line = self.file.readline().decode()
                if line.startswith("END "):
                    n = int(line.split()[1])
                    break
                rows.append(line.rstrip("\n"))
            return ("ROWS", hdr, rows, n)
        if first.startswith("ERR "):
            return ("ERR", first.split()[1], first.rstrip("\n"))
        return ("OK", first.rstrip("\n"))


def main():
    with open("/tmp/fz9_sent.txt") as fh:
        stmts = [l.rstrip("\n") for l in fh if l.strip()]
    m = re.search(r"fz9_[0-9a-f]+_", "\n".join(stmts))
    if not m:
        print("no prefix found")
        return 1
    old = m.group(0)
    new = "re_%s_" % old[4:10]
    stmts = [s.replace(old, new) for s in stmts]
    cand = Conn(CAND)
    ref = Conn(REF)
    for i, s in enumerate(stmts):
        c = cand.ask(s)
        r = ref.ask(s)
        same = c[0] == r[0] and (c[0] != "ERR" or c[1] == r[1]) \
            and (c[0] != "OK" or c[1] == r[1])
        if not same:
            print("DIVERGES at #%d\n  stmt: %s\n  cand: %r\n  ref : %r" % (i, s, c, r))
            return 1
        print("#%-3d %-70s => %r" % (i, s[:70], c))
    print("no divergence on replay")
    return 0


if __name__ == "__main__":
    sys.exit(main())
