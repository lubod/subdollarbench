#!/usr/bin/env python3
"""Ticket-10 differential fuzzer: subqueries in every form the acceptance
generator writes, compared against the reference (raw sockets only)."""
import random
import socket
import sys
import time

RUN = str(random.randint(1000, 9999)) + str(int(time.time()) % 1000)

CAND = ("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 5580)
REF = ("ref-sql", 5480)
ROUNDS = int(sys.argv[2]) if len(sys.argv) > 2 else 5
SEED0 = int(sys.argv[3]) if len(sys.argv) > 3 else random.randint(1000, 99999)


def run(addr, stmts):
    s = socket.create_connection(addr, timeout=30)
    f = s.makefile("rwb")
    out = []
    for st in stmts:
        f.write(st.encode() + b"\n")
        f.flush()
        line = f.readline().decode()
        if line.startswith("OK") or line.startswith("ERR"):
            out.append(line.rstrip("\n"))
        elif line.startswith("ROWS"):
            block = [line.rstrip("\n")]
            while True:
                l = f.readline().decode().rstrip("\n")
                block.append(l)
                if l.startswith("END"):
                    break
            out.append("\n".join(block))
        elif line == "":
            out.append("<EOF>")
            break
        else:
            out.append("? " + line)
    while len(out) < len(stmts):
        out.append("<MISSING>")
    s.close()
    return out


def norm(a):
    if a.startswith("ERR"):
        return "ERR " + a.split(" ", 2)[1]
    return a


def rows_of(ans):
    if not ans.startswith("ROWS"):
        return None
    return ans.split("\n")[1:-1]


def compare(st, a, b, ordered):
    na, nb = norm(a), norm(b)
    if na != nb:
        return f"DIFF class/ok\n  ref: {a!r}\n  cnd: {b!r}"
    if na.startswith("ROWS"):
        ra, rb = rows_of(a) or [], rows_of(b) or []
        if ordered:
            if ra != rb:
                return f"DIFF order\n  ref: {ra}\n  cnd: {rb}"
        else:
            if sorted(ra) != sorted(rb):
                return f"DIFF multiset\n  ref: {sorted(ra)}\n  cnd: {sorted(rb)}"
    return None


def gen(rng, n, sfx):
    """Return (statements, ordered_flags)."""
    tabs = []
    for i in range(rng.randint(3, 4)):
        name = f"f10t{sfx}_{i}"
        cols = []
        ncols = rng.randint(2, 4)
        for c in range(ncols):
            cname = "k" if (c == 0 and i > 0) else f"c{c}"
            # first column of table 0 stays k0 to keep joins simple
            if i == 0 and c == 0:
                cname = "k0"
            cols.append((cname, rng.choice(["INTEGER", "TEXT"])))
        tabs.append((name, cols))
    stmts = []
    ordered = []
    for (name, cols) in tabs:
        stmts.append(f"CREATE TABLE {name} ({', '.join(f'{c} {t}' for c, t in cols)})")
        ordered.append(False)
    # rows: anchored constants per table
    values = {}
    for (name, cols) in tabs:
        rows = []
        for r in range(rng.randint(4, 9)):
            row = []
            for (c, t) in cols:
                if t == "INTEGER":
                    v = str(rng.randint(0, 6))
                else:
                    v = rng.choice(["'a'", "'b'", "'c'", "NULL", "''"])
                row.append(v)
            rows.append("(" + ", ".join(row) + ")")
            # collect anchored ints for k-like columns
        values[name] = rows
        stmts.append(f"INSERT INTO {name} VALUES {', '.join(rows)}")
        ordered.append(False)
    t0 = tabs[0][0]
    t0cols = [c for c, _ in tabs[0][1]]
    others = [t for (t, _) in tabs[1:]] or [t0]

    for _ in range(n):
        kind = rng.choice([
            "scalar_item", "scalar_item", "scalar_cmp", "in_sub", "notin_sub",
            "in_list", "exists", "notexists", "insert_select", "scalar_item",
            "in_sub", "exists", "probe", "exists", "in_list", "scalar_cmp",
        ])
        outer = rng.choice([t0] + others)
        ocols = dict(tabs)[outer]
        oc = rng.choice(ocols)[0] if rng.random() < 0.5 else rng.choice(ocols)[0]
        # pick an outer int-ish column if possible
        intcols = [c for c, t in ocols if t == "INTEGER"]
        textcols = [c for c, t in ocols if t == "TEXT"]
        oc_int = rng.choice(intcols) if intcols else oc
        oc_txt = rng.choice(textcols) if textcols else oc
        inner = rng.choice(others)
        icols = dict(tabs)[inner]
        i_int = [c for c, t in icols if t == "INTEGER"]
        i_txt = [c for c, t in icols if t == "TEXT"]
        anchor = rng.randint(0, 6)

        if kind == "scalar_item":
            ic = (rng.choice(i_int) if i_int else rng.choice(icols)[0])
            agg = rng.choice(["COUNT(*)", f"COUNT({ic})", f"MAX({ic})", f"MIN({ic})"])
            if i_int and rng.random() < 0.4:
                agg = f"SUM({rng.choice(i_int)})"  # SUM takes INTEGER args
            arg = agg
            corr = rng.random() < 0.5
            w = ""
            if corr and intcols and i_int:
                # unqualified innermost-first: inner col with same name if any
                eq = rng.choice([f"{rng.choice(i_int)} = {rng.choice(intcols)}",
                                 f"{inner}.{rng.choice(i_int)} = {outer}.{rng.choice(intcols)}"])
                w = f" WHERE {eq}"
            stmts.append(f"SELECT {oc_int} AS n, (SELECT {arg} FROM {inner}{w}) AS s FROM {outer} ORDER BY n")
            ordered.append(True)
        elif kind == "scalar_cmp":
            ic = rng.choice(i_int) if i_int else rng.choice(icols)[0]
            variants = [f"(SELECT MAX({ic}) FROM {inner})",
                        f"(SELECT MIN({ic}) FROM {inner})",
                        f"(SELECT COUNT(*) FROM {inner})"]
            if i_int:
                variants.append(f"(SELECT SUM({rng.choice(i_int)}) FROM {inner})")
            agg = rng.choice(variants)
            op = rng.choice(["=", "<>", "<", "<=", ">", ">="])
            stmts.append(f"SELECT {oc_int} AS a FROM {outer} WHERE {agg} {op} {anchor} ORDER BY a")
            ordered.append(True)
        elif kind in ("in_sub", "notin_sub"):
            ic = rng.choice(i_int) if i_int else oc_int
            neg = "NOT " if kind == "notin_sub" else ""
            # left side must be INTEGER vs INTEGER; use a constant anchored int
            # or an integer column of the outer table
            if intcols and rng.random() < 0.7:
                left = rng.choice(intcols)
                if textcols and rng.random() < 0.3:
                    left = rng.choice(textcols)
                    ic = rng.choice(i_txt) if i_txt else ic
            else:
                left = str(anchor)
            stmts.append(
                f"SELECT {oc_int} AS a FROM {outer} WHERE {left} {neg}IN (SELECT {ic} FROM {inner}) ORDER BY a"
            )
            ordered.append(True)
        elif kind == "in_list":
            nvals = rng.randint(1, 3)
            vals = []
            for _ in range(nvals):
                vals.append(rng.choice([str(rng.randint(0, 6)), "NULL",
                                        f"'{rng.choice(['a', 'b', 'zz'])}'"]))
            neg = "NOT " if rng.random() < 0.4 else ""
            left = rng.choice(intcols) if intcols and rng.random() < 0.6 else str(rng.randint(0, 6))
            if rng.random() < 0.3:
                left = rng.choice(textcols) if textcols else left
            stmts.append(
                f"SELECT {oc_int} AS a FROM {outer} WHERE {left} {neg}IN ({', '.join(vals)}) ORDER BY a"
            )
            ordered.append(True)
        elif kind in ("exists", "notexists"):
            neg = "NOT " if kind == "notexists" else ""
            ic = rng.choice(i_int) if i_int else rng.choice(icols)[0]
            ocx = rng.choice(intcols) if intcols else oc_int
            eq = rng.choice([
                f"{inner}.{ic} = {outer}.{ocx}",
                f"{ic} = {ocx}",
                f"{ic} = {anchor}",
            ])
            stmts.append(
                f"SELECT {oc_int} AS a FROM {outer} WHERE {neg}EXISTS (SELECT 1 FROM {inner} WHERE {eq}) ORDER BY a"
            )
            ordered.append(True)
        elif kind == "insert_select":
            tgt = rng.choice(tabs)
            # select list matches target types
            picks = []
            ok = True
            for (_, tt) in tgt[1]:
                pool = i_int if tt == "INTEGER" else i_txt
                if not pool:
                    ok = False
                    break
                picks.append(rng.choice(pool))
            if not ok:
                picks = None
            if picks:
                stmts.append(f"INSERT INTO {tgt[0]} SELECT {', '.join(picks)} FROM {inner} WHERE {rng.choice(i_int)} IS NOT NULL"
                             if i_int else
                             f"INSERT INTO {tgt[0]} SELECT {', '.join(picks)} FROM {inner}")
                ordered.append(False)
                stmts.append(f"SELECT COUNT(*) AS n FROM {tgt[0]}")
                ordered.append(False)
        elif kind == "probe":
            probes = [
                f"SELECT 1 AS x FROM {outer} WHERE EXISTS (SELECT 1 FROM nope{sfx})",
                f"SELECT 1 AS x FROM {outer} WHERE EXISTS (SELECT 1 FROM {inner} WHERE zz = 1)",
                f"SELECT (SELECT zz FROM {inner} LIMIT 1) AS c FROM {outer}",
                f"SELECT (SELECT {', '.join(c for c, _ in icols)} FROM {inner}) AS c FROM {outer}",
                f"SELECT {rng.choice([oc_int, 'zz'])} AS a FROM {outer} WHERE {rng.choice(['1 = 1', 'k0 = 2'])} ORDER BY a",
            ]
            stmts.append(rng.choice(probes))
            ordered.append(True)
    return stmts, ordered


def main():
    fails = 0
    total = 0
    for r in range(ROUNDS):
        rng = random.Random(SEED0 + r)
        sfx = RUN + str(rng.randint(1000, 9999))
        stmts, ordered = gen(rng, 60, sfx)
        # setup on both: create/insert must agree
        pre_r = run(REF, stmts)
        pre_c = run(CAND, stmts)
        for st, o, a, b in zip(stmts, ordered, pre_r, pre_c):
            total += 1
            bad = compare(st, a, b, o)
            if bad:
                fails += 1
                print(f"[round {r}] {st}\n{bad}")
    print(f"\n{total - fails}/{total} statements match over {ROUNDS} rounds")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
