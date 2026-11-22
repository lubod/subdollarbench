#!/usr/bin/env python3
"""Randomized differential fuzzer for ticket 9: fresh constrained schemas
each round, random DML (valid and violating), candidate vs reference.

Raw sockets only: no SQL engine bindings in the workspace.
"""
import os
import random
import socket
import sys

CAND = ("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 5580)
REF = ("ref-sql", 5480)
ROUNDS = int(sys.argv[2]) if len(sys.argv) > 2 else 30
STMTS = int(sys.argv[3]) if len(sys.argv) > 3 else 60
SEED = int(sys.argv[4]) if len(sys.argv) > 4 else 4242

KWS = ["select", "from", "where", "insert", "into", "values", "create",
       "table", "integer", "text", "primary", "key", "unique", "not",
       "null", "default", "references", "update", "set", "delete", "order",
       "by", "limit", "offset", "and", "or", "as", "is", "distinct"]


class Conn:
    def __init__(self, addr):
        self.sock = socket.create_connection(addr, timeout=30)
        self.file = self.sock.makefile("rb")

    def ask(self, stmt):
        self.sock.sendall((stmt + "\n").encode())
        first = self.file.readline().decode()
        if not first:
            raise RuntimeError("connection closed")
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
            return ("ERR", first.split()[1])
        return ("OK", first.rstrip("\n"))


def scramble(stmt, rng):
    out = stmt
    for kw in KWS:
        low = out.lower()
        idx = low.find(kw)
        if idx >= 0 and rng.random() < 0.3:
            # only scramble whole words outside quotes
            if (idx == 0 or not (out[idx - 1].isalnum() or out[idx - 1] == "_")) \
               and (idx + len(kw) >= len(out)
                    or not (out[idx + len(kw)].isalnum() or out[idx + len(kw)] == "_")):
                repl = kw.upper() if rng.random() < 0.5 else kw
                out = out[:idx] + repl + out[idx + len(kw):]
    return out


def compare(stmt, c, r):
    if c[0] != r[0]:
        return "kind cand=%r ref=%r" % (c, r)
    if c[0] == "ERR":
        if c[1] != r[1]:
            return "class cand=%s ref=%s" % (c[1], r[1])
        return None
    if c[0] == "OK":
        if c[1] != r[1]:
            return "ok cand=%s ref=%s" % (c[1], r[1])
        return None
    if c[1] != r[1]:
        return "header cand=%r ref=%r" % (c[1], r[1])
    if c[3] != r[3]:
        return "count cand=%s ref=%s" % (c[3], r[3])
    ordered = "ORDER BY" in stmt.upper()
    if ordered:
        if c[2] != r[2]:
            return "rows cand=%r ref=%r" % (c[2][:8], r[2][:8])
    else:
        if sorted(c[2]) != sorted(r[2]):
            return "rows cand=%r ref=%r" % (sorted(c[2])[:8], sorted(r[2])[:8])
    return None


def q(s):
    """Quote a text literal."""
    return "'" + s.replace("'", "''") + "'"


def build_schema(rng, sfx):
    """Random constrained schema: returns (ddl, info)."""
    ntab = rng.randint(2, 3)
    names = ["%st%d" % (sfx, i) for i in range(ntab)]
    info = {"tables": [], "names": names}
    ddl = []
    for ti, tname in enumerate(names):
        ncol = rng.randint(2, 5)
        cols = []
        # random column names unique per table
        pool = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]
        rng.shuffle(pool)
        colnames = pool[:ncol]
        pk_idx = rng.choice([None] + list(range(ncol))) if ti == 0 else None
        # parent table (t0) gets a PK; others may get one too
        if ti == 0:
            pk_idx = rng.randint(0, ncol - 1)
        for ci, cn in enumerate(colnames):
            ty = rng.choice(["INTEGER", "TEXT"])
            if ci == pk_idx:
                ty = rng.choice(["INTEGER", "INTEGER", "TEXT"])
            parts = [cn, ty]
            if ci == pk_idx:
                parts.append("PRIMARY KEY")
                if ty == "INTEGER" and rng.random() < 0.4:
                    parts.append("AUTOINCREMENT")
            else:
                r = rng.random()
                if r < 0.2:
                    parts.append("NOT NULL")
                elif r < 0.4:
                    parts.append("UNIQUE")
                elif r < 0.6:
                    if ty == "INTEGER":
                        parts.append("DEFAULT %d" % rng.randint(-3, 9))
                    else:
                        parts.append("DEFAULT %s" % q(rng.choice(["x", "yy", "", "z"])))
            cols.append((cn, ty, parts[2:]))
        # foreign key column on non-parent tables pointing at t0's pk
        fk = None
        if ti > 0 and names:
            t0 = info["tables"][0]
            if t0.get("pk"):
                fkcol = "fk%d" % ti
                if fkcol not in colnames:
                    ty = t0["colty"][t0["pk"]]
                    extra = []
                    if rng.random() < 0.3:
                        extra.append("NOT NULL")
                    if rng.random() < 0.3:
                        extra.append("DEFAULT %d" % rng.randint(1, 3) if ty == "INTEGER"
                                     else "DEFAULT 'q'")
                    extra.append("REFERENCES %s(%s)" % (names[0], t0["pk"]))
                    cols.append((fkcol, ty, extra))
                    fk = (fkcol, ty, names[0], t0["pk"])
        # build properly: cols entries are (name, ty, extra-parts list)
        pieces = []
        for (cn, ty, extra) in cols:
            pieces.append(" ".join([cn, ty] + extra))
        # occasionally a table-level constraint
        if rng.random() < 0.35 and len(cols) >= 2:
            nonpk = [c[0] for c in cols
                     if c[0] != (colnames[pk_idx] if pk_idx is not None else None)]
            if nonpk:
                k = 1 if rng.random() < 0.6 or len(nonpk) < 2 else 2
                sel = rng.sample(nonpk, min(k, len(nonpk)))
                pieces.append("UNIQUE (%s)" % ", ".join(sel))
        if ti > 0 and pk_idx is None and rng.random() < 0.3:
            k = 1 if rng.random() < 0.6 else 2
            sel = rng.sample(colnames, min(k, len(colnames)))
            pieces.append("PRIMARY KEY (%s)" % ", ".join(sel))
        ddl.append("CREATE TABLE %s (%s)" % (tname, ", ".join(pieces)))
        info["tables"].append({
            "name": tname,
            "cols": [c[0] for c in cols],
            "colty": {c[0]: c[1] for c in cols},
            "pk": colnames[pk_idx] if pk_idx is not None else None,
            "notnull": set(), "unique": set(), "default": {}, "fk": fk,
        })
        # recompute constraint sets from the DDL pieces for the generator
        t = info["tables"][-1]
        for (cn, ty, extra) in cols:
            if "NOT NULL" in extra:
                t["notnull"].add(cn)
            if "UNIQUE" in extra:
                t["unique"].add(cn)
            if "DEFAULT" in extra:
                j = extra.index("DEFAULT")
                t["default"][cn] = " ".join(extra[j:])
    return ddl, info


def rand_value(rng, ty, col, t, allow_null=True):
    """A value for a column: sometimes valid, sometimes a violation probe."""
    if allow_null and rng.random() < 0.25:
        return "NULL"
    if ty == "INTEGER":
        # duplicate-friendly: often reuse small values already in play
        r = rng.random()
        if r < 0.3:
            return str(rng.randint(1, 3))
        if r < 0.5:
            return str(rng.randint(-2, 8))
        return str(rng.randint(0, 100))
    r = rng.random()
    if r < 0.3:
        return q(rng.choice(["a", "b", "c"]))
    if r < 0.5:
        return q("")
    return q("v%d" % rng.randint(0, 5))


def gen_insert(rng, info):
    t = rng.choice(info["tables"])
    name = t["name"]
    cols = t["cols"]
    nrow = rng.randint(1, 4)
    use_list = rng.random() < 0.6
    if use_list:
        k = rng.randint(1, len(cols))
        sel = rng.sample(cols, k)
        rows = []
        for _ in range(nrow):
            vals = [rand_value(rng, t["colty"][c], c, t) for c in sel]
            rows.append("(%s)" % ", ".join(vals))
        return "INSERT INTO %s (%s) VALUES %s" % (name, ", ".join(sel), ", ".join(rows))
    rows = []
    for _ in range(nrow):
        vals = [rand_value(rng, t["colty"][c], c, t) for c in cols]
        rows.append("(%s)" % ", ".join(vals))
    return "INSERT INTO %s VALUES %s" % (name, ", ".join(rows))


def gen_update(rng, info):
    t = rng.choice(info["tables"])
    name = t["name"]
    cols = t["cols"]
    k = rng.randint(1, min(3, len(cols)))
    sel = rng.sample(cols, k)
    sets = []
    for c in sel:
        r = rng.random()
        if r < 0.5:
            v = rand_value(rng, t["colty"][c], c, t, allow_null=True)
            sets.append("%s = %s" % (c, v))
        else:
            # reference another column of the same table
            other = rng.choice(cols)
            sets.append("%s = %s" % (c, other))
    stmt = "UPDATE %s SET %s" % (name, ", ".join(sets))
    wr = rng.random()
    if wr < 0.4:
        c = rng.choice(cols)
        v = rand_value(rng, t["colty"][c], c, t)
        stmt += " WHERE %s = %s" % (c, v)
    elif wr < 0.5:
        c = rng.choice(cols)
        stmt += " WHERE %s IS NULL" % c
    if rng.random() < 0.2:
        stmt += " ORDER BY %s LIMIT %d" % (rng.choice(cols), rng.randint(1, 3))
    return stmt


def gen_delete(rng, info):
    t = rng.choice(info["tables"])
    name = t["name"]
    cols = t["cols"]
    stmt = "DELETE FROM %s" % name
    wr = rng.random()
    if wr < 0.5:
        c = rng.choice(cols)
        v = rand_value(rng, t["colty"][c], c, t)
        stmt += " WHERE %s = %s" % (c, v)
    if rng.random() < 0.15:
        stmt += " ORDER BY %s LIMIT 1" % rng.choice(cols)
    return stmt


def gen_select(rng, info):
    t = rng.choice(info["tables"])
    name = t["name"]
    cols = t["cols"]
    if rng.random() < 0.5:
        sel = ", ".join(cols)
    else:
        sel = "*"
    stmt = "SELECT %s FROM %s" % (sel, name)
    ordered = False
    if rng.random() < 0.7:
        stmt += " ORDER BY " + ", ".join(cols)
        ordered = True
    if ordered and rng.random() < 0.4:
        stmt += " LIMIT %d" % rng.randint(1, 5)
    return stmt


def main():
    rng = random.Random(SEED)
    cand = Conn(CAND)
    ref = Conn(REF)
    fails = 0
    total = 0
    for r in range(ROUNDS):
        sfx = "fz9_%s_" % os.urandom(3).hex()
        ddl, info = build_schema(rng, sfx)
        plan = list(ddl)
        for _ in range(STMTS):
            x = rng.random()
            if x < 0.35:
                plan.append(gen_insert(rng, info))
            elif x < 0.6:
                plan.append(gen_update(rng, info))
            elif x < 0.75:
                plan.append(gen_delete(rng, info))
            else:
                plan.append(gen_select(rng, info))
        sent = []
        for s in plan:
            total += 1
            s2 = scramble(s, rng)
            sent.append(s2)
            try:
                c = cand.ask(s2)
                rres = ref.ask(s2)
            except Exception as e:
                print("CRASH on %r: %s" % (s2, e))
                return 1
            diff = compare(s2, c, rres)
            if diff:
                fails += 1
                with open("/tmp/fz9_sent.txt", "w") as fh:
                    for d in sent:
                        fh.write(d + "\n")
                if fails == 1:
                    for d in sent:
                        print("  prior: %s" % d)
                print("MISMATCH [%s]\n  stmt: %s\n  cand: %r\n  ref : %r\n  why : %s"
                      % (sfx, s2, c, rres, diff))
                if fails > 12:
                    print("too many failures")
                    return 1
                break  # state diverged: rest of the round is noise
    print("checked %d statements (%d rounds), %d mismatches"
          % (total, ROUNDS, fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
