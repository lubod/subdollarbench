#!/usr/bin/env python3
"""Ticket 4 hardening tests.

A. SIGTERM racing active writes: after the stop, the store must contain
   exactly a prefix of the pipelined inserts -- at least every OK the client
   actually read, and no row that was never sent.
B. Large store: 100k single-row inserts, restart, measure replay time.
"""
import os, shutil, signal, socket, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRV = os.path.join(ROOT, "target", "release", "sdb")
PORT = 5595
DATA = "/tmp/sdbtest/stress"


def start():
    p = subprocess.Popen([SRV, "--port", str(PORT), "--data", DATA],
                         stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    t0 = time.time()
    for _ in range(3000):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=2)
            return p, time.time() - t0, s
        except OSError:
            if p.poll() is not None:
                raise RuntimeError((p.stderr.read() or b"").decode())
            time.sleep(0.01)
    raise RuntimeError("server never came up")


def stop(p, sig=signal.SIGTERM):
    p.send_signal(sig)
    p.wait(timeout=15)


def read_lines(s, buf, n_lines):
    """Read exactly n_lines newline-terminated lines; return (lines, buf)."""
    lines = []
    while len(lines) < n_lines:
        if b"\n" in buf:
            first, buf = buf.split(b"\n", 1)
            lines.append(first.decode())
        else:
            chunk = s.recv(65536)
            if not chunk:
                raise RuntimeError("connection closed early")
            buf += chunk
    return lines, buf


def main():
    shutil.rmtree(DATA, ignore_errors=True)
    os.makedirs(DATA)

    # --- A. SIGTERM races in-flight acknowledged writes --------------------
    p, _, s = start()
    buf = b""
    s.sendall(b"CREATE TABLE stress (id INTEGER, v TEXT)\n")
    lines, buf = read_lines(s, buf, 1)
    assert lines[0] == "OK 0", lines
    s.sendall("".join(
        "INSERT INTO stress VALUES (%d, 'row-%d-xxxxxxxxxxxxxxxx')\n" % (i, i)
        for i in range(2000)
    ).encode())
    # Read only part of the answers, then stop immediately: every OK we DID
    # read must be durable; anything beyond is racy but must be a clean prefix.
    acked = 0
    while acked < 500:
        lines, buf = read_lines(s, buf, 1)
        assert lines[0] == "OK 1", lines
        acked += 1
    stop(p)

    p, up_t, s = start()
    buf = b""
    s.sendall(b"SELECT id FROM stress ORDER BY id\n")
    resp = b""
    while b"\nEND " not in resp:
        chunk = s.recv(65536)
        if not chunk:
            raise RuntimeError("closed during read")
        resp += chunk
    s.close()
    stop(p)
    rows = resp.decode().splitlines()
    n = int(rows[0].split()[1])
    ids = [int(x) for x in rows[2:-1]]
    m = len(ids)
    if ids != list(range(m)):
        print("FAIL not an exact prefix of the sent inserts:", ids[:20], "...")
        sys.exit(1)
    if m < acked:
        print("FAIL lost acknowledged rows: acked %d, stored %d" % (acked, m))
        sys.exit(1)
    if m > 2000:
        print("FAIL phantom rows:", m)
        sys.exit(1)
    print("ok   mid-write SIGTERM: stored prefix %d >= acked %d <= sent 2000"
          % (m, acked))

    # --- B. large store replay timing --------------------------------------
    p, _, s = start()
    buf = b""
    s.sendall(b"CREATE TABLE big (id INTEGER, v TEXT)\n")
    lines, buf = read_lines(s, buf, 1)
    assert lines[0] == "OK 0", lines
    CHUNK = 500
    total = 100000
    t0 = time.time()
    for base in range(0, total, CHUNK):
        s.sendall("".join(
            "INSERT INTO big VALUES (%d, 'value-%d-aaaaaaaaaa')\n" % (i, i)
            for i in range(base, base + CHUNK)
        ).encode())
        lines, buf = read_lines(s, buf, CHUNK)
        assert all(l == "OK 1" for l in lines), lines[:3]
    seed_t = time.time() - t0
    s.close()
    stop(p)
    logsz = os.path.getsize(os.path.join(DATA, "sdb.log"))
    p, up_t, s = start()
    t1 = time.time()
    buf = b""
    s.sendall(b"SELECT id, v FROM big WHERE id = 99999\n")
    resp = b""
    while b"\nEND " not in resp:
        resp += s.recv(65536)
    answer_t = time.time() - t1
    if b"99999" not in resp or b"value-99999-aaaaaaaaaa" not in resp:
        print("FAIL large store replay:", resp[:300])
        sys.exit(1)
    print("ok   100k-row store: seeded %.1fs, log %.1f MB, restart %.2fs, "
          "first answer %.2fs" % (seed_t, logsz / 1e6, up_t, answer_t))
    s.close()
    stop(p)
    print("STRESS TESTS PASSED")


if __name__ == "__main__":
    main()
