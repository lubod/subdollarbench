//! The sdb-sql reference adapter: a stdlib-only Python server backed by its
//! own `sqlite3` (allowed for the reference only, docs/PROPOSAL-sdb-sql.md,
//! "Reference adapter") speaking the text protocol.
//!
//! It proves the verifier end to end, not solvability. [`write_reference`]
//! drops the server and a `start.sh` into a workspace, exactly like the
//! sdb-http mock: the sandbox starts `start.sh` with no arguments, so the
//! port is baked in.
//!
//! The reference translates DDL like the Rust oracle (append `STRICT`,
//! a TEXT primary key becomes `NOT NULL`), maps SQLite messages to the
//! dialect's error classes, and reports `OK <changed>` from
//! `total_changes` deltas, so DDL/txn statements answer `OK 0` after DML.

/// The reference server (stdlib `sqlite3`, threads, one DB file shared by
/// all connections so multi-client scenarios see each other's rows).
pub const SQL_REF_SERVER_PY: &str = r#"# sdb-sql reference server: the text protocol over its own sqlite3.
# Reference only (docs/PROPOSAL-sdb-sql.md): it proves the harness end to
# end, it is not a candidate. STRICT tables, INTEGER and TEXT only.
import base64
import os
import re
import socket
import sqlite3
import struct
import sys
import threading
import time


def mask_quoted(s):
    out = []
    in_q = False
    for ch in s:
        if ch == "'":
            in_q = not in_q
            out.append(ch)
        elif in_q:
            out.append(" ")
        else:
            out.append(ch)
    return "".join(out)


def split_top_level(body):
    out = []
    cur = []
    depth = 0
    in_q = False
    for ch in body:
        if ch == "'":
            in_q = not in_q
            cur.append(ch)
        elif ch == "(" and not in_q:
            depth += 1
            cur.append(ch)
        elif ch == ")" and not in_q:
            depth -= 1
            cur.append(ch)
        elif ch == "," and depth == 0 and not in_q:
            out.append("".join(cur).strip())
            cur = []
        else:
            cur.append(ch)
    if "".join(cur).strip():
        out.append("".join(cur).strip())
    return out


def has_word(masked_upper, word):
    return word in re.split(r"[^A-Z0-9_]+", masked_upper)


def translate_ddl(sql):
    stmt = sql.strip().rstrip(";").rstrip()
    # Keyword case and whitespace (spaces, tabs) are randomized.
    if not re.match(r"\s*CREATE\s+TABLE\b", stmt, re.I):
        return stmt
    masked = mask_quoted(stmt)
    open_i = masked.find("(")
    if open_i < 0:
        return stmt
    depth = 0
    close_i = -1
    for i in range(open_i, len(masked)):
        if masked[i] == "(":
            depth += 1
        elif masked[i] == ")":
            depth -= 1
            if depth == 0:
                close_i = i
                break
    if close_i < 0:
        return stmt
    head = stmt[:open_i]
    body = stmt[open_i + 1:close_i]
    tail = stmt[close_i + 1:].strip()
    if has_word(mask_quoted(tail).upper(), "STRICT"):
        return stmt
    segments = split_top_level(body)
    table_pk = []
    for seg in segments:
        mu = mask_quoted(seg).upper().strip()
        if mu.startswith("PRIMARY KEY") and "(" in seg and ")" in seg:
            inner = seg[seg.find("(") + 1:seg.rfind(")")]
            table_pk += [n.strip().lower() for n in inner.split(",")]
    cols = []
    for seg in segments:
        mu = mask_quoted(seg).upper()
        toks = mu.split()
        first = toks[0] if toks else ""
        if first in ("PRIMARY", "UNIQUE", "FOREIGN", "CHECK", "CONSTRAINT"):
            cols.append(seg)
            continue
        type_is_text = len(toks) > 1 and toks[1] == "TEXT"
        is_pk = "PRIMARY KEY" in mu or first.lower() in table_pk
        if is_pk and type_is_text and "NOT NULL" not in mu:
            seg = seg + " NOT NULL"
        cols.append(seg)
    options = "STRICT" if not tail else "STRICT, " + tail
    return head + "(" + ", ".join(cols) + ") " + options


# Ticket 14: exact service counters for SHOW STATS. One lock for the whole
# server; each connection thread bumps it.
STATS_LOCK = threading.Lock()
STATS = {
    "statements": 0,
    "selects": 0,
    "inserts": 0,
    "updates": 0,
    "deletes": 0,
    "commits": 0,
    "rollbacks": 0,
    "errors": 0,
    "connections_current": 0,
    "connections_total": 0,
}

# Ticket 18: hostile input and limits. A password is optional (the
# differential suites run without one); the limits apply always.
PASSWORD = os.environ.get("SDB_SQL_PASSWORD", "")
MAX_STMT = 16 * 1024 * 1024
MAX_DEPTH = 10000
MAX_CONNS = 256
# Ticket 18's statement budget is opt-in: the security restart sets
# SDB_SQL_STATEMENT_TIMEOUT_MS; the cumulative suites (including ticket
# 16's 400 MiB sort and ticket 11's index build) run without it.
STMT_TIMEOUT = 0.0
CONN_LOCK = threading.Lock()
CONNS_CURRENT = 0

# Ticket 19: replication. NODES is the per-cluster node list (Docker
# aliases) from SDB_SQL_NODES / --nodes; the data dir holds the primary's
# retained log, which a promoted replica keeps serving.
NODES = []
ROLE = "primary"
LAST_LSN = 0
ACK_MAX = 0
ACK_LOCK = threading.Lock()
LOG_LOCK = threading.Lock()
SKIP_SYNC = False
PORT = 0
DATA = "/tmp"
PRIMARY = None
WRITE_KEYWORDS = ("INSERT", "UPDATE", "DELETE", "REPLACE", "CREATE", "DROP", "ALTER")
LOG_KEYWORDS = WRITE_KEYWORDS + ("BEGIN", "COMMIT", "ROLLBACK")

# Ticket 20: PostgreSQL wire protocol v3 on --pg-port.
PG_PORT = 0
SQLSTATE = {
    "syntax": "42601", "no_table": "42P01", "no_column": "42703",
    "ambiguous": "42702", "exists": "42P07", "arity": "42601",
    "type": "42804", "constraint": "23505", "txn": "25P02",
    "busy": "40001", "limit": "54000", "full": "53100",
    "readonly": "25006", "auth": "28P01",
}


def stats_bump(sql):
    if not sql.strip():
        return
    first = sql.split(None, 1)[0].upper()
    kind = {
        "SELECT": "selects",
        "INSERT": "inserts",
        "UPDATE": "updates",
        "DELETE": "deletes",
        "COMMIT": "commits",
        "ROLLBACK": "rollbacks",
    }.get(first)
    with STATS_LOCK:
        STATS["statements"] += 1
        if kind:
            STATS[kind] += 1


def stats_error():
    with STATS_LOCK:
        STATS["errors"] += 1


def classify(msg):
    if msg.startswith("no such table") or msg.startswith("no such index"):
        return "no_table"
    if (msg.startswith("no such column")
            or msg.startswith("no such function")
            or "has no column named" in msg):
        return "no_column"
    if msg.startswith("ambiguous column name"):
        return "ambiguous"
    if ("duplicate column name" in msg
            or "already another table or index" in msg):
        return "exists"
    if "already exists" in msg:
        return "exists"
    if msg.startswith("datatype mismatch") or "cannot store" in msg:
        return "type"
    if "constraint failed" in msg:
        return "constraint"
    if ("cannot drop column" in msg
            or "after drop column" in msg
            or "cannot drop PRIMARY KEY column" in msg
            or "cannot drop UNIQUE column" in msg
            or "cannot drop unique column" in msg):
        return "constraint"
    if (msg.startswith("syntax error")
            or msg.startswith("near ")
            or msg.startswith("unrecognized token")
            or msg.startswith("incomplete input")
            or msg.startswith("misuse of aggregate")
            or "aggregate functions are not allowed" in msg
            or msg.startswith("the query contains a null character")):
        return "syntax"
    if ("wrong number of arguments" in msg
            or (" values for " in msg and " columns" in msg)
            or (" columns but " in msg and "values were supplied" in msg)
            or "do not have the same number of result columns" in msg
            or msg.startswith("sub-select returns")):
        return "arity"
    if ("cannot start a transaction within a transaction" in msg
            or "cannot commit - no transaction is active" in msg
            or "cannot rollback - no transaction is active" in msg):
        return "txn"
    if "database is locked" in msg or "database table is locked" in msg:
        return "busy"
    if "database or disk is full" in msg:
        return "full"
    if "attempt to write a readonly database" in msg:
        return "readonly"
    if "interrupted" in msg:
        return "limit"
    if "too many" in msg:
        return "limit"
    return None


def encode_cell(v):
    if v is None:
        return "\\N"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, str):
        return v.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
    raise ValueError("REAL/BLOB outside the dialect: %r" % (v,))


def rewrite_alter_if_exists(conn, sql):
    # SQLite has no IF [NOT] EXISTS on ALTER; the dialect does (ticket 13).
    # The generator sometimes ends a statement with ';', and the oracle
    # trims it before parsing; without the same trim a DROP's last token
    # would be "col;" and the existence check would miss.
    stripped = sql.strip().rstrip(";").strip()
    toks = stripped.split()
    if len(toks) < 5 or toks[0].lower() != "alter" or toks[1].lower() != "table":
        return sql

    def table_exists():
        try:
            return conn.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ? COLLATE NOCASE",
                (toks[2],)).fetchone() is not None
        except Exception:
            return False

    def column_exists(name):
        try:
            return conn.execute(
                "SELECT 1 FROM pragma_table_info(?) WHERE name = ? COLLATE NOCASE",
                (toks[2], name)).fetchone() is not None
        except Exception:
            return False

    def kw(j):
        return toks[j].lower() if j < len(toks) else ""

    i = 3
    if kw(i) == "add":
        i += 1
        if kw(i) == "column":
            i += 1
        if (kw(i), kw(i + 1), kw(i + 2)) != ("if", "not", "exists") or i + 3 >= len(toks):
            return sql
        name = toks[i + 3]
        out = "ALTER TABLE %s ADD COLUMN %s" % (toks[2], " ".join(toks[i + 3:]))
        if not table_exists():
            return out
        return None if column_exists(name) else out
    if kw(i) == "drop":
        i += 1
        if kw(i) == "column":
            i += 1
        if (kw(i), kw(i + 1)) != ("if", "exists") or i + 2 >= len(toks):
            return sql
        name = toks[i + 2]
        out = "ALTER TABLE %s DROP COLUMN %s" % (toks[2], name)
        if not table_exists():
            return out
        return None if not column_exists(name) else out
    return sql


def answer(conn, sql):
    rewritten = rewrite_alter_if_exists(conn, sql)
    if rewritten is None:
        return b"OK 0\n"
    sql = rewritten
    cur = conn.cursor()
    first = sql.split(None, 1)[0].upper() if sql.split() else ""
    if first == "SHOW":
        parts = sql.split(None, 1)
        if len(parts) < 2 or parts[1].strip().upper() != "STATS":
            raise ValueError("syntax error")
        with STATS_LOCK:
            items = sorted(STATS.items())
        lines = ["ROWS 2", "name\tvalue"]
        lines.extend("%s\t%d" % (k, v) for k, v in items)
        lines.append("END %d" % len(items))
        return ("\n".join(lines) + "\n").encode()
    if first == "EXPLAIN":
        parts = sql.split(None, 1)
        if len(parts) < 2:
            raise ValueError("syntax error")
        # The dialect's plan names INDEX <table>.<index> or SCAN <table>;
        # SQLite's planner supplies the decision.
        cur.execute("EXPLAIN QUERY PLAN " + parts[1])
        plan = []
        for row in cur.fetchall():
            detail = str(row[-1])
            m = re.match(r"SEARCH (\S+) USING (?:COVERING )?INDEX (\S+)", detail, re.I)
            if m:
                plan.append("INDEX %s.%s" % (m.group(1), m.group(2)))
            else:
                plan.append(detail)
        lines = ["ROWS 1", "plan"]
        lines.extend(plan)
        lines.append("END %d" % len(plan))
        return ("\n".join(lines) + "\n").encode()
    before = conn.total_changes
    # The dialect's statement atomicity: a failing statement changes
    # nothing. The host's SQLite (3.45) keeps the rows a failed multi-row
    # VALUES inserted before the bad one, inside a transaction and in
    # autocommit alike (3.46 fixed the autocommit case only), so a
    # statement savepoint enforces the spec in both modes.
    guard = None
    if re.match(r"\s*(INSERT|UPDATE|DELETE|REPLACE)\b", sql, re.I):
        guard = "sdb_stmt"
        conn.execute("SAVEPOINT sdb_stmt")
    try:
        cur.execute(translate_ddl(sql))
    except Exception:
        if guard:
            try:
                conn.execute("ROLLBACK TO %s" % guard)
                conn.execute("RELEASE %s" % guard)
            except Exception:
                pass
        raise
    if guard:
        conn.execute("RELEASE %s" % guard)
    # ALTER TABLE on a STRICT table runs a nested check query and so has a
    # description; it is still DDL (same rule as the Rust oracle).
    if cur.description is not None and not re.match(r"\s*ALTER\b", sql, re.I):
        headers = [d[0] for d in cur.description]
        rows = cur.fetchall()
        lines = ["ROWS %d" % len(headers), "\t".join(headers)]
        for r in rows:
            lines.append("\t".join(encode_cell(v) for v in r))
        lines.append("END %d" % len(rows))
        return ("\n".join(lines) + "\n").encode()
    changed = conn.total_changes - before
    n = cur.rowcount if changed else 0
    if n < 0:
        n = 0
    return ("OK %d\n" % n).encode()


def nesting_too_deep(sql):
    depth = 0
    in_q = False
    i = 0
    while i < len(sql):
        ch = sql[i]
        if ch == "'":
            if in_q and i + 1 < len(sql) and sql[i + 1] == "'":
                i += 2
                continue
            in_q = not in_q
        elif not in_q:
            if ch == "(":
                depth += 1
                if depth >= MAX_DEPTH:
                    return True
            elif ch == ")":
                depth -= 1
        i += 1
    return False


def read_line_capped(f):
    """One line, at most MAX_STMT+2 bytes; drain an oversized line."""
    raw = f.readline(MAX_STMT + 2)
    if not raw:
        return None, False
    # The statement is the line without its trailing newline. `readline`
    # can overshoot the requested size by its final chunk, so compare the
    # actual length rather than trusting the limit.
    if len(raw) > MAX_STMT + 1:
        if not raw.endswith(b"\n"):
            while True:
                chunk = f.readline(65536)
                if not chunk or chunk.endswith(b"\n"):
                    break
        return raw, True
    return raw, False


def timed_answer(conn, sql):
    if STMT_TIMEOUT <= 0:
        return answer(conn, sql)
    start = time.monotonic()

    def progress():
        return 1 if time.monotonic() - start > STMT_TIMEOUT else 0

    conn.set_progress_handler(progress, 1000)
    try:
        return answer(conn, sql)
    finally:
        conn.set_progress_handler(None, 0)


def first_keyword(sql):
    parts = sql.strip().split(None, 1)
    # The dialect permits an optional trailing semicolon; classification
    # (logging, read-only writes, command tags) must see the bare keyword.
    return parts[0].upper().rstrip(";") if parts else ""


def role_reply():
    return ("ROWS 2\nname\tvalue\nrole\t%s\nlsn\t%d\nEND 2\n" % (ROLE, LAST_LSN)).encode()


def load_log(data):
    global LAST_LSN
    path = os.path.join(data, "repl.log")
    last = 0
    if os.path.exists(path):
        with open(path, "rb") as f:
            for line in f:
                try:
                    last = max(last, int(line.split(b"\t", 1)[0]))
                except Exception:
                    pass
    LAST_LSN = last


def append_log(data, sql):
    global LAST_LSN
    with LOG_LOCK:
        LAST_LSN += 1
        append_log_at_locked(data, LAST_LSN, sql)
        return LAST_LSN


def append_log_at(data, lsn, sql):
    global LAST_LSN
    with LOG_LOCK:
        append_log_at_locked(data, lsn, sql)
        if lsn > LAST_LSN:
            LAST_LSN = lsn


def append_log_at_locked(data, lsn, sql):
    entry = "%d\t%s\n" % (lsn, base64.b64encode(sql.encode()).decode())
    with open(os.path.join(data, "repl.log"), "ab") as f:
        f.write(entry.encode())
        f.flush()
        os.fsync(f.fileno())


def log_since(data, want):
    path = os.path.join(data, "repl.log")
    out = []
    last = 0
    if os.path.exists(path):
        with open(path, "rb") as f:
            for line in f:
                try:
                    lsn_s, b64 = line.rstrip(b"\n").split(b"\t", 1)
                    lsn = int(lsn_s)
                except Exception:
                    continue
                last = max(last, lsn)
                if lsn > want:
                    out.append((lsn, base64.b64decode(b64).decode()))
    return out, last


def wait_ack(lsn):
    # The replica may be applying a batch from the cumulative suite; give
    # the sync ack room without making a dead replica stall a commit for
    # long (the harness's statement timeout is 10 s).
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        with ACK_LOCK:
            if ACK_MAX >= lsn:
                return True
        time.sleep(0.005)
    return False


def peer_status(addr):
    try:
        host, port_s = addr.rsplit(":", 1)
        s = socket.create_connection((host, int(port_s)), 0.6)
        s.settimeout(0.6)
        s.sendall(b"SHOW ROLE\n")
        data = b""
        while b"END " not in data:
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
        s.close()
        lines = data.decode("utf-8", "replace").splitlines()
        role = None
        lsn = 0
        for line in lines[2:]:
            if line.startswith("role\t"):
                role = line.split("\t", 1)[1]
            elif line.startswith("lsn\t"):
                lsn = int(line.split("\t", 1)[1])
        return (role, lsn)
    except Exception:
        return None


def try_elect():
    global ROLE, PRIMARY
    if ROLE != "replica":
        return
    peers = []
    for node in NODES:
        try:
            port = int(node.rsplit(":", 1)[1])
        except Exception:
            continue
        if port == PORT:
            continue
        st = peer_status(node)
        if st is None:
            continue
        if st[0] == "primary":
            PRIMARY = node
            return
        peers.append((st[1], port))
    peers.append((LAST_LSN, PORT))
    best = max(peers, key=lambda x: (x[0], -x[1]))
    if best[1] != PORT:
        return
    # A peer may have won while we waited; follow it instead of splitting.
    time.sleep(0.3)
    for node in NODES:
        try:
            port = int(node.rsplit(":", 1)[1])
        except Exception:
            continue
        if port == PORT:
            continue
        st = peer_status(node)
        if st is not None and st[0] == "primary":
            PRIMARY = node
            return
    ROLE = "primary"
    print("sdb-sql reference: node %d promoted" % PORT, file=sys.stderr, flush=True)


def replica_loop(data, start_primary):
    global ROLE, LAST_LSN, PRIMARY
    PRIMARY = start_primary
    db_path = os.path.join(data, "sdb-sql-ref-%d.sqlite" % PORT)
    conn = None
    last_ok = time.monotonic()
    while ROLE == "replica":
        try:
            host, port_s = PRIMARY.rsplit(":", 1)
            s = socket.create_connection((host, int(port_s)), 1.0)
            s.settimeout(5.0)
            f = s.makefile("rwb")
            last_ok = time.monotonic()
            while ROLE == "replica":
                f.write(("SDB_REPL %d\n" % LAST_LSN).encode())
                f.flush()
                entries = []
                primary_last = LAST_LSN
                while True:
                    line = f.readline()
                    if not line:
                        raise EOFError("primary closed")
                    line = line.decode("utf-8", "replace").rstrip("\n")
                    if line.startswith("RLOG "):
                        _, lsn_s, b64 = line.split(" ", 2)
                        entries.append((int(lsn_s), base64.b64decode(b64).decode()))
                    elif line.startswith("REND "):
                        primary_last = int(line.split(" ", 1)[1])
                        break
                if primary_last < LAST_LSN and not entries:
                    # Diverged: rebuild from the leader's retained history.
                    if conn:
                        try:
                            conn.close()
                        except Exception:
                            pass
                        conn = None
                    for suffix in ("", "-wal", "-shm"):
                        try:
                            os.remove(db_path + suffix)
                        except OSError:
                            pass
                    try:
                        os.remove(os.path.join(data, "repl.log"))
                    except OSError:
                        pass
                    LAST_LSN = 0
                    continue
                if entries:
                    if conn is None:
                        conn = sqlite3.connect(db_path, isolation_level=None)
                        conn.execute("PRAGMA journal_mode=WAL")
                        conn.execute("PRAGMA synchronous=FULL")
                    for idx, (lsn, entry_sql) in enumerate(entries):
                        # Replay through the same `answer` path as a client:
                        # ALTER ... IF [NOT] EXISTS and statement atomicity
                        # are handled there, not by translate_ddl alone.
                        try:
                            answer(conn, entry_sql)
                        except Exception as e:
                            print("sdb-sql reference: replica apply failed at lsn %d: %r entry=%r"
                                  % (lsn, e, entry_sql[:200]), file=sys.stderr, flush=True)
                            for plsn, psql in entries[max(0, idx - 6):idx + 1]:
                                print("  log %d %r" % (plsn, psql[:160]),
                                      file=sys.stderr, flush=True)
                            try:
                                with open(os.path.join(data, "repl.log"), "rb") as lf:
                                    tail = lf.readlines()[-12:]
                                for line in tail:
                                    ls, b64 = line.rstrip(b"\n").split(b"\t", 1)
                                    print("  local %s %r" % (
                                        ls.decode(),
                                        base64.b64decode(b64)[:100],
                                    ), file=sys.stderr, flush=True)
                            except Exception:
                                pass
                            raise
                        append_log_at(data, lsn, entry_sql)
                f.write(("SDB_ACK %d\n" % LAST_LSN).encode())
                f.flush()
                f.readline()
                last_ok = time.monotonic()
                time.sleep(0.05)
            try:
                f.close()
                s.close()
            except Exception:
                pass
        except Exception:
            if conn:
                try:
                    conn.close()
                except Exception:
                    pass
                conn = None
            # The harness freezes a replica for about 1 s during the
            # sync-commit probe; 3 s leaves that comfortable margin while
            # staying inside the 10 s failover budget (round-17 J1).
            if ROLE == "replica" and time.monotonic() - last_ok > 3.0:
                try_elect()
            time.sleep(0.2)


def pg_send(f, typ, body):
    f.write(typ + struct.pack("!i", len(body) + 4) + body)
    f.flush()


def pg_error(f, code, msg):
    clean = msg.replace("\n", " ").encode("utf-8", "replace")
    body = b"SERROR\0VERROR\0C" + code.encode() + b"\0M" + clean + b"\0\0"
    pg_send(f, b"E", body)


def pg_rowdesc(f, headers):
    body = struct.pack("!h", len(headers))
    for h in headers:
        body += h.encode("utf-8", "replace") + b"\0"
        body += struct.pack("!i", 0)          # table OID
        body += struct.pack("!h", 0)          # column number
        body += struct.pack("!i", 25)         # text
        body += struct.pack("!h", -1)         # type size
        body += struct.pack("!i", -1)         # type modifier
        body += struct.pack("!h", 0)          # text format
    pg_send(f, b"T", body)


def pg_datarow(f, values):
    body = struct.pack("!h", len(values))
    for v in values:
        if v is None:
            body += struct.pack("!i", -1)
        else:
            b = str(v).encode("utf-8", "replace")
            body += struct.pack("!i", len(b)) + b
    pg_send(f, b"D", body)


def pg_complete(f, tag):
    pg_send(f, b"C", tag.encode() + b"\0")


def pg_ready(f, status):
    pg_send(f, b"Z", status)


def sqlstate(msg):
    cls = classify(msg) or "syntax"
    return SQLSTATE.get(cls, "XX000")


def pg_statement(conn, sql):
    before = conn.total_changes
    guard = None
    if re.match(r"\s*(INSERT|UPDATE|DELETE|REPLACE)\b", sql, re.I):
        guard = "sdb_pg"
        conn.execute("SAVEPOINT sdb_pg")
    try:
        cur = conn.execute(translate_ddl(sql))
    except Exception:
        if guard:
            try:
                conn.execute("ROLLBACK TO sdb_pg")
                conn.execute("RELEASE sdb_pg")
            except Exception:
                pass
        raise
    if guard:
        conn.execute("RELEASE sdb_pg")
    if cur.description is not None and not re.match(r"\s*ALTER\b", sql, re.I):
        headers = [d[0] for d in cur.description]
        rows = cur.fetchall()
        return ("rows", headers, rows)
    changed = conn.total_changes - before
    n = cur.rowcount if changed else 0
    if n < 0:
        n = 0
    first = first_keyword(sql)
    tag = {
        "INSERT": "INSERT 0 %d" % n,
        "UPDATE": "UPDATE %d" % n,
        "DELETE": "DELETE %d" % n,
        "CREATE": "CREATE TABLE",
        "DROP": "DROP TABLE",
        "ALTER": "ALTER TABLE",
        "BEGIN": "BEGIN",
        "COMMIT": "COMMIT",
        "ROLLBACK": "ROLLBACK",
    }.get(first, "OK")
    return ("cmd", tag)


def pg_read_msg(f):
    typ = f.read(1)
    if not typ:
        return None
    ln = f.read(4)
    if len(ln) < 4:
        return None
    n = struct.unpack("!i", ln)[0]
    body = f.read(max(0, n - 4))
    return typ, body


def pg_send_result(f, kind):
    if kind[0] == "rows":
        pg_rowdesc(f, kind[1])
        for row in kind[2]:
            pg_datarow(f, row)
        pg_complete(f, "SELECT %d" % len(kind[2]))
    else:
        pg_complete(f, kind[1])


def pg_serve(sock, db_path):
    conn = sqlite3.connect(db_path, isolation_level=None)
    conn.execute("PRAGMA foreign_keys=ON")
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=FULL")
    f = sock.makefile("rwb")
    try:
        hdr = f.read(8)
        if len(hdr) < 8:
            return
        length, code = struct.unpack("!ii", hdr)
        if length == 8 and code == 80877103:
            f.write(b"N")
            f.flush()
            hdr = f.read(8)
            if len(hdr) < 8:
                return
            length, code = struct.unpack("!ii", hdr)
        body = f.read(max(0, length - 8))
        if code != 196608:
            pg_error(f, "08P01", "unsupported frontend protocol")
            return
        if PASSWORD:
            pg_send(f, b"R", struct.pack("!i", 3))
            m = pg_read_msg(f)
            if m is None or m[0] != b"p":
                return
            pw = m[1].split(b"\0")[0].decode("utf-8", "replace")
            if pw != PASSWORD:
                pg_error(f, "28P01", "password authentication failed for user")
                return
        pg_send(f, b"R", struct.pack("!i", 0))
        pg_send(f, b"S", b"server_version\0" + b"16.0\0")
        pg_send(f, b"S", b"client_encoding\0" + b"UTF8\0")
        pg_send(f, b"K", struct.pack("!ii", 1234, 5678))
        pg_ready(f, b"I")
        txn = b"I"
        stmts = {}
        portals = {}
        while True:
            m = pg_read_msg(f)
            if m is None:
                break
            typ, body = m
            if typ == b"Q":
                sql = body.split(b"\0")[0].decode("utf-8", "replace")
                if not sql.strip():
                    pg_send(f, b"I", b"")
                    pg_ready(f, txn)
                    continue
                try:
                    kind = pg_statement(conn, sql)
                except Exception as e:
                    pg_error(f, sqlstate(str(e)), str(e))
                    if txn == b"T":
                        txn = b"E"
                    pg_ready(f, txn)
                    continue
                pg_send_result(f, kind)
                first = first_keyword(sql)
                if first == "BEGIN":
                    txn = b"T"
                elif first in ("COMMIT", "ROLLBACK"):
                    txn = b"I"
                pg_ready(f, txn)
            elif typ == b"P":
                parts = body.split(b"\0", 2)
                stmts[parts[0].decode("utf-8", "replace")] = parts[1].decode("utf-8", "replace")
                pg_send(f, b"1", b"")
            elif typ == b"B":
                pos = 0

                def cstr_at():
                    nonlocal pos
                    end = body.index(b"\0", pos)
                    out = body[pos:end]
                    pos = end + 1
                    return out

                portal = cstr_at().decode("utf-8", "replace")
                stmt = cstr_at().decode("utf-8", "replace")
                nfmt = struct.unpack("!h", body[pos:pos + 2])[0]
                pos += 2 + 2 * nfmt
                nparams = struct.unpack("!h", body[pos:pos + 2])[0]
                pos += 2
                vals = []
                for _ in range(nparams):
                    ln = struct.unpack("!i", body[pos:pos + 4])[0]
                    pos += 4
                    if ln < 0:
                        vals.append(None)
                    else:
                        vals.append(body[pos:pos + ln].decode("utf-8", "replace"))
                        pos += ln
                portals[portal] = (stmts.get(stmt, ""), vals)
                pg_send(f, b"2", b"")
            elif typ == b"D":
                if body[:1] == b"S":
                    pg_send(f, b"t", struct.pack("!h", 0))
                pg_send(f, b"n", b"")
            elif typ == b"E":
                portal = body.split(b"\0")[0].decode("utf-8", "replace")
                query, vals = portals.get(portal, ("", []))
                for i, val in enumerate(vals, 1):
                    lit = "NULL" if val is None else "'" + val.replace("'", "''") + "'"
                    query = query.replace("$%d" % i, lit)
                try:
                    kind = pg_statement(conn, query)
                except Exception as e:
                    pg_error(f, sqlstate(str(e)), str(e))
                    if txn == b"T":
                        txn = b"E"
                    continue
                pg_send_result(f, kind)
                first = first_keyword(query)
                if first == "BEGIN":
                    txn = b"T"
                elif first in ("COMMIT", "ROLLBACK"):
                    txn = b"I"
            elif typ == b"S":
                pg_ready(f, txn)
            elif typ == b"C":
                pg_send(f, b"3", b"")
            elif typ == b"H":
                pass
            elif typ == b"X":
                break
            else:
                pg_error(f, "08P01", "unsupported message")
    finally:
        try:
            conn.close()
        except Exception:
            pass
        sock.close()


def pg_accept_loop(srv, db_path):
    while True:
        conn, _ = srv.accept()
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        threading.Thread(target=pg_serve, args=(conn, db_path), daemon=True).start()


def serve(sock, db_path):
    global CONNS_CURRENT, ROLE, ACK_MAX, SKIP_SYNC
    conn = sqlite3.connect(db_path, isolation_level=None)
    with STATS_LOCK:
        STATS["connections_current"] += 1
        STATS["connections_total"] += 1
    conn.execute("PRAGMA foreign_keys=ON")
    # Ticket 5's crash ladder: acked commits must survive SIGKILL (FULL
    # fsyncs the WAL on commit) and a torn *newest* file must recover; WAL
    # keeps the hot tail in <db>-wal, whose torn tail SQLite discards
    # frame by frame.
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=FULL")
    f = sock.makefile("rwb")
    try:
        if PASSWORD:
            raw = f.readline(65536)
            line = raw.decode("utf-8", "replace").rstrip("\n")
            if not line.startswith("AUTH ") or line[5:] != PASSWORD:
                f.write(b"ERR auth authentication required\n")
                f.flush()
                return
            f.write(b"OK 0\n")
            f.flush()
        while True:
            raw, too_long = read_line_capped(f)
            if raw is None:
                break
            if too_long:
                stats_error()
                f.write(b"ERR limit statement too large\n")
                f.flush()
                continue
            sql = raw.decode("utf-8", "replace").rstrip("\n")
            if not sql.strip():
                continue
            stats_bump(sql)
            up = sql.strip().upper()
            if up == "SHOW ROLE":
                f.write(role_reply())
                f.flush()
                continue
            if up == "PROMOTE":
                # Manual promotion (the two-node lite): the survivor is the
                # only node left, so its commits need no replica ack.
                SKIP_SYNC = True
                if ROLE == "replica":
                    ROLE = "primary"
                    print("sdb-sql reference: node %d promoted (manual)" % PORT,
                          file=sys.stderr, flush=True)
                f.write(b"OK 0\n")
                f.flush()
                continue
            if sql.startswith("SDB_REPL "):
                try:
                    want = int(sql.split(None, 1)[1])
                except Exception:
                    want = 0
                entries, last = log_since(DATA, want)
                out = []
                for lsn, entry_sql in entries:
                    out.append("RLOG %d %s\n" % (
                        lsn, base64.b64encode(entry_sql.encode()).decode()))
                out.append("REND %d\n" % last)
                f.write("".join(out).encode())
                f.flush()
                continue
            if sql.startswith("SDB_ACK "):
                try:
                    ack = int(sql.split(None, 1)[1])
                    if ack > ACK_MAX:
                        ACK_MAX = ack
                except Exception:
                    pass
                f.write(b"OK 0\n")
                f.flush()
                continue
            if ROLE == "replica" and first_keyword(sql) in WRITE_KEYWORDS:
                stats_error()
                f.write(b"ERR readonly replica\n")
                f.flush()
                continue
            if nesting_too_deep(sql):
                stats_error()
                f.write(b"ERR limit nesting too deep\n")
                f.flush()
                continue
            try:
                reply = timed_answer(conn, sql)
            except Exception as e:
                stats_error()
                msg = str(e).replace("\n", " ")
                cls = classify(msg)
                if cls is None:
                    print("sdb-sql reference: unmapped error %r for %r" % (msg, sql),
                          file=sys.stderr, flush=True)
                    cls = "syntax"
                reply = ("ERR %s %s\n" % (cls, msg)).encode()
            else:
                if ROLE == "primary" and NODES and first_keyword(sql) in LOG_KEYWORDS:
                    lsn = append_log(DATA, sql)
                    if (first_keyword(sql) == "COMMIT"
                            and not SKIP_SYNC and not wait_ack(lsn)):
                        print("sdb-sql reference: ack timeout at lsn %d; leader tail:" % lsn,
                              file=sys.stderr, flush=True)
                        try:
                            tail, _ = log_since(DATA, max(0, lsn - 8))
                            for plsn, psql in tail:
                                print("  leader log %d %r" % (plsn, psql[:160]),
                                      file=sys.stderr, flush=True)
                        except Exception:
                            pass
                        reply = b"ERR busy replica ack timeout\n"
            f.write(reply)
            f.flush()
    finally:
        with STATS_LOCK:
            STATS["connections_current"] -= 1
        with CONN_LOCK:
            CONNS_CURRENT -= 1
        try:
            conn.close()
        except Exception:
            pass
        sock.close()


def main():
    global ROLE, LAST_LSN, PORT, DATA, NODES, PRIMARY, PG_PORT, STMT_TIMEOUT
    port = 5480
    data = None
    replicaof = None
    nodes_arg = None
    pg_port = 0
    args = sys.argv[1:]
    for i, a in enumerate(args):
        if a == "--port" and i + 1 < len(args):
            port = int(args[i + 1])
        elif a == "--data" and i + 1 < len(args):
            data = args[i + 1]
        elif a == "--replicaof" and i + 1 < len(args):
            replicaof = args[i + 1]
        elif a == "--nodes" and i + 1 < len(args):
            nodes_arg = args[i + 1]
        elif a == "--pg-port" and i + 1 < len(args):
            pg_port = int(args[i + 1])
        elif a.isdigit() and (i == 0 or args[i - 1] not in (
                "--port", "--data", "--replicaof", "--nodes", "--pg-port")):
            port = int(a)
    if data is None:
        data = "/tmp"
    os.makedirs(data, exist_ok=True)
    PORT = port
    DATA = data
    PG_PORT = pg_port
    try:
        STMT_TIMEOUT = max(0, int(os.environ.get("SDB_SQL_STATEMENT_TIMEOUT_MS", "0"))) / 1000.0
    except ValueError:
        STMT_TIMEOUT = 0.0
    nodes_text = nodes_arg or os.environ.get("SDB_SQL_NODES", "")
    NODES = [n.strip() for n in nodes_text.split(",") if n.strip()]
    ROLE = "replica" if replicaof else "primary"
    PRIMARY = replicaof
    load_log(data)
    db_path = os.path.join(data, "sdb-sql-ref-%d.sqlite" % port)
    # The store is the harness's to own: it hands a fresh --data per run and
    # expects the same directory to survive a restart (persistence).
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    srv.listen(128)
    # One long-lived connection: SQLite checkpoints and *deletes* the WAL
    # file when the last connection closes, and the crash ladder needs a
    # log that survives between harness connections so it can tear its
    # tail. The keeper holds the database open without any transaction.
    keeper = sqlite3.connect(db_path, isolation_level=None)
    keeper.execute("PRAGMA journal_mode=WAL")
    keeper.execute("PRAGMA synchronous=FULL")
    print("sdb-sql reference listening on %d (db %s)" % (port, db_path), flush=True)
    if ROLE == "replica" and PRIMARY:
        threading.Thread(target=replica_loop, args=(data, PRIMARY), daemon=True).start()
    if PG_PORT:
        pgsrv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        pgsrv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        pgsrv.bind(("0.0.0.0", PG_PORT))
        pgsrv.listen(128)
        print("sdb-sql reference PostgreSQL wire on %d" % PG_PORT, flush=True)
        threading.Thread(target=pg_accept_loop, args=(pgsrv, db_path), daemon=True).start()
    global CONNS_CURRENT
    while True:
        conn, _ = srv.accept()
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        with CONN_LOCK:
            if CONNS_CURRENT >= MAX_CONNS:
                try:
                    conn.sendall(b"ERR limit too many connections\n")
                except Exception:
                    pass
                conn.close()
                continue
            CONNS_CURRENT += 1
        threading.Thread(target=serve, args=(conn, db_path), daemon=True).start()


main()
"#;

/// Write the reference server into `dir`, listening on `port`. `start.sh`
/// bakes the port (the sandbox starts it without arguments) and uses a
/// `--data` directory under `/tmp` inside the container.
pub fn write_reference(dir: &std::path::Path, port: u16) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("sql_server.py"), SQL_REF_SERVER_PY)?;
    // The launch contract passes `--port`/`--data`; a bare `./start.sh`
    // falls back to the baked defaults.
    let start = format!(
        "#!/bin/bash\n\
         if [ $# -eq 0 ]; then set -- --port {port} --data /tmp; fi\n\
         exec python3 -u /workspace/sql_server.py \"$@\"\n"
    );
    std::fs::write(dir.join("start.sh"), start)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("start.sh"), std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::compare::{compare, render_expected};
    use super::super::driver::{run_stage, stage_seed};
    use super::super::generator::{build_script, Profile};
    use super::super::oracle::Oracle;
    use super::super::scenarios::pipeline;
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// Kills the reference on drop, panics included.
    struct RefProc {
        child: Child,
        script: std::path::PathBuf,
        port: u16,
        data: std::path::PathBuf,
    }

    impl RefProc {
        fn spawn(&mut self) {
            self.child = Command::new("python3")
                .arg(&self.script)
                .args(["--port", &self.port.to_string(), "--data"])
                .arg(&self.data)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("python3 must be installed for the reference adapter");
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("reference did not open port {}", self.port);
        }

        /// SIGKILL the reference (ticket 5's crash probe): no respawn.
        fn kill(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }

        /// Stop and start on the same store (tickets 4 and 5).
        fn restart(&mut self) -> Result<(), String> {
            self.kill();
            self.spawn();
            Ok(())
        }
    }

    impl Drop for RefProc {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            // The fixture's script and data dirs go with it (the suite used
            // to leave /tmp/sdb-sql-* dirs behind, including on panics).
            if let Some(dir) = self.script.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
            let _ = std::fs::remove_dir_all(&self.data);
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn start_reference(port: u16) -> RefProc {
        let dir = std::env::temp_dir().join(format!("sdb-sql-ref-{}-{port}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_reference(&dir, port).unwrap();
        let data = std::env::temp_dir().join(format!("sdb-sql-data-{}-{port}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        let mut guard = RefProc {
            // Placeholder replaced by spawn(): the struct owns the child.
            child: Command::new("true").spawn().unwrap(),
            script: dir.join("sql_server.py"),
            port,
            data,
        };
        // From here the guard owns the child, so the wait-for-port panic
        // path still kills and reaps it.
        guard.spawn();
        guard
    }

    /// The suite's 20/20 bar does not cover the ladder and the transaction
    /// checks (the pipeline runs those itself). Run them 20 times against
    /// one host process too, with a smaller bulk table: the point is
    /// flakiness, not size — the Docker live proofs use the real 50k.
    #[test]
    fn reference_passes_the_crash_and_txn_checks_twenty_times() {
        use super::super::crash;
        use super::super::driver::wait_until_answering;
        use super::super::txn;
        let port = free_port();
        let mut proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        let t = Duration::from_secs(10);
        for rep in 0..20u32 {
            let out = {
                let mut kill = || {
                    proc.kill();
                    Ok(())
                };
                crash::ack_kill(&addr, t, &format!("r{rep}_a"), 20, &mut kill).expect("ack_kill")
            };
            let acked = out.acked;
            proc.restart().unwrap();
            wait_until_answering(&addr, t).expect("answers after a SIGKILL");
            if out.completed {
                crash::check_acked_rows(&addr, t, &format!("r{rep}_a"), acked + 1)
                    .expect("answered in-flight row survived");
            } else {
                crash::check_acked_prefix(&addr, t, &format!("r{rep}_a"), acked, 1)
                    .expect("acked prefix");
            }

            let completed = {
                let mut kill = || {
                    proc.kill();
                    Ok(())
                };
                crash::bulk_update_and_kill(&addr, t, &format!("r{rep}_b"), 2_000, 500, &mut kill)
                    .expect("bulk update")
            };
            proc.restart().unwrap();
            wait_until_answering(&addr, t).expect("answers after the killed update");
            crash::check_uniform(&addr, t, &format!("r{rep}_b"), 2_000, completed)
                .expect("uniform");

            let before = crash::snapshot_files(&proc.data).expect("stamps");
            crash::append_row(&addr, t, &format!("r{rep}_a"), acked + 1).expect("victim");
            proc.kill();
            let victim = crash::changed_file(&proc.data, &before)
                .expect("changed")
                .expect("the victim commit wrote something");
            let cut = crash::torn_tail(&victim, 256).expect("torn tail");
            assert!(cut > 0, "rep {rep}: nothing to tear");
            proc.restart().unwrap();
            wait_until_answering(&addr, t).expect("answers after a torn tail");
            crash::check_acked_prefix(&addr, t, &format!("r{rep}_a"), acked, 2)
                .expect("after a torn tail");

            txn::isolation(&addr, t, &format!("r{rep}_iso")).expect("isolation");
            {
                // Two callbacks share the one process.
                let proc = std::cell::RefCell::new(&mut proc);
                let mut kill = || {
                    proc.borrow_mut().kill();
                    Ok(())
                };
                let mut restart = || {
                    proc.borrow_mut().restart()?;
                    wait_until_answering(&addr, t)
                };
                txn::kill_with_open_transaction(
                    &addr,
                    t,
                    &format!("r{rep}_txn"),
                    &mut kill,
                    &mut restart,
                )
                .expect("kill with a transaction open");
            }
        }
    }

    /// Stage 8's lite variant (`join-inner-only`): the cumulative suite
    /// keeps comma/inner joins and never emits `LEFT JOIN`, and the
    /// reference passes it.
    #[test]
    fn reference_passes_the_stage8_lite_suite() {
        use super::super::driver::{run_stage_with_variants, wait_until_answering};
        use super::super::generator::{build_script, Profile};
        let port = free_port();
        let mut proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        let t = Duration::from_secs(10);
        wait_until_answering(&addr, t).expect("answers");
        let passed: Vec<u32> = (0..=8).collect();
        let variants = vec![(8u32, vec!["join-inner-only".to_string()])];
        for run in 0..3u64 {
            let seed = stage_seed(&format!("stage8-lite-{run}"), 8);
            let mut restart = || proc.restart();
            let out = run_stage_with_variants(
                &addr,
                t,
                seed,
                &passed,
                &variants,
                120,
                None,
                Some(&mut restart),
            )
            .unwrap_or_else(|e| panic!("run {run}: verifier fault: {e}"));
            assert!(out.passed, "stage-8 lite run {run}: {}", out.detail);
        }
        // The narrowed profile really drops LEFT JOIN (and keeps joins).
        let mut p = Profile::for_passed(&passed);
        p.narrow(&variants[0].1);
        let (mut joins, mut left) = (0, 0);
        for seed in 0..40u64 {
            let s = build_script(seed, &p, 120);
            for c in &s.steps {
                let q = c.stmt.sql.to_lowercase();
                if c.stmt.kind == super::super::generator::Kind::Select
                    && (q.contains(" join ") || q.contains(", "))
                {
                    joins += 1;
                    left += usize::from(q.contains("left join"));
                }
            }
        }
        assert!(joins > 0, "the lite profile emitted no joins at all");
        assert_eq!(left, 0, "the lite profile emitted LEFT JOIN");
    }

    /// The dialect's statement atomicity: a failed multi-row INSERT
    /// changes nothing, in autocommit and inside a transaction alike. The
    /// host's SQLite keeps earlier rows, so the reference wraps DML in a
    /// statement savepoint — this pins both cases.
    #[test]
    fn reference_statement_atomicity_always_holds() {
        use super::super::client::TcpCandidate;
        use super::super::run::Candidate;
        use super::super::wire::Response;
        let port = free_port();
        let proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        let mut c = TcpCandidate::connect(&addr, Duration::from_secs(10)).unwrap();
        c.exec("CREATE TABLE quirk (a INTEGER, b INTEGER)").unwrap();
        match c
            .exec("INSERT INTO quirk VALUES (1, 1), ('zz', 2)")
            .unwrap()
        {
            Response::Err { .. } => {}
            other => panic!("expected a type error, got {other:?}"),
        }
        match c.exec("SELECT * FROM quirk").unwrap() {
            Response::Rows { rows, .. } => {
                assert!(rows.is_empty(), "autocommit kept partial rows: {rows:?}")
            }
            other => panic!("{other:?}"),
        }
        // Inside a transaction the same statement changes nothing either.
        c.exec("BEGIN").unwrap();
        assert!(matches!(
            c.exec("INSERT INTO quirk VALUES (2, 2), ('zz', 3)")
                .unwrap(),
            Response::Err { .. }
        ));
        match c.exec("SELECT * FROM quirk").unwrap() {
            Response::Rows { rows, .. } => {
                assert!(rows.is_empty(), "inside a transaction: {rows:?}")
            }
            other => panic!("{other:?}"),
        }
        // `INSERT ... SELECT` with a bad cell is atomic too.
        c.exec("CREATE TABLE quirk_src (a INTEGER, b TEXT)")
            .unwrap();
        c.exec("INSERT INTO quirk_src VALUES (1, '1'), (2, 'zz')")
            .unwrap();
        assert!(matches!(
            c.exec("INSERT INTO quirk SELECT a, b FROM quirk_src")
                .unwrap(),
            Response::Err { .. }
        ));
        match c.exec("SELECT * FROM quirk").unwrap() {
            Response::Rows { rows, .. } => {
                assert!(rows.is_empty(), "insert-select left rows: {rows:?}")
            }
            other => panic!("{other:?}"),
        }
        c.exec("ROLLBACK").unwrap();
        drop(proc);
    }

    /// The flake bar (docs/PROPOSAL-sdb-sql.md): each stage passes 20/20
    /// against the reference, through stage 11 (aggregate, join,
    /// constraint, subquery and index scripts). All
    /// runs share one server process, so this also proves stage scripts
    /// never collide on schema names. Stage 11's *perf* half is timing,
    /// not flakes, and lives in `reference_passes_the_stage11_perf_gate`
    /// (a loaded host must not fail the flake bar).
    #[test]
    fn reference_passes_every_stage_twenty_times() {
        let port = free_port();
        let mut proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        for stage in 0..=13u32 {
            let passed: Vec<u32> = (0..=stage).collect();
            for run in 0..20u32 {
                let seed = stage_seed(&format!("reference-bar-{run}"), stage);
                // Stage 4's suite restarts the candidate: the reference
                // comes back on the same store.
                let mut restart = || proc.restart();
                let out = run_stage(
                    &addr,
                    Duration::from_secs(10),
                    seed,
                    &passed,
                    120,
                    None,
                    Some(&mut restart),
                )
                .unwrap_or_else(|e| panic!("stage {stage} run {run}: verifier fault: {e}"));
                assert!(
                    out.passed,
                    "stage {stage} run {run} (seed {seed}): {}",
                    out.detail
                );
            }
        }
    }

    /// Stage 11's perf gate against a real process: two 200k-row tables,
    /// 500 alternating point lookups, a range round, and a restart between
    /// the two point rounds. The host is shared, so the gate's own bounds
    /// (5 ms p50, 10x) are the assertions; a healthy SQLite clears them by
    /// an order of magnitude.
    #[test]
    fn reference_passes_the_stage11_perf_gate() {
        use super::super::perf;
        let port = free_port();
        let mut proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        let full = {
            let mut restart = || proc.restart();
            perf::perf_gate(
                &addr,
                Duration::from_secs(30),
                0x5E11,
                false,
                Some(&mut restart),
            )
            .expect("the full perf gate")
        };
        assert!(full.contains("point p50"), "full report: {full}");
        assert!(full.contains("range p50"), "full report: {full}");
        assert!(full.contains("after restart"), "full report: {full}");
        // Lite: equality only, no restart.
        let lite =
            perf::perf_gate(&addr, Duration::from_secs(30), 0x5E12, true, None).expect("lite gate");
        assert!(!lite.contains("range"), "lite report: {lite}");
        drop(proc);
    }

    /// Stage 5 against a real process: SIGKILL after acks, a mid-flight
    /// bulk UPDATE, a torn newest data file, and a double restart.
    #[test]
    fn reference_survives_the_crash_safety_ladder() {
        use super::super::crash;
        use super::super::driver::wait_until_answering;
        let port = free_port();
        let mut proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        let t = Duration::from_secs(10);

        // (a) Acked rows survive a SIGKILL; only the one statement that was
        // still in flight when the kill landed may be missing.
        let out = {
            let mut kill = || {
                proc.kill();
                Ok(())
            };
            crash::ack_kill(&addr, t, "crash_acked", 40, &mut kill).expect("ack_kill")
        };
        let acked = out.acked;
        assert_eq!(acked, 40);
        proc.restart().unwrap();
        wait_until_answering(&addr, t).expect("answers after the SIGKILL");
        if out.completed {
            crash::check_acked_rows(&addr, t, "crash_acked", acked + 1)
                .expect("the answered in-flight row survived");
        } else {
            crash::check_acked_prefix(&addr, t, "crash_acked", acked, 1)
                .expect("acked rows survived");
        }

        // (b) A bulk UPDATE killed mid-flight is all or nothing.
        let completed = {
            let mut kill = || {
                proc.kill();
                Ok(())
            };
            crash::bulk_update_and_kill(&addr, t, "crash_bulk", 2_000, 500, &mut kill)
                .expect("bulk update")
        };
        proc.restart().unwrap();
        wait_until_answering(&addr, t).expect("answers after the killed update");
        crash::check_uniform(&addr, t, "crash_bulk", 2_000, completed).expect("all-or-nothing");

        // (c) A torn tail (a partial trailing record): kill first, cut the
        // file the victim commit touched, and require every older row.
        let before = crash::snapshot_files(&proc.data).expect("store stamps");
        crash::append_row(&addr, t, "crash_acked", acked + 1).expect("victim row");
        proc.kill();
        let victim = crash::changed_file(&proc.data, &before)
            .expect("changed file")
            .expect("the victim commit wrote something");
        let cut = crash::torn_tail(&victim, 256).expect("torn tail");
        assert!(cut > 0, "the store must have a newest file to tear");
        proc.restart().unwrap();
        wait_until_answering(&addr, t).expect("answers after a torn tail");
        crash::check_acked_prefix(&addr, t, "crash_acked", acked, 2)
            .expect("data after a torn tail");
        crash::check_uniform(&addr, t, "crash_bulk", 2_000, false).expect("bulk after a torn tail");

        // (d) A second restart changes nothing.
        proc.restart().unwrap();
        wait_until_answering(&addr, t).expect("answers after two restarts");
        crash::check_acked_prefix(&addr, t, "crash_acked", acked, 2).expect("double restart");
        crash::check_uniform(&addr, t, "crash_bulk", 2_000, false).expect("double restart");
    }

    /// Stage 6 against a real process: connections see only committed
    /// rows, and a kill with a transaction open discards it.
    #[test]
    fn reference_transactions_isolate_and_roll_back_on_a_kill() {
        use super::super::driver::wait_until_answering;
        use super::super::txn;
        let port = free_port();
        let proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        let t = Duration::from_secs(10);
        txn::isolation(&addr, t, "txn_iso").expect("isolation");
        {
            // Two callbacks share one process: a cell keeps the borrows
            // disjoint.
            let proc = std::cell::RefCell::new(proc);
            let mut kill = || {
                proc.borrow_mut().kill();
                Ok(())
            };
            let mut restart = || {
                proc.borrow_mut().restart().map_err(|e| e.to_string())?;
                wait_until_answering(&addr, t).map_err(|e| e.to_string())
            };
            txn::kill_with_open_transaction(&addr, t, "txn_kill", &mut kill, &mut restart)
                .expect("open transaction discarded by recovery");
        }
    }

    /// A restart that loses the store must be caught (the reference's
    /// `--data` is the harness-owned directory).
    #[test]
    fn reference_data_lost_across_a_restart_is_a_failure() {
        let port = free_port();
        let mut proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        let mut keep = || proc.restart();
        assert_eq!(
            super::super::scenarios::persistence(
                &addr,
                Duration::from_secs(10),
                stage_seed("reference-persistence", 4),
                &Profile::through(4),
                &mut keep,
            ),
            super::super::scenarios::ScenarioResult::Pass
        );
        // Point the reference at a fresh store and restart: the reads must
        // drift.
        proc.data =
            std::env::temp_dir().join(format!("sdb-sql-data-lost-{}-{port}", std::process::id()));
        let _ = std::fs::remove_dir_all(&proc.data);
        std::fs::create_dir_all(&proc.data).unwrap();
        let mut fresh = || proc.restart();
        match super::super::scenarios::persistence(
            &addr,
            Duration::from_secs(10),
            stage_seed("reference-persistence", 4) ^ 1,
            &Profile::through(4),
            &mut fresh,
        ) {
            super::super::scenarios::ScenarioResult::Fail(d) => {
                assert!(d.contains("after a clean restart"), "{d}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// R37 F5/R36-1: the persistence path records the sentinel seed it
    /// actually used, and that seed stays out of the candidate-visible
    /// failure detail. The run seed maps onto a known bad persistence seed.
    #[test]
    fn r37_f5_persistence_seed_is_evidence_not_feedback() {
        let bad = 9231256963851754504u64;
        let run_seed = bad ^ 0x5E27_A510_0000_0004;
        let port = free_port();
        let mut proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        let mut used = None;
        let mut keep = || proc.restart();
        assert_eq!(
            super::super::scenarios::persistence_with_seed(
                &addr,
                Duration::from_secs(10),
                run_seed,
                &Profile::through(4),
                &mut keep,
                &mut used,
            ),
            super::super::scenarios::ScenarioResult::Pass
        );
        let used = used.expect("the used seed is recorded");
        assert!(used > bad, "the bad seed moved on: {used} > {bad}");

        // The same run seed against an empty store fails; its detail is
        // candidate-visible and must not contain the harness seed.
        proc.data =
            std::env::temp_dir().join(format!("sdb-sql-seed-lost-{}-{port}", std::process::id()));
        let _ = std::fs::remove_dir_all(&proc.data);
        std::fs::create_dir_all(&proc.data).unwrap();
        let mut used_again = None;
        let mut fresh = || proc.restart();
        match super::super::scenarios::persistence_with_seed(
            &addr,
            Duration::from_secs(10),
            run_seed,
            &Profile::through(4),
            &mut fresh,
            &mut used_again,
        ) {
            super::super::scenarios::ScenarioResult::Fail(d) => {
                assert!(!d.contains(&used.to_string()), "seed leaked: {d}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(used_again, Some(used));
    }

    /// Review round 14, G1, as it happened live. A ladder that failed
    /// tickets 2 and 4 is graded on `[0, 1, 3, 9]`: no restart, no
    /// persistence dataset (gating on the highest stage used to run it). One
    /// that failed tickets 1-3 is graded on `[0, 4]`: the restart runs, on
    /// a dataset of ticket-0 grammar compared as a multiset.
    #[test]
    fn reference_suites_run_persistence_only_with_ticket_4() {
        let port = free_port();
        let mut proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        let restarts = std::cell::Cell::new(0u32);
        for (passed, restarted) in [
            (vec![0u32, 1, 3, 9], false),
            (vec![0, 4], true),
            (vec![0, 1, 2, 3, 4], true),
        ] {
            for run in 0..3u32 {
                restarts.set(0);
                let seed = stage_seed(&format!("g1-{passed:?}-{run}"), passed[passed.len() - 1]);
                let mut restart = || {
                    restarts.set(restarts.get() + 1);
                    proc.restart()
                };
                let out = run_stage(
                    &addr,
                    Duration::from_secs(10),
                    seed,
                    &passed,
                    120,
                    None,
                    Some(&mut restart),
                )
                .unwrap_or_else(|e| panic!("{passed:?} run {run}: verifier fault: {e}"));
                assert!(out.passed, "{passed:?} run {run}: {}", out.detail);
                assert_eq!(restarts.get() > 0, restarted, "{passed:?} run {run}");
            }
        }
    }

    /// Pipelining a generated script against the reference: every line is
    /// sent before the first response is read, and every answer grades
    /// clean against the oracle.
    #[test]
    fn reference_answers_a_pipelined_script() {
        let port = free_port();
        let _ref = start_reference(port);
        let script = build_script(
            stage_seed("reference-pipeline", 3),
            &Profile::through(3),
            60,
        );
        let checked: Vec<_> = script.setup.iter().chain(&script.steps).collect();
        let lines: Vec<String> = checked.iter().map(|c| c.stmt.sql.clone()).collect();
        let got = pipeline(("127.0.0.1", port), &lines, Duration::from_secs(10))
            .expect("the reference must answer every pipelined line");
        assert_eq!(got.len(), lines.len());
        let oracle = Oracle::new().unwrap();
        for ((reply, line), c) in got.iter().zip(&lines).zip(&checked) {
            let expected = oracle.exec(line);
            compare(&expected, reply, c.stmt.ordered).unwrap_or_else(|m| {
                panic!("{line:?}: {m:?} (expected {})", render_expected(&expected))
            });
        }
    }
}
