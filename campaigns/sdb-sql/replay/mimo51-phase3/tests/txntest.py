#!/usr/bin/env python3
"""Ticket 6: transactions.

1. isolation across connections (uncommitted invisible, COMMIT visible,
   ROLLBACK never),
2. errors inside a transaction do not abort it (SQLite semantics),
3. ERR txn probes: nested BEGIN, COMMIT/ROLLBACK with no transaction,
   compared by class against the reference,
4. kill -9 with a transaction open: committed rows survive, uncommitted
   ones leave no trace,
5. writer behaviour: a write transaction blocks other writers, a read-only
   one does not, and a dropped connection rolls back and unblocks,
6. a differential random-transaction generator: whole transactions plus
   probes, every answer compared with the reference,
7. DDL inside a transaction rolls back; pipelined transactions work.
"""
import os
import random
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRV = os.path.join(ROOT, "start.sh")
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 5606
REF = ("ref-sql", 5480)
FAILURES = []


def check(cond, what, detail=""):
    if cond:
        print("ok   " + what)
    else:
        print("FAIL " + what + ("  " + detail if detail else ""))
        FAILURES.append(what)


class Conn:
    def __init__(self, addr, timeout=20):
        self.s = socket.create_connection(addr, timeout=timeout)
        self.buf = b""

    def sendq(self, q):
        if isinstance(q, str):
            q = q.encode()
        self.s.sendall(q if q.endswith(b"\n") else q + b"\n")

    def _line(self, timeout):
        self.s.settimeout(timeout)
        while b"\n" not in self.buf:
            d = self.s.recv(65536)
            if not d:
                raise EOFError("closed")
            self.buf += d
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode("utf-8", "replace")

    def ask(self, line, timeout=20):
        """One statement -> one answer; ROWS answers are lists."""
        self.sendq(line)
        return self.recv(timeout)

    def recv(self, timeout=20):
        first = self._line(timeout)
        if first.startswith("ROWS "):
            out = [first]
            while True:
                l = self._line(timeout)
                out.append(l)
                if l.startswith("END "):
                    return out
            # unreachable
        return [first]

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


def start(data, port=PORT):
    os.makedirs(data, exist_ok=True)
    p = subprocess.Popen([SRV, "--port", str(port), "--data", data],
                         stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    for _ in range(4000):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=1)
            s.close()
            return p
        except OSError:
            if p.poll() is not None:
                raise RuntimeError("server died: " +
                                   (p.stderr.read().decode() or ""))
            time.sleep(0.01)
    raise RuntimeError("server never came up")


def stop(p, sig=signal.SIGTERM):
    if p.poll() is None:
        p.send_signal(sig)
        try:
            p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait(timeout=10)


def norm(ans):
    """Normalize an answer for cross-server comparison."""
    if isinstance(ans, list):
        if ans and ans[0].startswith("ROWS "):
            hdr = ans[0]
            rows = sorted(ans[1:-1])
            return [hdr] + rows + [ans[-1]]
        return ans
    return ans


def same(a, b):
    a, b = norm(a), norm(b)
    if isinstance(a, list) and a and a[0].startswith("ERR "):
        return a[0].split()[1] == b[0].split()[1]  # class only
    if isinstance(a, list) and a and a[0].startswith("ERR "):
        return False
    if isinstance(b, list) and b and b[0].startswith("ERR "):
        return False
    return a == b


def run_pair(stmts, tag):
    """Send the same statements to candidate and reference, compare."""
    c = Conn(("127.0.0.1", PORT))
    r = Conn(REF)
    bad = 0
    try:
        for st in stmts:
            ca, ra = c.ask(st), r.ask(st)
            if not same(ca, ra):
                bad += 1
                check(False, tag, "%r cand=%r ref=%r" % (st, ca, ra))
    finally:
        c.close()
        r.close()
    if bad == 0:
        check(True, "%s (%d statements)" % (tag, len(stmts)))
    return bad


# ---------------------------------------------------------------- 1. isolation
def test_isolation():
    a, b = Conn(("127.0.0.1", PORT)), Conn(("127.0.0.1", PORT))
    t = "iso_%d" % random.randrange(1 << 30)
    check(a.ask("CREATE TABLE %s (id INTEGER, s TEXT)" % t) == ["OK 0"], "iso create")
    check(a.ask("INSERT INTO %s VALUES (0, 'base')" % t) == ["OK 1"], "iso seed")
    check(a.ask("BEGIN") == ["OK 0"], "iso begin")
    check(a.ask("INSERT INTO %s VALUES (1, 'pending')" % t) == ["OK 1"], "iso insert in txn")
    seen = b.ask("SELECT * FROM %s" % t)
    check(seen[2:-1] == ["0\tbase"], "other connection does not see uncommitted row", str(seen))
    # the writer sees its own changes
    seen_self = a.ask("SELECT * FROM %s" % t)
    check(len(seen_self) == 5, "writer sees own uncommitted row", str(seen_self))
    check(a.ask("COMMIT") == ["OK 0"], "iso commit")
    seen = b.ask("SELECT * FROM %s" % t)
    check(seen[2:-1] == ["0\tbase", "1\tpending"], "row visible after COMMIT", str(seen))
    # rollback
    check(a.ask("BEGIN") == ["OK 0"], "iso begin2")
    check(a.ask("INSERT INTO %s VALUES (2, 'doomed')" % t) == ["OK 1"], "iso insert2")
    check(a.ask("ROLLBACK") == ["OK 0"], "iso rollback")
    seen = b.ask("SELECT * FROM %s" % t)
    check(seen[2:-1] == ["0\tbase", "1\tpending"], "rolled-back row never visible", str(seen))
    a.close()
    b.close()


# ------------------------------------------------- 2. errors keep txn open
def test_errors_keep_txn_open():
    c = Conn(("127.0.0.1", PORT))
    t = "err_%d" % random.randrange(1 << 30)
    c.ask("CREATE TABLE %s (id INTEGER, s TEXT)" % t)
    check(c.ask("BEGIN") == ["OK 0"], "err begin")
    check(c.ask("INSERT INTO %s VALUES (1, 'ok')" % t) == ["OK 1"], "err first insert")
    e1 = c.ask("INSERT INTO %s VALUES ('zz', 'x')" % t)      # type error
    e2 = c.ask("INSERT INTO %s VALUES (2, 'a', 'b')" % t)    # arity error
    e3 = c.ask("INSERT INTO nosuch VALUES (1)")              # no_table
    e4 = c.ask("UPDATE %s SET nosuch = 1" % t)               # no_column
    check(e1[0].startswith("ERR type"), "type error class", str(e1))
    check(e2[0].startswith("ERR arity"), "arity error class", str(e2))
    check(e3[0].startswith("ERR no_table"), "no_table error class", str(e3))
    check(e4[0].startswith("ERR no_column"), "no_column error class", str(e4))
    rows = c.ask("SELECT * FROM %s" % t)
    check(rows[2:-1] == ["1\tok"], "failing statements changed nothing", str(rows))
    check(c.ask("INSERT INTO %s VALUES (3, 'after')" % t) == ["OK 1"],
          "transaction still open after errors")
    check(c.ask("COMMIT") == ["OK 0"], "commit after errors")
    rows = c.ask("SELECT * FROM %s ORDER BY id" % t)
    check(rows[2:-1] == ["1\tok", "3\tafter"], "good statements committed", str(rows))
    c.close()


# ------------------------------------------------------------- 3. txn probes
PROBES = [
    "BEGIN", "COMMIT", "ROLLBACK",
    "BEGIN", "BEGIN", "ROLLBACK", "ROLLBACK",
    "BEGIN", "COMMIT", "COMMIT", "ROLLBACK",
    "bEgIn", "cOmMiT", "rOlLbAcK",
    "BEGIN TRANSACTION", "COMMIT TRANSACTION", "ROLLBACK TRANSACTION",
    "END", "END TRANSACTION",
    "BEGIN FOO", "COMMIT FOO", "ROLLBACK FOO",
    "BEGIN WORK", "COMMIT WORK",
    "START TRANSACTION", "TRANSACTION",
    "BEGIN TRANSACTION TRANSACTION",
    "SAVEPOINT sp",  # informational: reference accepts, we do not claim to
]


def test_probes_differential():
    # Fresh connection state on both sides: run the txn probes only, and
    # compare the classes (SAVEPOINT is outside the contract, excluded).
    c = Conn(("127.0.0.1", PORT))
    r = Conn(REF)
    bad = 0
    for st in PROBES:
        if st.startswith("SAVEPOINT"):
            c.ask(st)
            r.ask(st)
            continue
        ca, ra = c.ask(st), r.ask(st)
        cc = ca[0].split()[1] if ca[0].startswith("ERR ") else ca[0]
        rc = ra[0].split()[1] if ra[0].startswith("ERR ") else ra[0]
        if cc != rc:
            bad += 1
            check(False, "probe %r" % st, "cand=%r ref=%r" % (ca, ra))
    check(bad == 0, "txn error probes match reference (%d)" % len(PROBES))
    c.close()
    r.close()


# ---------------------------------------------------- 4. kill with txn open
def test_kill_with_txn_open():
    data = "/tmp/sdbtest/txn/kill"
    shutil.rmtree(data, ignore_errors=True)
    p = start(data)
    port = PORT
    c = Conn(("127.0.0.1", port))
    t = "kill_%d" % random.randrange(1 << 30)
    assert c.ask("CREATE TABLE %s (id INTEGER, s TEXT)" % t) == ["OK 0"]
    acked = 0
    for i in range(10):
        assert c.ask("INSERT INTO %s VALUES (%d, 'ac')" % (t, i)) == ["OK 1"]
        acked += 1
    # committed inside a transaction
    assert c.ask("BEGIN") == ["OK 0"]
    for i in range(10, 20):
        assert c.ask("INSERT INTO %s VALUES (%d, 'tx')" % (t, i)) == ["OK 1"]
    assert c.ask("COMMIT") == ["OK 0"]
    # left open — the connection stays alive while the server is killed
    assert c.ask("BEGIN") == ["OK 0"]
    for i in range(20, 30):
        assert c.ask("INSERT INTO %s VALUES (%d, 'un')" % (t, i)) == ["OK 1"]
    os.kill(p.pid, signal.SIGKILL)
    p.wait(timeout=10)
    c.close()

    p = start(data)
    c = Conn(("127.0.0.1", port))
    rows = c.ask("SELECT * FROM %s" % t)
    ids = sorted(int(l.split("\t")[0]) for l in rows[2:-1])
    expect = list(range(0, 20))
    check(ids == expect, "acked+committed survive kill -9, uncommitted gone",
          "got %s" % ids)
    # kill immediately after a COMMIT ack: the commit must be durable
    assert c.ask("BEGIN") == ["OK 0"]
    assert c.ask("INSERT INTO %s VALUES (50, 'fast')" % t) == ["OK 1"]
    assert c.ask("COMMIT") == ["OK 0"]
    os.kill(p.pid, signal.SIGKILL)
    p.wait(timeout=10)
    c.close()

    p = start(data)
    c = Conn(("127.0.0.1", port))
    rows = c.ask("SELECT * FROM %s" % t)
    ids = sorted(int(l.split("\t")[0]) for l in rows[2:-1])
    check(ids == expect + [50], "commit acked right before kill -9 survives",
          "got %s" % ids)
    # and the store still works and persists
    assert c.ask("BEGIN") == ["OK 0"]
    assert c.ask("INSERT INTO %s VALUES (99, 'new')" % t) == ["OK 1"]
    assert c.ask("COMMIT") == ["OK 0"]
    c.close()
    stop(p)
    p = start(data)
    c = Conn(("127.0.0.1", port))
    rows = c.ask("SELECT * FROM %s" % t)
    ids = sorted(int(l.split("\t")[0]) for l in rows[2:-1])
    check(ids == expect + [50, 99], "commit after restart survives another restart",
          "got %s" % ids)
    c.close()
    stop(p)


# --------------------------------------------------- 5. writer/drop behaviour
def test_writer_blocking():
    a, b = Conn(("127.0.0.1", PORT)), Conn(("127.0.0.1", PORT))
    t = "blk_%d" % random.randrange(1 << 30)
    a.ask("CREATE TABLE %s (id INTEGER, s TEXT)" % t)
    a.ask("BEGIN")
    a.ask("INSERT INTO %s VALUES (1, 'w')" % t)

    res = {}

    def blocked_write(vid=2):
        try:
            res["b"] = b.ask("INSERT INTO %s VALUES (%d, 'b')" % (t, vid),
                             timeout=30)
        except Exception as e:  # noqa: BLE001
            res["b"] = "EXC %r" % (e,)

    th = threading.Thread(target=blocked_write)
    th.start()
    time.sleep(0.5)
    check("b" not in res, "autocommit writer blocks behind a write transaction")
    # a plain reader is never blocked and sees the old state
    r = Conn(("127.0.0.1", PORT))
    seen = r.ask("SELECT * FROM %s" % t)
    check(seen[2:-1] == [], "reader sees old state during transaction (only the uncommitted row exists)", str(seen))
    assert a.ask("COMMIT") == ["OK 0"]
    th.join(30)
    check(res.get("b") == ["OK 1"], "blocked writer proceeds after COMMIT", str(res))
    seen = r.ask("SELECT * FROM %s" % t)
    check(len(seen) == 5, "both rows visible after COMMIT", str(seen))

    # a transaction with no writes takes no lock: the other writer answers
    # immediately (reference: deferred transactions).
    a.ask("BEGIN")
    res2 = {}

    def free_write():
        try:
            res2["b"] = b.ask("INSERT INTO %s VALUES (3, 'c')" % t, timeout=5)
        except Exception as e:  # noqa: BLE001
            res2["b"] = "EXC %r" % (e,)

    th2 = threading.Thread(target=free_write)
    th2.start()
    th2.join(8)
    check(res2.get("b") == ["OK 1"], "idle transaction does not block writers",
          str(res2))
    assert a.ask("ROLLBACK") == ["OK 0"]

    # a connection that dies mid-transaction rolls back and unblocks
    a.ask("BEGIN")
    a.ask("INSERT INTO %s VALUES (4, 'dead')" % t)
    a.close()
    th3 = threading.Thread(target=lambda: blocked_write(5))
    res["b"] = None
    th3.start()
    th3.join(15)
    check(res.get("b") == ["OK 1"], "writer proceeds after disconnect rolls back",
          str(res))
    seen = r.ask("SELECT * FROM %s" % t)
    ids = sorted(int(l.split("\t")[0]) for l in seen[2:-1])
    check(ids == [1, 2, 3, 5], "dropped transaction left no trace", str(seen))
    r.close()
    b.close()


# ------------------------------------------- 6. differential transactions
def gen_txn_workload(seed):
    rnd = random.Random(seed)
    pre = "g%d_%d_" % (seed, int(time.time() * 1000) % 100000000)
    stmts = []
    cols = 3
    names = [pre + "t%d" % i for i in range(2)]
    for n in names:
        ctype = ["INTEGER", "TEXT"] * 3
        stmts.append("CREATE TABLE %s (%s)" % (
            n, ", ".join("c%d %s" % (i, ctype[i]) for i in range(cols))))
    for i in range(6):
        stmts.append("INSERT INTO %s VALUES (%d, 'v%d', %d)" %
                     (names[0], i, i, i * 2))
        stmts.append("INSERT INTO %s VALUES (%d, %d, 'x%d')" %
                     (names[1], i, i + 100, i))

    def one_dml():
        n = rnd.choice(names)
        k = rnd.randrange(7)
        if k == 0:
            v = rnd.randrange(100)
            return "INSERT INTO %s VALUES (%d, 'n%d', %d)" % (n, v, v, v)
        if k == 1:
            i = rnd.randrange(6)
            return "INSERT INTO %s (c0, c2) VALUES (%d, %d)" % (n, i, i)
        if k == 2:
            return "UPDATE %s SET c2 = c2 + 1 WHERE c0 < 4" % n
        if k == 3:
            return "UPDATE %s SET c1 = 'u' WHERE c0 = %d" % (n, rnd.randrange(6))
        if k == 4:
            return "DELETE FROM %s WHERE c0 = %d" % (n, rnd.randrange(6))
        if k == 5:
            return "INSERT INTO %s VALUES ('zz', 1, 2)" % n  # type error
        return "INSERT INTO %s VALUES (1, 2)" % n            # arity error

    def one_select():
        n = rnd.choice(names)
        k = rnd.randrange(4)
        if k == 0:
            return "SELECT * FROM %s ORDER BY c0, c1" % n
        if k == 1:
            return "SELECT c0, c2 FROM %s WHERE c0 >= 2 ORDER BY c0" % n
        if k == 2:
            return "SELECT count(*) AS n FROM %s" % n if False else \
                   "SELECT c0 FROM %s ORDER BY c0" % n
        return "SELECT c0, c1 FROM %s WHERE c1 > 'q' ORDER BY c0" % n

    # body: a mix of autocommit statements and whole transactions
    for _ in range(70):
        r = rnd.random()
        if r < 0.35:
            stmts.append(one_dml())
        elif r < 0.5:
            stmts.append(one_select())
        elif r < 0.75:
            block = ["BEGIN"]
            for _ in range(rnd.randrange(1, 5)):
                block.append(one_dml() if rnd.random() < 0.8 else one_select())
            block.append(rnd.choice(["COMMIT", "ROLLBACK"]))
            stmts.extend(block)
        else:
            # txn error probes sprinkled in
            stmts.append(rnd.choice(
                ["BEGIN", "COMMIT", "ROLLBACK", "BEGIN", "COMMIT",
                 "ROLLBACK", "BEGIN TRANSACTION", "END"]))
    # make sure no transaction is left open at the end
    stmts.append("COMMIT")
    stmts.append("ROLLBACK")
    return stmts


def test_differential():
    total_bad = 0
    for seed in (11, 22, 33, 44, 55):
        stmts = gen_txn_workload(seed)
        total_bad += run_pair(stmts, "differential txn workload seed=%d" % seed)
    return total_bad


# ------------------------------------- 7. DDL rollback + pipelining
def test_ddl_and_pipeline():
    c = Conn(("127.0.0.1", PORT))
    base = "ddl_%d" % random.randrange(1 << 30)
    c.ask("CREATE TABLE %s (id INTEGER)" % base)
    c.ask("INSERT INTO %s VALUES (1)" % base)
    c.ask("BEGIN")
    c.ask("CREATE TABLE %s_x (x INTEGER)" % base)
    c.ask("INSERT INTO %s_x VALUES (7)" % base)
    c.ask("ROLLBACK")
    got = c.ask("SELECT * FROM %s_x" % base)
    check(got[0].startswith("ERR no_table"), "DDL rolled back with the transaction",
          str(got))
    got = c.ask("SELECT * FROM %s" % base)
    check(got[2:-1] == ["1"], "base table intact after DDL rollback", str(got))

    # pipelined transaction: all statements in one write, answers in order
    p = Conn(("127.0.0.1", PORT))
    t = "pipe_%d" % random.randrange(1 << 30)
    p.sendq("CREATE TABLE %s (id INTEGER)\nBEGIN\nINSERT INTO %s VALUES (1)\n"
            "INSERT INTO %s VALUES (2)\nCOMMIT\nSELECT * FROM %s ORDER BY id\n"
            % (t, t, t, t))
    answers = [p.recv() for _ in range(6)]
    expect = [["OK 0"], ["OK 0"], ["OK 1"], ["OK 1"], ["OK 0"],
              ["ROWS 1", "id", "1", "2", "END 2"]]
    check(answers == expect, "pipelined transaction", str(answers))
    p.close()
    c.close()


# ------------------------------------- 7. DDL rollback + pipelining
def test_interleaved_differential():
    """Two connections, strictly sequenced: every answer compared with the
    reference, including a write landing between another connection's BEGIN
    and its first write (deferred transactions)."""
    ca, cb = Conn(("127.0.0.1", PORT)), Conn(("127.0.0.1", PORT))
    ra, rb = Conn(REF), Conn(REF)
    # Unique per run: the reference keeps its data between runs.
    t = "il_%d_%d" % (random.randrange(1 << 30), int(time.time() * 1000))
    bad = 0
    script = [
        (ca, ra, "CREATE TABLE %s (id INTEGER, s TEXT)" % t),
        (cb, rb, "SELECT * FROM %s" % t),
        (ca, ra, "BEGIN"),
        (ca, ra, "INSERT INTO %s VALUES (1, 'a')" % t),
        (cb, rb, "SELECT * FROM %s" % t),
        (ca, ra, "SELECT * FROM %s" % t),
        (ca, ra, "END"),                      # END commits an active txn
        (cb, rb, "SELECT * FROM %s" % t),
        (ca, ra, "BEGIN"),
        (cb, rb, "INSERT INTO %s VALUES (2, 'b')" % t),  # lands mid-txn
        (ca, ra, "SELECT * FROM %s" % t),     # deferred txn sees it
        (ca, ra, "INSERT INTO %s VALUES (3, 'c')" % t),
        (ca, ra, "COMMIT"),
        (cb, rb, "SELECT * FROM %s ORDER BY id" % t),
        (ca, ra, "BEGIN"),
        (ca, ra, "INSERT INTO %s VALUES (4, 'd')" % t),
        (cb, rb, "BEGIN"),
        (ca, ra, "ROLLBACK"),
        (cb, rb, "INSERT INTO %s VALUES (5, 'e')" % t),
        (cb, rb, "COMMIT"),
        (ca, ra, "SELECT * FROM %s ORDER BY id" % t),
        (cb, rb, "SELECT * FROM %s ORDER BY id" % t),
        (ca, ra, "COMMIT"),                   # closed already
        (cb, rb, "ROLLBACK"),                 # closed already
    ]
    try:
        for cconn, rconn, st in script:
            cA, rA = cconn.ask(st), rconn.ask(st)
            if not same(cA, rA):
                bad += 1
                check(False, "interleaved %r" % st,
                      "cand=%r ref=%r" % (cA, rA))
    finally:
        for x in (ca, cb, ra, rb):
            x.close()
    check(bad == 0, "two-connection interleavings match reference (%d)"
          % len(script))


def main():
    random.seed(20261120)
    data = "/tmp/sdbtest/txn/main"
    shutil.rmtree(data, ignore_errors=True)
    p = start(data)
    try:
        test_isolation()
        test_errors_keep_txn_open()
        test_probes_differential()
        test_writer_blocking()
        test_ddl_and_pipeline()
        test_interleaved_differential()
        test_differential()
    finally:
        stop(p)
    test_kill_with_txn_open()
    if FAILURES:
        print("%d FAILURES" % len(FAILURES))
        sys.exit(1)
    print("ALL TRANSACTION TESTS PASSED")


if __name__ == "__main__":
    main()
