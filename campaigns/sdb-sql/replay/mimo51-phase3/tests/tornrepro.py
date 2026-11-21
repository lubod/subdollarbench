#!/usr/bin/env python3
"""Confirm the torn-tail cursor bug exists in the OLD binary and is fixed in
the NEW one: seed -> torn tail -> restart+insert -> restart -> row must exist."""
import os, signal, socket, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

def ask(port, stmts, wait_up=True):
    for _ in range(60):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=5)
            break
        except OSError:
            time.sleep(0.05)
    else:
        raise RuntimeError("server not up")
    s.sendall("".join(x + "\n" for x in stmts).encode())
    buf = b""
    out = []
    for _ in stmts:
        while b"\n" not in buf:
            buf += s.recv(65536)
        line, buf = buf.split(b"\n", 1)
        if line.startswith(b"ROWS "):
            while True:
                while b"\n" not in buf:
                    buf += s.recv(65536)
                l, buf = buf.split(b"\n", 1)
                line += b"\n" + l
                if l.startswith(b"END "):
                    break
        out.append(line.decode())
    s.close()
    return out

def run(binary, port, data):
    p = subprocess.Popen([binary, "--port", str(port), "--data", data],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(0.3)
    if p.poll() is not None:
        raise RuntimeError("died")
    return p

def stop(p):
    p.send_signal(signal.SIGTERM)
    p.wait(timeout=10)

def scenario(binary, tag, port):
    data = f"/tmp/sdbtest/bug_{tag}"
    subprocess.run(["rm", "-rf", data])
    p = run(binary, port, data)
    ask(port, ["CREATE TABLE t (id INTEGER, s TEXT)",
               "INSERT INTO t VALUES (1, 'a')"])
    stop(p)
    # torn tail (write interrupted before the newline)
    with open(os.path.join(data, "sdb.log"), "ab") as f:
        f.write(b"INSERT INTO t VALUES (99, 'torn")
    p = run(binary, port, data)
    ask(port, ["INSERT INTO t VALUES (3, 'c')"])
    stop(p)
    p = run(binary, port, data)
    got = ask(port, ["SELECT id FROM t ORDER BY id"])[0]
    stop(p)
    expect = "ROWS 1\nid\n1\n3\nEND 2"
    status = "OK" if got == expect else "DATA LOSS"
    print(f"{tag:4} rows -> {got!r}  [{status}]")
    return got == expect

old_bin = os.path.join(ROOT, ".tmpwork", "sdb_old")
if not os.path.isfile(old_bin):
    print("skip old-binary comparison (sdb_old missing)")
    sys.exit(0 if scenario(os.path.join(ROOT, "target", "release", "sdb"),
                           "new", 5594) else 1)
old_ok = scenario(old_bin, "old", 5593)
new_ok = scenario(os.path.join(ROOT, "target", "release", "sdb"), "new", 5594)
print("old binary exhibits the bug:", not old_ok)
print("new binary fixed:", new_ok)
sys.exit(0 if new_ok else 1)
