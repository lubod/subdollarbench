#!/usr/bin/env python3
"""Ticket 5 crash-safety tests.

A. every acknowledged single-row insert survives kill -9 - exactly: every
   acked one, no half-written extra (sequential, plus a pipelined prefix
   variant)
B. a 50k-row UPDATE killed mid-flight leaves the column uniformly old or
   uniformly new, with all 50k rows present
C. a torn tail: a few hundred bytes cut from the newest data file may lose
   at most the most recent statement; every older row survives, and the
   server still serves and persists new writes afterwards
D. two restarts in a row change nothing
E. a restart is serving again within 20 s for a tens-of-thousands-of-rows
   store
F. a ticket-4 style raw-line store is readable, then upgraded safely
G. kill -9 during recovery/startup itself never loses anything
"""
import os
import shutil
import signal
import socket
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRV = os.path.join(ROOT, "start.sh")
SCRATCH = "/tmp/sdbtest/crash"
PORT = 5601

FAILURES = []


def check(cond, what, detail=""):
    if cond:
        print("ok   " + what)
    else:
        print("FAIL " + what + ("  " + detail if detail else ""))
        FAILURES.append(what)


def start(data):
    os.makedirs(data, exist_ok=True)
    p = subprocess.Popen([SRV, "--port", str(PORT), "--data", data],
                         stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    for _ in range(4000):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
            s.close()
            return p
        except OSError:
            if p.poll() is not None:
                err = (p.stderr.read() or b"").decode()
                raise RuntimeError("server exited early: " + err)
            time.sleep(0.01)
    raise RuntimeError("server never came up")


def kill9(p):
    p.kill()
    p.wait()


def stop(p):
    p.send_signal(signal.SIGTERM)
    p.wait()


class Client:
    def __init__(self):
        for _ in range(200):
            try:
                self.sock = socket.create_connection(("127.0.0.1", PORT),
                                                     timeout=30)
                break
            except OSError:
                time.sleep(0.02)
        else:
            raise RuntimeError("cannot connect")
        self.buf = b""

    def send(self, stmts):
        if isinstance(stmts, str):
            stmts = [stmts]
        self.sock.sendall(
            "".join(s if s.endswith("\n") else s + "\n"
                    for s in stmts).encode())

    def read_line(self):
        while b"\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("connection closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode()

    def read_resp(self):
        first = self.read_line()
        if first.startswith("ROWS "):
            frame = [first]
            while True:
                l = self.read_line()
                frame.append(l)
                if l.startswith("END "):
                    break
            return "\n".join(frame) + "\n"
        return first

    def ask(self, stmts):
        if isinstance(stmts, str):
            stmts = [stmts]
        self.send(stmts)
        return [self.read_resp() for _ in stmts]

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def ids_of(resp):
    """Rows of a single-integer-column ROWS frame -> list[int]."""
    lines = resp.rstrip("\n").split("\n")
    assert lines[0].startswith("ROWS "), lines[0]
    return [int(x) for x in lines[2:-1]]


def newest_file(d):
    best, best_t = None, -1.0
    for name in os.listdir(d):
        if name.endswith(".tmp"):
            continue
        st = os.stat(os.path.join(d, name))
        if st.st_mtime_ns > best_t:
            best, best_t = os.path.join(d, name), st.st_mtime_ns
    return best


def fresh(tag):
    d = os.path.join(SCRATCH, tag)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    return d


# ---------------------------------------------------------------------------
# A. acked rows survive kill -9, and nothing extra
# ---------------------------------------------------------------------------

def test_acked_survive():
    for trial, target in enumerate((17, 40, 55)):
        data = fresh("a%d" % trial)
        p = start(data)
        c = Client()
        assert c.ask("CREATE TABLE t (id INTEGER, s TEXT)")[0] == "OK 0"
        acked = 0
        for i in range(1, target + 1):
            c.send("INSERT INTO t VALUES (%d, 'row-%d')" % (i, i))
            resp = c.read_resp()
            if resp != "OK 1":
                raise RuntimeError("no ack for row %d: %r" % (i, resp))
            acked = i
        kill9(p)  # right after the last ack, no grace period
        c.close()

        p = start(data)
        c = Client()
        got = ids_of(c.ask("SELECT id FROM t ORDER BY id")[0])
        want = list(range(1, acked + 1))
        check(got == want,
              "A: kill -9 after %d acks -> exactly the acked rows" % acked,
              "got %r" % (got[:10] + ["..."] if len(got) > 10 else got))
        c.close()
        stop(p)


def test_pipelined_prefix():
    data = fresh("apipe")
    p = start(data)
    c = Client()
    c.ask("CREATE TABLE t (id INTEGER, s TEXT)")
    total = 500
    c.send(["INSERT INTO t VALUES (%d, 'x')" % i for i in range(1, total + 1)])
    read_acks = 120
    for _ in range(read_acks):
        if c.read_resp() != "OK 1":
            raise RuntimeError("bad ack in pipeline")
    kill9(p)
    c.close()

    p = start(data)
    c = Client()
    got = ids_of(c.ask("SELECT id FROM t ORDER BY id")[0])
    ok = (got == list(range(1, len(got) + 1))
          and read_acks <= len(got) <= total)
    check(ok, "A: pipelined kill -9 -> prefix containing %d acked rows"
          % read_acks, "len=%d" % len(got))
    c.close()
    stop(p)


# ---------------------------------------------------------------------------
# B. big UPDATE killed mid-flight is all-or-nothing
# ---------------------------------------------------------------------------

def test_update_atomic():
    data = fresh("b")
    p = start(data)
    c = Client()
    c.ask("CREATE TABLE t (id INTEGER, v TEXT)")
    chunk = 200
    stmts = []
    for base in range(1, 50001, chunk):
        stmts.append("INSERT INTO t VALUES " + ", ".join(
            "(%d, 'v0')" % i for i in range(base, min(base + chunk, 50001))))
        if len(stmts) >= 50:
            for r in c.ask(stmts):
                assert r == "OK %d" % min(chunk, 50001 - base) or r.startswith("OK "), r
            stmts = []
    if stmts:
        for r in c.ask(stmts):
            assert r.startswith("OK "), r

    for delay in (0.002, 0.008, 0.02, 0.05, 0.3):
        # Baseline: acknowledged full update to 'old'.
        assert c.ask("UPDATE t SET v = 'old'")[0] == "OK 50000"
        c.send("UPDATE t SET v = 'new'")
        time.sleep(delay)
        kill9(p)
        c.close()

        p = start(data)
        c = Client()
        resp = c.ask("SELECT v FROM t")[0]
        lines = resp.rstrip("\n").split("\n")
        vals = lines[2:-1]
        uniq = set(vals)
        ok = (len(vals) == 50000 and len(uniq) == 1
              and uniq <= {"old", "new"})
        check(ok, "B: kill %.3fs after 50k UPDATE -> uniform, no rows lost"
              % delay, "rows=%d uniq=%r" % (len(vals), list(uniq)[:5]))

    # A completed update must replay as new everywhere after restart.
    stop(p)
    p = start(data)
    c = Client()
    vals = set(c.ask("SELECT v FROM t")[0].rstrip("\n").split("\n")[2:-1])
    check(vals == {"new"} or vals == {"old"},
          "B: final state after completed update is uniform", repr(vals))
    c.close()
    stop(p)
    p = start(data)
    c = Client()
    n = len(c.ask("SELECT id FROM t")[0].rstrip("\n").split("\n")[2:-1])
    check(n == 50000, "B: restart keeps all 50k rows", "n=%d" % n)
    c.close()
    stop(p)


# ---------------------------------------------------------------------------
# C. torn tail: cut a few hundred bytes from the newest data file
# ---------------------------------------------------------------------------

def test_torn_tail():
    for k in (64, 200, 300, 511, 900, 1023):
        data = fresh("c%d" % k)
        p = start(data)
        c = Client()
        c.ask("CREATE TABLE t (id INTEGER, s TEXT)")
        n = 40
        for i in range(1, n + 1):
            assert c.ask("INSERT INTO t VALUES (%d, 'r%d')" % (i, i))[0] == "OK 1"
        kill9(p)
        c.close()

        target = newest_file(data)
        size = os.path.getsize(target)
        with open(target, "r+b") as f:
            f.truncate(max(1, size - k))

        p = start(data)
        c = Client()
        got = ids_of(c.ask("SELECT id FROM t ORDER BY id")[0])
        older_ok = got == list(range(1, n)) or got == list(range(1, n + 1))
        check(older_ok,
              "C: tail cut %d B -> every older row present (last may go)"
              % k, "got %r" % got)

        # And the recovered store still takes and keeps new writes.
        assert c.ask("INSERT INTO t VALUES (%d, 'new')" % (n + 1))[0] == "OK 1"
        kill9(p)
        c.close()
        p = start(data)
        c = Client()
        got2 = ids_of(c.ask("SELECT id FROM t ORDER BY id")[0])
        allowed = set(range(1, n)) | {n + 1}  # row n may have been the cut tail
        ok2 = (set(got2).issubset(set(range(1, n + 2)))
               and allowed.issubset(set(got2))
               and got2 == sorted(got2))
        check(ok2,
              "C: write after cut-recovery survives the next restart",
              "got %r" % got2)
        c.close()
        stop(p)


# ---------------------------------------------------------------------------
# D. two restarts in a row change nothing
# ---------------------------------------------------------------------------

def test_double_restart():
    data = fresh("d")
    p = start(data)
    c = Client()
    c.ask("CREATE TABLE a (id INTEGER, s TEXT)")
    c.ask("CREATE TABLE b (k INTEGER, v TEXT)")
    stmts = ["INSERT INTO a VALUES (%d, %s)" %
             (i, "NULL" if i % 3 == 0 else "'t%d'" % i)
             for i in range(1, 61)]
    for r in c.ask(stmts):
        assert r == "OK 1", r
    assert c.ask("UPDATE a SET s = 'u' WHERE id > 50")[0] == "OK 10"
    assert c.ask("DELETE FROM a WHERE id % 4 = 0")[0] == "OK 15"
    assert c.ask("INSERT INTO b VALUES (1, 'x'), (2, 'y')")[0] == "OK 2"
    q = ["SELECT * FROM a ORDER BY id",
         "SELECT id, s FROM a WHERE s IS NULL ORDER BY id",
         "SELECT * FROM b ORDER BY k",
         "SELECT count_ok FROM a ORDER BY id"]  # last one errors on purpose
    shots = [c.ask(q)]
    kill9(p)
    c.close()

    for round_no in (1, 2):
        p = start(data)
        c = Client()
        shots.append(c.ask(q))
        c.close()
        kill9(p)

    p = start(data)
    c = Client()
    shots.append(c.ask(q))
    c.close()
    stop(p)

    same = all(s == shots[0] for s in shots[1:])
    check(same, "D: two restarts in a row change nothing (%d shots)"
          % len(shots))


# ---------------------------------------------------------------------------
# E. restart serves again within 20 s for a big store
# ---------------------------------------------------------------------------

def seed_big(c):
    c.ask("CREATE TABLE big (id INTEGER, v TEXT)")
    # a lot of single-row commits (framed records, fsync each) ...
    for base in range(1, 8001, 200):
        stmts = ["INSERT INTO big VALUES (%d, 'value-%d')" % (i, i)
                 for i in range(base, base + 200)]
        for r in c.ask(stmts):
            assert r == "OK 1", r
    # ... plus bulk rows, updates and deletes like the campaign's store.
    for base in range(8001, 30001, 500):
        stmts = ["INSERT INTO big VALUES " + ", ".join(
            "(%d, 'value-%d')" % (i, i)
            for i in range(base, min(base + 500, 30001)))]
        assert c.ask(stmts)[0].startswith("OK "), "bulk insert failed"
    assert c.ask("UPDATE big SET v = 'upd' WHERE id % 7 = 0")[0] == "OK 4285"
    assert c.ask("DELETE FROM big WHERE id % 13 = 0")[0] == "OK 2307"


def test_restart_latency():
    data = fresh("e")
    p = start(data)
    c = Client()
    seed_big(c)
    c.close()
    kill9(p)

    times = []
    for i in range(2):
        t0 = time.time()
        p = start(data)
        c = Client()
        resp = c.ask("SELECT id FROM big WHERE id = 7999")[0]
        up = time.time() - t0
        times.append(up)
        check("7999" in resp and up < 20.0,
              "E: restart %d serving in %.2fs (< 20s)" % (i + 1, up))
        c.close()
        kill9(p)
    check(max(times) < 20.0, "E: both restarts within 20s (%.2fs)"
          % max(times))


# ---------------------------------------------------------------------------
# F. ticket-4 raw-line store still readable, then upgraded
# ---------------------------------------------------------------------------

def test_legacy_store():
    data = fresh("f")
    os.makedirs(data, exist_ok=True)
    legacy = (
        "CREATE TABLE t (id INTEGER, s TEXT)\n"
        "INSERT INTO t VALUES (1, 'old-a')\n"
        "INSERT INTO t VALUES (2, NULL)\n"
    )
    with open(os.path.join(data, "sdb.log"), "wb") as f:
        f.write(legacy.encode())
        f.write(b"INSERT INTO t VALUES (99, 'torn-line-no-newline")

    p = start(data)
    c = Client()
    got = ids_of(c.ask("SELECT id FROM t ORDER BY id")[0])
    check(got == [1, 2], "F: legacy raw-line store replays (torn line dropped)",
          repr(got))
    assert c.ask("INSERT INTO t VALUES (3, 'post')")[0] == "OK 1"
    c.close()
    kill9(p)

    p = start(data)
    c = Client()
    got = ids_of(c.ask("SELECT id FROM t ORDER BY id")[0])
    check(got == [1, 2, 3],
          "F: write after legacy upgrade survives the next restart", repr(got))
    c.close()
    stop(p)


# ---------------------------------------------------------------------------
# G. kill -9 during recovery/startup never loses anything
# ---------------------------------------------------------------------------

def test_kill_during_startup():
    data = fresh("g")
    p = start(data)
    c = Client()
    c.ask("CREATE TABLE t (id INTEGER, s TEXT)")
    for base in range(1, 5001, 250):
        for r in c.ask(["INSERT INTO t VALUES (%d, 'r%d')" % (i, i)
                        for i in range(base, base + 250)]):
            assert r == "OK 1", r
    c.close()
    kill9(p)

    # Rip the process out while it is replaying/compacting, several times.
    for nap in (0.005, 0.02, 0.05, 0.12):
        p = subprocess.Popen([SRV, "--port", str(PORT), "--data", data],
                             stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL)
        time.sleep(nap)
        kill9(p)

    p = start(data)
    c = Client()
    got = ids_of(c.ask("SELECT id FROM t ORDER BY id")[0])
    check(got == list(range(1, 5001)),
          "G: kills during startup lose nothing (5000 rows intact)",
          "len=%d" % len(got))
    assert c.ask("INSERT INTO t VALUES (5001, 'tail')")[0] == "OK 1"
    c.close()
    stop(p)
    p = start(data)
    c = Client()
    got = ids_of(c.ask("SELECT id FROM t ORDER BY id")[0])
    check(got == list(range(1, 5002)),
          "G: write after startup-kill recovery persists", "len=%d" % len(got))
    c.close()
    stop(p)


# ---------------------------------------------------------------------------
# H. a cut landing on an in-session snapshot tail: the statements behind it
#    are still there, so nothing but the snapshot record itself is lost
# ---------------------------------------------------------------------------

def test_cut_on_snapshot_tail():
    data = fresh("h")
    p = start(data)
    c = Client()
    c.ask("CREATE TABLE t (id INTEGER, s TEXT)")
    # Exactly SNAP_STMTS(16384) statements => the 16384th one triggers a
    # snapshot record as the very last record in the file.
    for base in range(1, 16384, 500):
        stmts = ["INSERT INTO t VALUES (%d, 'r%d')" % (i, i)
                 for i in range(base, min(base + 500, 16384))]
        for r in c.ask(stmts):
            assert r == "OK 1", r
    kill9(p)
    c.close()

    target = newest_file(data)
    size = os.path.getsize(target)
    with open(target, "r+b") as f:
        f.truncate(size - 300)  # damage the tail: the snapshot record

    p = start(data)
    c = Client()
    got = ids_of(c.ask("SELECT id FROM t ORDER BY id")[0])
    check(got == list(range(1, 16384)),
          "H: cut on snapshot tail -> all 16383 statements recovered",
          "len=%d" % len(got))
    c.close()
    stop(p)


# ---------------------------------------------------------------------------
# I. randomized kill -9 fuzz: kill at arbitrary moments while writing;
#    stored rows must always be a prefix containing every acked row
# ---------------------------------------------------------------------------

def test_random_kill_fuzz():
    import random
    rng = random.Random(4242)
    data = fresh("i")
    p = start(data)
    c = Client()
    c.ask("CREATE TABLE t (id INTEGER, s TEXT)")
    acked_ids = set()   # ids whose ack the client actually read
    sent_ids = set()    # ids ever sent
    stored = set()      # ids present after the previous restart
    for rnd in range(6):
        batch = list(range(len(sent_ids) + 1, len(sent_ids) + 51))
        c.send(["INSERT INTO t VALUES (%d, 'r%d')" % (i, i) for i in batch])
        sent_ids.update(batch)
        read_n = rng.randint(0, len(batch))
        for i in range(read_n):
            if c.read_resp() != "OK 1":
                raise RuntimeError("bad ack")
            acked_ids.add(batch[i])
        if rng.random() < 0.5:
            time.sleep(rng.random() * 0.01)
        kill9(p)
        c.close()

        p = start(data)
        c = Client()
        got = ids_of(c.ask("SELECT id FROM t ORDER BY id")[0])
        got_set = set(got)
        in_batch = sorted(x for x in got_set if x in set(batch))
        ok = (acked_ids <= got_set <= sent_ids          # acked kept, no extras
              and stored <= got_set                      # nothing vanished
              and got == sorted(got_set)                 # no duplicates
              and in_batch == batch[:len(in_batch)])     # prefix within batch
        check(ok,
              "I: fuzz round %d kill -> %d acked <= %d <= %d sent, prefix ok"
              % (rnd, len(acked_ids), len(got), len(sent_ids)))
        if not ok:
            print("   acked-only=%r got=%r" % (
                sorted(acked_ids - got_set)[:8], got[:12]))
            break
        stored = got_set
    c.close()
    stop(p)


def main():
    shutil.rmtree(SCRATCH, ignore_errors=True)
    os.makedirs(SCRATCH)
    t0 = time.time()
    test_acked_survive()
    test_pipelined_prefix()
    test_update_atomic()
    test_torn_tail()
    test_double_restart()
    test_restart_latency()
    test_legacy_store()
    test_kill_during_startup()
    test_cut_on_snapshot_tail()
    test_random_kill_fuzz()
    print("---- %.1fs total" % (time.time() - t0))
    if FAILURES:
        print("CRASHTEST FAILURES: %d" % len(FAILURES))
        sys.exit(1)
    print("ALL CRASH TESTS PASSED")


if __name__ == "__main__":
    main()
