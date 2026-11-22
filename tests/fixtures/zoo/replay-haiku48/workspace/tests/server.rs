use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU16, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_sdb");
const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");
const DEADLINE: Duration = Duration::from_secs(20);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sdb-it-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// Ports never repeat within one test process, so two servers from this run
/// cannot end up sharing a listener. The base is derived from the process id
/// so concurrent test processes start in different ranges.
fn fresh_port() -> u16 {
    static SEQ: AtomicU16 = AtomicU16::new(0);
    let base = 20000 + (std::process::id() % 250) as u16 * 128;
    base + SEQ.fetch_add(1, Ordering::SeqCst)
}

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(data: &Path) -> Server {
        for _ in 0..8 {
            let port = fresh_port();
            let mut child = Command::new(BIN)
                .args(["--port", &port.to_string(), "--data", data.to_str().expect("utf8 path")])
                .spawn()
                .expect("spawn sdb");
            let until = Instant::now() + DEADLINE;
            loop {
                if child.try_wait().expect("poll sdb").is_some() {
                    break;
                }
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Server { child, port };
                }
                assert!(Instant::now() < until, "server did not listen on {}", port);
                thread::sleep(Duration::from_millis(10));
            }
        }
        panic!("sdb could not bind any port in its range");
    }

    /// Hard stop (SIGKILL): nothing gets a chance to flush, so every
    /// acknowledged write must already be on disk.
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(port: u16) -> Client {
        let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(10))).expect("read timeout");
        stream.set_nodelay(true).expect("nodelay");
        let writer = stream.try_clone().expect("clone");
        Client {
            reader: BufReader::new(stream),
            writer,
        }
    }

    /// Sends one statement and returns its complete answer frame.
    fn ask(&mut self, stmt: &str) -> String {
        self.writer
            .write_all(format!("{}\n", stmt).as_bytes())
            .expect("send statement");
        self.answer()
    }

    fn answer(&mut self) -> String {
        let mut out = String::new();
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read answer");
        assert!(!line.is_empty(), "server closed the connection");
        let rows = line.starts_with("ROWS ");
        out.push_str(&line);
        if rows {
            loop {
                line.clear();
                self.reader.read_line(&mut line).expect("read row");
                assert!(!line.is_empty(), "frame cut short");
                out.push_str(&line);
                if line.starts_with("END ") {
                    break;
                }
            }
        }
        out
    }
}

/// Same contract as the in-process golden check: ERR lines keep only the class.
fn normalize(transcript: &str) -> String {
    let mut out = String::new();
    let mut in_rows = false;
    for line in transcript.lines() {
        let kept = if in_rows {
            in_rows = !line.starts_with("END ");
            line.to_string()
        } else if line.starts_with("ROWS ") {
            in_rows = true;
            line.to_string()
        } else if line.starts_with("ERR ") {
            line.splitn(3, ' ').take(2).collect::<Vec<_>>().join(" ")
        } else {
            line.to_string()
        };
        out.push_str(&kept);
        out.push('\n');
    }
    out
}

fn stats(c: &mut Client) -> BTreeMap<String, i64> {
    let frame = c.ask("SHOW STATS");
    assert!(frame.starts_with("ROWS 2\nname\tvalue\n"), "stats frame: {}", frame);
    frame
        .lines()
        .skip(2)
        .filter(|l| !l.starts_with("END "))
        .map(|l| {
            let (k, v) = l.split_once('\t').expect("name/value row");
            (k.to_string(), v.parse::<i64>().expect("integer value"))
        })
        .collect()
}

/// Reads the single data value of a one-row, one-column answer.
fn scalar(frame: &str) -> i64 {
    let lines: Vec<&str> = frame.lines().collect();
    assert_eq!(lines.len(), 4, "frame: {}", frame);
    lines[2].parse().expect("integer")
}

#[test]
fn golden_scripts_over_the_wire() {
    let mut failures = Vec::new();
    let mut paths: Vec<PathBuf> = fs::read_dir(GOLDEN)
        .expect("golden directory")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().map_or(false, |x| x == "sql"))
        .collect();
    paths.sort();
    for path in paths {
        let script = fs::read_to_string(&path).expect("script");
        let want = fs::read_to_string(path.with_extension("out")).expect("expectation");
        let dir = scratch(&path.file_stem().unwrap().to_string_lossy());
        let mut server = Server::start(&dir);
        let mut client = Client::connect(server.port);
        let mut transcript = String::new();
        for line in script.lines().filter(|l| !l.trim().is_empty()) {
            if line == ".restart" {
                drop(client);
                server.kill();
                server = Server::start(&dir);
                client = Client::connect(server.port);
                continue;
            }
            transcript.push_str(&client.ask(line));
        }
        drop(client);
        drop(server);
        let _ = fs::remove_dir_all(&dir);
        let got = normalize(&transcript);
        if got != want {
            let first = got
                .lines()
                .zip(want.lines())
                .enumerate()
                .find(|(_, (g, w))| g != w)
                .map(|(i, (g, w))| format!("line {}: got {:?}, want {:?}", i + 1, g, w))
                .unwrap_or_else(|| "lengths differ".to_string());
            failures.push(format!("{}: {}", path.display(), first));
        }
    }
    assert!(failures.is_empty(), "wire mismatches:\n{}", failures.join("\n"));
}

#[test]
fn show_stats_deltas_are_exact() {
    let dir = scratch("stats");
    let server = Server::start(&dir);
    let mut a = Client::connect(server.port);
    // The readiness probe of Server::start may still be closing; wait until only `a` is open.
    let until = Instant::now() + DEADLINE;
    while stats(&mut a)["connections_current"] != 1 {
        assert!(Instant::now() < until, "stale connections never closed");
    }
    let before = stats(&mut a);
    let script = [
        "CREATE TABLE st_t (id INTEGER, v INTEGER)",
        "INSERT INTO st_t VALUES (1, 10), (2, 20)",
        "INSERT INTO st_t VALUES (3, 30)",
        "SELECT * FROM st_t",
        "UPDATE st_t SET v = v + 1 WHERE id = 1",
        "DELETE FROM st_t WHERE id = 2",
        "BEGIN",
        "INSERT INTO st_t VALUES (4, 40)",
        "COMMIT",
        "BEGIN",
        "INSERT INTO st_t VALUES (5, 50)",
        "ROLLBACK",
        "SELECT nope FROM st_t",
    ];
    for stmt in script {
        a.ask(stmt);
    }
    let after = stats(&mut a);
    let delta = |k: &str| after[k] - before[k];
    // Thirteen script lines plus the SHOW STATS that takes the second snapshot.
    assert_eq!(delta("statements"), 14, "statements");
    assert_eq!(delta("selects"), 2, "selects");
    assert_eq!(delta("inserts"), 4, "inserts");
    assert_eq!(delta("updates"), 1, "updates");
    assert_eq!(delta("deletes"), 1, "deletes");
    assert_eq!(delta("commits"), 1, "commits");
    assert_eq!(delta("rollbacks"), 1, "rollbacks");
    assert_eq!(delta("errors"), 1, "errors");
    assert_eq!(after["connections_current"], 1);

    let mut b = Client::connect(server.port);
    assert_eq!(b.ask("SELECT 1 AS one"), "ROWS 1\none\n1\nEND 1\n");
    let open = stats(&mut a);
    assert_eq!(open["connections_current"], 2, "second client open");
    assert_eq!(open["connections_total"], after["connections_total"] + 1, "total counts accepts");
    drop(b);
    let until = Instant::now() + DEADLINE;
    while stats(&mut a)["connections_current"] != 1 {
        assert!(Instant::now() < until, "closed connection never released");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn explain_names_the_index_for_equality() {
    let dir = scratch("explain");
    let server = Server::start(&dir);
    let mut c = Client::connect(server.port);
    assert_eq!(c.ask("CREATE TABLE ex_t (k INTEGER, v TEXT)"), "OK 0\n");
    assert_eq!(c.ask("CREATE UNIQUE INDEX ex_k ON ex_t (k)"), "OK 0\n");
    assert_eq!(c.ask("INSERT INTO ex_t VALUES (1, 'a'), (2, 'b')"), "OK 2\n");
    let indexed = c.ask("EXPLAIN SELECT v AS v FROM ex_t WHERE k = 2");
    assert!(indexed.starts_with("ROWS "), "plan: {}", indexed);
    assert!(indexed.contains("INDEX ex_t.ex_k"), "plan: {}", indexed);
    assert!(!indexed.contains("SCAN ex_t"), "plan: {}", indexed);
    let scanned = c.ask("EXPLAIN SELECT v AS v FROM ex_t WHERE v = 'b'");
    assert!(scanned.contains("SCAN ex_t"), "plan: {}", scanned);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_increments_lose_no_update() {
    let dir = scratch("increments");
    let server = Server::start(&dir);
    let port = server.port;
    let mut setup = Client::connect(port);
    assert_eq!(setup.ask("CREATE TABLE cc_t (id INTEGER, v INTEGER)"), "OK 0\n");
    assert_eq!(setup.ask("INSERT INTO cc_t VALUES (1, 0)"), "OK 1\n");
    drop(setup);

    let workers: Vec<_> = (0..16)
        .map(|_| {
            thread::spawn(move || {
                let mut c = Client::connect(port);
                let mut done = 0;
                while done < 50 {
                    if c.ask("BEGIN") != "OK 0\n" {
                        continue;
                    }
                    let read = c.ask("SELECT v AS v FROM cc_t WHERE id = 1");
                    if read.starts_with("ERR ") {
                        c.ask("ROLLBACK");
                        continue;
                    }
                    let v = scalar(&read);
                    let upd = c.ask(&format!("UPDATE cc_t SET v = {} WHERE id = 1", v + 1));
                    if upd.starts_with("ERR ") {
                        c.ask("ROLLBACK");
                        continue;
                    }
                    if c.ask("COMMIT") == "OK 0\n" {
                        done += 1;
                    }
                }
            })
        })
        .collect();
    for w in workers {
        w.join().expect("worker finished");
    }
    let mut check = Client::connect(port);
    assert_eq!(scalar(&check.ask("SELECT v AS v FROM cc_t WHERE id = 1")), 800);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn readers_see_committed_state_without_waiting() {
    let dir = scratch("isolation");
    let server = Server::start(&dir);
    let mut a = Client::connect(server.port);
    let mut b = Client::connect(server.port);
    assert_eq!(a.ask("CREATE TABLE iso_t (id INTEGER)"), "OK 0\n");
    assert_eq!(a.ask("INSERT INTO iso_t VALUES (1)"), "OK 1\n");

    assert_eq!(a.ask("BEGIN"), "OK 0\n");
    assert_eq!(a.ask("INSERT INTO iso_t VALUES (2)"), "OK 1\n");
    assert_eq!(b.ask("SELECT id AS id FROM iso_t ORDER BY id"), "ROWS 1\nid\n1\nEND 1\n");
    assert_eq!(a.ask("COMMIT"), "OK 0\n");
    assert_eq!(b.ask("SELECT id AS id FROM iso_t ORDER BY id"), "ROWS 1\nid\n1\n2\nEND 2\n");

    assert_eq!(a.ask("BEGIN"), "OK 0\n");
    assert_eq!(a.ask("INSERT INTO iso_t VALUES (3)"), "OK 1\n");
    assert_eq!(b.ask("SELECT COUNT(*) AS c FROM iso_t"), "ROWS 1\nc\n2\nEND 1\n");
    assert_eq!(a.ask("ROLLBACK"), "OK 0\n");
    assert_eq!(b.ask("SELECT COUNT(*) AS c FROM iso_t"), "ROWS 1\nc\n2\nEND 1\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn kill_during_transaction_keeps_only_commits() {
    let dir = scratch("kill");
    let server = Server::start(&dir);
    let mut c = Client::connect(server.port);
    assert_eq!(c.ask("CREATE TABLE kl_t (id INTEGER)"), "OK 0\n");
    assert_eq!(c.ask("INSERT INTO kl_t VALUES (1), (2)"), "OK 2\n");
    assert_eq!(c.ask("BEGIN"), "OK 0\n");
    assert_eq!(c.ask("INSERT INTO kl_t VALUES (3)"), "OK 1\n");
    drop(c);
    server.kill();

    let server = Server::start(&dir);
    let mut c = Client::connect(server.port);
    assert_eq!(
        c.ask("SELECT id AS id FROM kl_t ORDER BY id"),
        "ROWS 1\nid\n1\n2\nEND 2\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn acknowledged_rows_survive_kill_and_restart_twice() {
    let dir = scratch("acked");
    let mut server = Server::start(&dir);
    let mut c = Client::connect(server.port);
    assert_eq!(c.ask("CREATE TABLE ak_t (id INTEGER)"), "OK 0\n");
    for i in 1..=50 {
        assert_eq!(c.ask(&format!("INSERT INTO ak_t VALUES ({})", i)), "OK 1\n");
    }
    drop(c);
    server.kill();
    for _ in 0..2 {
        server = Server::start(&dir);
        let mut c = Client::connect(server.port);
        assert_eq!(scalar(&c.ask("SELECT COUNT(*) AS c FROM ak_t")), 50);
        drop(c);
        server.kill();
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn garbage_and_oversized_input_leave_the_server_serving() {
    let dir = scratch("garbage");
    let server = Server::start(&dir);

    let mut junk = Client::connect(server.port);
    junk.writer.write_all(b"\xff\xfe\x01\x02\n").expect("send junk");
    assert!(junk.answer().starts_with("ERR "));

    let mut partial = Client::connect(server.port);
    partial.writer.write_all(b"SELECT 1 AS one").expect("send partial");
    partial.writer.shutdown(Shutdown::Write).expect("half close");
    let mut rest = String::new();
    partial.reader.read_line(&mut rest).expect("partial answer");
    assert_eq!(rest, "ROWS 1\n");
    drop(partial);

    let mut big = Client::connect(server.port);
    let mut line = String::from("SELECT '");
    line.push_str(&"x".repeat(2 * 1024 * 1024));
    line.push_str("' AS s\n");
    big.writer.write_all(line.as_bytes()).expect("send big line");
    let answer = big.answer();
    assert!(answer.starts_with("ROWS 1\ns\nxxx"), "big line answer head");
    assert!(answer.ends_with("\nEND 1\n"), "big line answer tail");

    let mut fresh = Client::connect(server.port);
    assert_eq!(fresh.ask("SELECT 1 AS one"), "ROWS 1\none\n1\nEND 1\n");
    let _ = fs::remove_dir_all(&dir);
}
