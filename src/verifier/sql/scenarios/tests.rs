use super::super::compare::compare;
use super::super::generator::{build_script, Profile};
use super::super::oracle::Oracle;
use super::*;
use std::io::BufRead;
use std::net::TcpListener;
use std::thread;

/// A server that reads `n` statements *before* answering any of them —
/// it only makes progress if the client really pipelines — then answers
/// in the received order.
fn burst_server<F>(n: usize, answer: F) -> std::net::SocketAddr
where
    F: FnOnce(Vec<String>) -> Vec<String> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_nodelay(true).unwrap();
        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut lines = Vec::new();
        let mut line = String::new();
        while lines.len() < n {
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            lines.push(line.trim_end().to_string());
            line.clear();
        }
        for reply in answer(lines) {
            sock.write_all(reply.as_bytes()).unwrap();
            sock.write_all(b"\n").unwrap();
        }
    });
    addr
}

#[test]
fn pipelined_statements_are_answered_in_order() {
    let addr = burst_server(3, |lines| {
        assert_eq!(lines, vec!["one", "two", "three"]);
        vec![
            "OK 1".to_string(),
            "ROWS 1\nv\n7\nEND 1".to_string(),
            "ERR no_table gone".to_string(),
        ]
    });
    let lines: Vec<String> = ["one", "two", "three"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let got = pipeline(addr, &lines, Duration::from_secs(5)).unwrap();
    assert_eq!(got[0], Response::Ok(1));
    match &got[1] {
        Response::Rows { rows, .. } => assert_eq!(*rows, vec![vec!["7".to_string()]]),
        other => panic!("{other:?}"),
    }
    match &got[2] {
        Response::Err { class, .. } => assert_eq!(class, "no_table"),
        other => panic!("{other:?}"),
    }
}

/// Pipelining a real generated script: the reference answers every line
/// in order, and every answer grades clean against the oracle.
#[test]
fn pipelined_script_answers_match_the_oracle() {
    let script = build_script(3, &Profile::through(3), 40);
    let checked: Vec<crate::verifier::sql::generator::Checked> =
        script.setup.iter().chain(&script.steps).cloned().collect();
    let lines: Vec<String> = checked.iter().map(|c| c.stmt.sql.clone()).collect();
    let ordered: Vec<bool> = checked.iter().map(|c| c.stmt.ordered).collect();
    let sent = lines.clone();
    let addr = burst_server(lines.len(), move |got| {
        assert_eq!(got, sent, "the server received the wrong lines");
        let oracle = Oracle::new().unwrap();
        checked
            .iter()
            .map(|c| {
                let out = oracle.exec(&c.stmt.sql);
                assert!(!out.is_discard(), "ungradeable: {out:?}");
                super::super::compare::render_expected(&out)
            })
            .collect()
    });
    let got = pipeline(addr, &lines, Duration::from_secs(10)).unwrap();
    assert_eq!(got.len(), lines.len());
    let oracle = Oracle::new().unwrap();
    for ((reply, line), ordered) in got.iter().zip(&lines).zip(&ordered) {
        let expected = oracle.exec(line);
        compare(&expected, reply, *ordered).expect("pipelined answer must grade clean");
    }
}

#[test]
fn a_server_that_never_answers_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_secs(20));
        drop(sock);
    });
    let lines = vec!["SELECT 1".to_string()];
    let err = pipeline(addr, &lines, Duration::from_millis(300)).unwrap_err();
    assert!(err.starts_with("read:"), "{err}");
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// A correct server: one database, connections served in parallel.
    Good,
    /// Serves one connection at a time.
    Serial,
    /// Stops answering anyone after receiving a line over 1 MiB.
    DiesOnLongLine,
}

fn oracle_server(mode: Mode) -> String {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let oracle = Arc::new(Mutex::new(Oracle::new().unwrap()));
    let dead = Arc::new(AtomicBool::new(false));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(sock) = stream else { break };
            if dead.load(Ordering::SeqCst) {
                drop(sock);
                continue;
            }
            let (oracle, dead) = (oracle.clone(), dead.clone());
            let serve = move || {
                let mut sock = sock;
                sock.set_nodelay(true).unwrap();
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut line = String::new();
                while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                    if mode == Mode::DiesOnLongLine && line.len() > 1 << 20 {
                        dead.store(true, Ordering::SeqCst);
                        return;
                    }
                    let out = oracle.lock().unwrap().exec(line.trim_end());
                    let text = if out.is_discard() {
                        "ERR syntax".to_string()
                    } else {
                        super::super::compare::render_expected(&out)
                    };
                    if sock.write_all(format!("{text}\n").as_bytes()).is_err() {
                        return;
                    }
                    line.clear();
                }
            };
            if mode == Mode::Serial {
                serve();
            } else {
                thread::spawn(serve);
            }
        }
    });
    addr
}

#[test]
fn stage0_scenarios_pass_against_a_correct_server() {
    let addr = oracle_server(Mode::Good);
    for seed in [1u64, 2, 3] {
        assert_eq!(
            stage0(&addr, Duration::from_secs(10), seed),
            ScenarioResult::Pass,
            "seed {seed}"
        );
    }
}

#[test]
fn stage0_catches_a_one_client_at_a_time_server() {
    let addr = oracle_server(Mode::Serial);
    match stage0(&addr, Duration::from_millis(400), 4) {
        ScenarioResult::Fail(d) => assert!(d.contains("8 concurrent clients"), "{d}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn stage0_catches_a_server_killed_by_a_long_line() {
    let addr = oracle_server(Mode::DiesOnLongLine);
    match stage0(&addr, Duration::from_secs(2), 5) {
        ScenarioResult::Fail(d) => assert!(d.contains("after a 2 MiB line"), "{d}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn later_stages_have_no_scenarios_yet() {
    // Nothing listens here; a stage without scenarios never connects.
    assert_eq!(
        for_stage(3, "127.0.0.1:1", Duration::from_millis(100), 0),
        ScenarioResult::Pass
    );
}
