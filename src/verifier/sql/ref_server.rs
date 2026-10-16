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
import os
import re
import socket
import sqlite3
import sys
import threading


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
    if not stmt.upper().startswith("CREATE TABLE"):
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


def classify(msg):
    if msg.startswith("no such table"):
        return "no_table"
    if (msg.startswith("no such column")
            or msg.startswith("no such function")
            or "has no column named" in msg):
        return "no_column"
    if msg.startswith("ambiguous column name"):
        return "ambiguous"
    if "already exists" in msg:
        return "exists"
    if msg.startswith("datatype mismatch") or "cannot store" in msg:
        return "type"
    if "constraint failed" in msg:
        return "constraint"
    if msg.startswith("cannot drop column") or "after drop column" in msg:
        return "constraint"
    if (msg.startswith("syntax error")
            or msg.startswith("near ")
            or msg.startswith("unrecognized token")
            or msg.startswith("incomplete input")):
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


def answer(conn, sql):
    cur = conn.cursor()
    before = conn.total_changes
    cur.execute(translate_ddl(sql))
    if cur.description is not None:
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


def serve(sock, db_path):
    conn = sqlite3.connect(db_path, isolation_level=None)
    conn.execute("PRAGMA foreign_keys=ON")
    f = sock.makefile("rwb")
    try:
        for raw in f:
            sql = raw.decode("utf-8", "replace").rstrip("\n")
            try:
                reply = answer(conn, sql)
            except Exception as e:
                msg = str(e).replace("\n", " ")
                cls = classify(msg)
                if cls is None:
                    print("sdb-sql reference: unmapped error %r for %r" % (msg, sql),
                          file=sys.stderr, flush=True)
                    cls = "syntax"
                reply = ("ERR %s %s\n" % (cls, msg)).encode()
            f.write(reply)
            f.flush()
    finally:
        try:
            conn.close()
        except Exception:
            pass
        sock.close()


def main():
    port = 5480
    data = None
    args = sys.argv[1:]
    for i, a in enumerate(args):
        if a == "--port" and i + 1 < len(args):
            port = int(args[i + 1])
        elif a == "--data" and i + 1 < len(args):
            data = args[i + 1]
        elif a.isdigit():
            port = int(a)
    if data is None:
        data = "/tmp"
    db_path = os.path.join(data, "sdb-sql-ref-%d.sqlite" % port)
    if os.path.exists(db_path):
        os.remove(db_path)
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    srv.listen(128)
    print("sdb-sql reference listening on %d (db %s)" % (port, db_path), flush=True)
    while True:
        conn, _ = srv.accept()
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        threading.Thread(target=serve, args=(conn, db_path), daemon=True).start()


main()
"#;

/// Write the reference server into `dir`, listening on `port`. `start.sh`
/// bakes the port (the sandbox starts it without arguments) and uses a
/// `--data` directory under `/tmp` inside the container.
pub fn write_reference(dir: &std::path::Path, port: u16) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("sql_server.py"), SQL_REF_SERVER_PY)?;
    let start = format!(
        "#!/bin/bash\nexec python3 -u /workspace/sql_server.py --port {port} --data /tmp\n"
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
    }

    impl Drop for RefProc {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
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
        write_reference(&dir, port).unwrap();
        let child = Command::new("python3")
            .arg(dir.join("sql_server.py"))
            .args(["--port", &port.to_string(), "--data"])
            .arg(std::env::temp_dir())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("python3 must be installed for the reference adapter");
        // The guard owns the child from here, so the wait-for-port panic
        // path still kills and reaps it.
        let guard = RefProc { child };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return guard;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("reference did not open port {port}");
    }

    /// The flake bar (docs/PROPOSAL-sdb-sql.md): each stage passes 20/20
    /// against the reference. All runs share one server process, so this
    /// also proves stage scripts never collide on schema names.
    #[test]
    fn reference_passes_every_stage_twenty_times() {
        let port = free_port();
        let _ref = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        for stage in 0..=3u32 {
            let passed: Vec<u32> = (0..=stage).collect();
            for run in 0..20u32 {
                let seed = stage_seed(&format!("reference-bar-{run}"), stage);
                let out = run_stage(&addr, Duration::from_secs(10), seed, &passed, 120, None)
                    .unwrap_or_else(|e| panic!("stage {stage} run {run}: verifier fault: {e}"));
                assert!(
                    out.passed,
                    "stage {stage} run {run} (seed {seed}): {}",
                    out.detail
                );
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
        let script = build_script(stage_seed("reference-pipeline", 3), &Profile::through(3), 60);
        let checked: Vec<_> = script.setup.iter().chain(&script.steps).collect();
        let lines: Vec<String> = checked.iter().map(|c| c.stmt.sql.clone()).collect();
        let got = pipeline(
            ("127.0.0.1", port),
            &lines,
            Duration::from_secs(10),
        )
        .expect("the reference must answer every pipelined line");
        assert_eq!(got.len(), lines.len());
        let oracle = Oracle::new().unwrap();
        for ((reply, line), c) in got.iter().zip(&lines).zip(&checked) {
            let expected = oracle.exec(line);
            compare(&expected, reply, c.stmt.ordered)
                .unwrap_or_else(|m| panic!("{line:?}: {m:?} (expected {})", render_expected(&expected)));
        }
    }
}
