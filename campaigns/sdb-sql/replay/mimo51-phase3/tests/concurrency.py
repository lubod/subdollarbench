#!/usr/bin/env python3
"""8 concurrent clients must see each other's committed rows."""
import os
import socket
import sys
import threading

ADDR = ("127.0.0.1", 5580)
TBL = "cc" + os.urandom(3).hex()


def client(idx, results):
    try:
        s = socket.create_connection(ADDR, timeout=15)
        f = s.makefile("rb")

        def ask(stmt):
            s.sendall((stmt + "\n").encode())
            return f.readline().decode()

        ask("CREATE TABLE %s (id INTEGER, who INTEGER)" % TBL)
        ask("INSERT INTO %s (id, who) VALUES (%d, %d)" % (TBL, idx, idx))
        # everyone sees all 8 rows committed by the others
        for _ in range(40):
            s.sendall(("SELECT who FROM %s ORDER BY who\n" % TBL).encode())
            hdr = f.readline().decode()
            if not hdr.startswith("ROWS"):
                results[idx] = "bad frame %r" % hdr
                return
            f.readline()
            rows = []
            while True:
                line = f.readline().decode()
                if line.startswith("END "):
                    break
                rows.append(line.strip())
            if len(rows) == 8:
                results[idx] = "ok"
                return
        results[idx] = "only saw %s" % rows
    except Exception as e:  # noqa: BLE001
        results[idx] = "exc %s" % e


def main():
    results = {}
    threads = [threading.Thread(target=client, args=(i, results)) for i in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    bad = {k: v for k, v in results.items() if v != "ok"}
    if bad:
        print("FAIL", bad)
        sys.exit(1)
    print("PASS (8 concurrent clients)")


if __name__ == "__main__":
    main()
