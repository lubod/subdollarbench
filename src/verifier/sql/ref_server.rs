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


def serve(sock, db_path):
    conn = sqlite3.connect(db_path, isolation_level=None)
    conn.execute("PRAGMA foreign_keys=ON")
    # Ticket 5's crash ladder: acked commits must survive SIGKILL (FULL
    # fsyncs the WAL on commit) and a torn *newest* file must recover; WAL
    # keeps the hot tail in <db>-wal, whose torn tail SQLite discards
    # frame by frame.
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=FULL")
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
    /// against the reference, through stage 9 (aggregate, join and
    /// constraint scripts). All
    /// runs share one server process, so this also proves stage scripts
    /// never collide on schema names. Stages 7+ land here as they do.
    #[test]
    fn reference_passes_every_stage_twenty_times() {
        let port = free_port();
        let mut proc = start_reference(port);
        let addr = format!("127.0.0.1:{port}");
        for stage in 0..=9u32 {
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
            &mut fresh,
        ) {
            super::super::scenarios::ScenarioResult::Fail(d) => {
                assert!(d.contains("after a clean restart"), "{d}")
            }
            other => panic!("{other:?}"),
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
