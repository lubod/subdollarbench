use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

/// A fake engine: answers the protocol without any storage. The scan
/// table sleeps `scan_delay` per lookup, the indexed table
/// `index_delay`, the one-row floor table `floor_delay` — the table
/// name's last letter picks the behaviour (…s / …f / …o), which is how
/// the gate names them.
fn fake_engine(
    scan_delay: Duration,
    index_delay: Duration,
    floor_delay: Duration,
) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        let (sock, _) = l.accept().unwrap();
        // Without nodelay the small response writes pay a delayed-ACK
        // stall (~40 ms) per statement, which swamps the sleeps.
        sock.set_nodelay(true).unwrap();
        let mut w = sock.try_clone().unwrap();
        let mut r = BufReader::new(sock);
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let sql = line.trim_end();
            let reply = if sql.starts_with("CREATE TABLE") || sql.starts_with("CREATE INDEX") {
                "OK 0".to_string()
            } else if sql.starts_with("INSERT INTO") {
                format!("OK {}", sql.matches("),(").count() + 1)
            } else if let Some(rest) = sql.strip_prefix("SELECT v FROM ") {
                let table = rest.split(' ').next().unwrap_or("");
                let k = rest.rsplit(" = ").next().unwrap_or("0");
                let delay = if table.ends_with('s') {
                    scan_delay
                } else if table.ends_with('o') {
                    floor_delay
                } else {
                    index_delay
                };
                std::thread::sleep(delay);
                format!("ROWS 1\nv\nr{k}\nEND 1")
            } else {
                "ERR syntax fake".to_string()
            };
            if w.write_all(reply.as_bytes())
                .and_then(|()| w.write_all(b"\n"))
                .is_err()
            {
                return;
            }
        }
    });
    addr
}

fn small_params() -> Params {
    Params {
        rows: 200,
        batch: 100,
        lookups: 50,
        ranges: 0,
        range_rows: 4,
    }
}

/// A scan too fast to show 10x (4 ms vs 0.5 ms is ~8x) passes when the
/// index sits at the round-trip floor: the rule must not punish a
/// compiled engine for scanning quickly.
#[test]
fn a_fast_scan_with_an_index_at_the_floor_passes() {
    let addr = fake_engine(
        Duration::from_micros(4_000),
        Duration::from_micros(500),
        Duration::from_micros(500),
    );
    let report = perf_gate_with(
        small_params(),
        &addr.to_string(),
        Duration::from_secs(5),
        7,
        true,
        None,
    )
    .expect("the floor rule keeps a fast scan passing");
    assert!(report.contains("floor"), "{report}");
}

/// Round-7 finding 2: a constant per-statement overhead lifts the
/// floor, and it must not lift the allowance with it. Scan and index
/// are equally slow here (no index at all); both pay 1 ms of overhead.
#[test]
fn a_missing_index_hidden_by_overhead_fails() {
    let addr = fake_engine(
        Duration::from_micros(1_500),
        Duration::from_micros(1_500),
        Duration::from_micros(1_000),
    );
    let err = perf_gate_with(
        small_params(),
        &addr.to_string(),
        Duration::from_secs(5),
        7,
        true,
        None,
    )
    .unwrap_err();
    assert!(err.contains("slack") || err.contains("net work"), "{err}");
}

/// Without a working index the "indexed" side is as slow as the scan:
/// the gate must still fail (the floor rule is not a free pass).
#[test]
fn an_index_as_slow_as_the_scan_still_fails() {
    let addr = fake_engine(
        Duration::from_micros(4_000),
        Duration::from_micros(4_000),
        Duration::from_micros(500),
    );
    let err = perf_gate_with(
        small_params(),
        &addr.to_string(),
        Duration::from_secs(5),
        7,
        true,
        None,
    )
    .unwrap_err();
    assert!(err.contains("floor"), "{err}");
}
