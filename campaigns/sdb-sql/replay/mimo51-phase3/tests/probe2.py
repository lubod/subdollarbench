#!/usr/bin/env python3
"""Quick differential probes: ordinals, big tables, clause combinations."""
import random
import sys

import orderfuzz as of  # noqa: E402  (raw-socket helpers)


def stmts():
    s = []
    import os
    name = "big%s" % os.urandom(3).hex()
    s.append("CREATE TABLE %s (id INTEGER, s TEXT, n INTEGER)" % name)
    rng = random.Random(7)
    rows = []
    for _ in range(500):
        n = "NULL" if rng.random() < 0.2 else str(rng.randint(-50, 50))
        v = "NULL" if rng.random() < 0.2 else "'%s'" % rng.choice(
            ["a", "b", "c", "zz", "", "m1", "m2"])
        rows.append("(%d, %s, %s)" % (rng.randint(0, 60), v, n))
    for i in range(0, len(rows), 50):
        s.append("INSERT INTO big VALUES %s" % ", ".join(rows[i:i + 50]))
    # ordinals
    s.append("SELECT id, s, n FROM big ORDER BY 1 ASC, 2 DESC")
    s.append("SELECT id, s, n FROM big ORDER BY 3 DESC, 1 ASC")
    s.append("SELECT DISTINCT id FROM big ORDER BY 1 DESC LIMIT 5")
    s.append("SELECT DISTINCT s, n FROM big ORDER BY 2, 1")
    s.append("SELECT id + 1 AS z, s FROM big ORDER BY 1, 2 DESC")
    # combined: DISTINCT + WHERE + multi-key ORDER + LIMIT/OFFSET
    s.append("SELECT DISTINCT id, s FROM big WHERE id > 10 "
             "ORDER BY s DESC, id ASC LIMIT 17 OFFSET 5")
    s.append("SELECT DISTINCT n FROM big ORDER BY n ASC LIMIT 3 OFFSET 3")
    s.append("SELECT DISTINCT n FROM big ORDER BY n DESC LIMIT 4")
    s.append("SELECT id, s FROM big WHERE n IS NULL ORDER BY id, s LIMIT 10")
    s.append("SELECT id, s FROM big WHERE n IS NULL ORDER BY id DESC, "
             "s DESC LIMIT 10 OFFSET 2")
    s.append("SELECT s FROM big ORDER BY s, id LIMIT 0")
    s.append("SELECT s FROM big ORDER BY s LIMIT 100000 OFFSET 999999")
    s.append("SELECT DISTINCT id, n FROM big ORDER BY id, n")
    s.append("SELECT id * 0 AS k, s FROM big ORDER BY k, s LIMIT 20")
    s.append("SELECT DISTINCT upper(s) AS u FROM big ORDER BY u")
    s.append("SELECT coalesce(n, 999) AS c FROM big ORDER BY c DESC LIMIT 12")
    # no ORDER BY: multiset compare
    s.append("SELECT DISTINCT id FROM big")
    s.append("SELECT DISTINCT s, n FROM big")
    s.append("SELECT id, s FROM big WHERE id % 7 = 0")
    return [st.replace("big", name) for st in s]


def main():
    cand = of.Conn(("127.0.0.1", 5580))
    ref = of.Conn(("ref-sql", 5480))
    rng = random.Random(99)
    fails = 0
    for st in stmts():
        st2 = of.scramble(st, rng)
        ordered = of.has_order_by(st)
        a = cand.ask(st2)
        b = ref.ask(st2)
        err = of.compare(a, b, ordered)
        if err:
            fails += 1
            print("MISMATCH %s\n  stmt: %s\n  cand: %r\n  ref:  %r"
                  % (err, st2, a, b))
    print("FAIL %d" % fails if fails else "PROBES PASS")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
