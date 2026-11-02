use super::*;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;

#[test]
fn r45_f2_score_math() {
    // Hand: 2.0 latency, 0.5 throughput -> GM 1.0. Clip extremes.
    assert!((ratio(10.0, 5.0, false) - 2.0).abs() < 1e-12);
    assert!((ratio(5.0, 10.0, true) - 2.0).abs() < 1e-12);
    assert_eq!(ratio(10.0, 0.01, false), CLIP_MAX);
    assert!((ratio(0.01, 10.0, false) - CLIP_MIN).abs() < 1e-12);
    let gm = geometric_mean(&[2.0, 0.5, 1.0]).unwrap();
    assert!((gm - 1.0).abs() < 1e-12, "{gm}");
    let score = PerfScore::from_workloads(vec![
        WorkloadResult {
            id: "W1".into(),
            metric: "latency".into(),
            reference: 10.0,
            candidate: 5.0,
            ratio: 2.0,
            correct: true,
            numbers: BTreeMap::new(),
            note: None,
            harness_fault: false,
        },
        WorkloadResult {
            id: "W4".into(),
            metric: "throughput".into(),
            reference: 5.0,
            candidate: 10.0,
            ratio: 2.0,
            correct: true,
            numbers: BTreeMap::new(),
            note: None,
            harness_fault: false,
        },
    ]);
    assert!((score.score.expect("a score") - 2.0).abs() < 1e-12);
    let wrong = PerfScore::from_workloads(vec![WorkloadResult {
        id: "W1".into(),
        metric: "latency".into(),
        reference: 10.0,
        candidate: 0.1,
        ratio: 100.0,
        correct: false,
        numbers: BTreeMap::new(),
        note: None,
        harness_fault: false,
    }]);
    assert!((wrong.score.expect("a floor score") - WRONG_FLOOR).abs() < 1e-12);
}

#[test]
fn r45_f2_phase_workloads() {
    assert_eq!(workloads_for_phase(1), ["W1", "W4"]);
    assert_eq!(workloads_for_phase(2), ["W1", "W3", "W4"]);
    assert_eq!(workloads_for_phase(3), ["W1", "W2", "W3", "W4", "W5"]);
    assert_eq!(phase_of_workload("W5"), Some(3));
}

/// A tiny in-process engine that stores `(id, v)` rows. It answers the
/// W1/W4 shapes correctly unless `wrong`, and sleeps `delay` before
/// every statement (the slow-candidate test).
#[derive(Default)]
struct FakeStore {
    tables: HashMap<String, Vec<(i64, String)>>,
}

fn fake_engine(delay: Duration, wrong: bool) -> std::net::SocketAddr {
    fake_engine_counted(delay, wrong, None)
}

/// R50 F6: the fake engine can expose its acknowledged inserts, so a
/// flush counter can be driven by the real commits.
fn fake_engine_counted(
    delay: Duration,
    wrong: bool,
    inserts: Option<Arc<AtomicU64>>,
) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Arc::new(Mutex::new(FakeStore::default()));
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let store = store.clone();
            let inserts = inserts.clone();
            std::thread::spawn(move || serve_fake(sock, store, delay, wrong, inserts));
        }
    });
    addr
}

fn answer_fake(store: &mut FakeStore, sql: &str, wrong: bool) -> String {
    if let Some(rest) = sql.strip_prefix("CREATE TABLE ") {
        let table = rest.split_whitespace().next().unwrap_or("").to_string();
        if store.tables.contains_key(&table) {
            return "ERR exists table already exists".to_string();
        }
        store.tables.insert(table, Vec::new());
        return "OK 0".to_string();
    }
    if let Some(rest) = sql.strip_prefix("INSERT INTO ") {
        let table = rest.split_whitespace().next().unwrap_or("").to_string();
        let vals = rest.split("VALUES").nth(1).unwrap_or("");
        let tuples: Vec<&str> = vals.split("),(").collect();
        let rows = store.tables.entry(table).or_default();
        for tuple in &tuples {
            let t = tuple.trim().trim_start_matches('(').trim_end_matches(')');
            let mut parts = t.splitn(2, ',');
            let id: i64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
            let v = parts
                .next()
                .unwrap_or("")
                .split('\'')
                .nth(1)
                .unwrap_or("")
                .to_string();
            rows.push((id, v));
        }
        return format!("OK {}", tuples.len());
    }
    if let Some(rest) = sql.strip_prefix("SELECT COUNT(*) FROM ") {
        let table = rest.trim();
        let n = store.tables.get(table).map(|r| r.len()).unwrap_or(0) as u64;
        let shown = if wrong { n + 1 } else { n };
        return format!("ROWS 1\nCOUNT(*)\n{shown}\nEND 1");
    }
    if let Some(rest) = sql.strip_prefix("SELECT v FROM ") {
        let table = rest.split_whitespace().next().unwrap_or("");
        if let Some(k) = rest
            .split("k = ")
            .nth(1)
            .or_else(|| rest.split("id = ").nth(1))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<i64>().ok())
        {
            let v = store
                .tables
                .get(table)
                .and_then(|rows| rows.iter().find(|(id, _)| *id == k))
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| format!("r{k}"));
            let v = if wrong { "WRONG".to_string() } else { v };
            return format!("ROWS 1\nv\n{v}\nEND 1");
        }
        if let Some(a) = rest
            .split("k >= ")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<i64>().ok())
        {
            let mut out = String::from("ROWS 1\nv\n");
            let n = if wrong { 1 } else { 20 };
            for j in 0..n {
                out.push_str(&format!("r{}\n", if wrong { 0 } else { a + j }));
            }
            out.push_str(&format!("END {n}"));
            return out;
        }
        return "ROWS 1\nv\nWRONG\nEND 1".to_string();
    }
    if sql.contains("JOIN") {
        let mut out = String::from("ROWS 3\ng\tcount\tsum\n");
        // The unit tests use the small config (100 b rows); the real
        // reference is a separate process and never goes through here.
        let rows = if wrong {
            vec![(0i64, 1i64, 1i64)]
        } else {
            join_expected(100)
        };
        for (g, count, sum) in &rows {
            out.push_str(&format!("{g}\t{count}\t{sum}\n"));
        }
        out.push_str(&format!("END {}", rows.len()));
        return out;
    }
    "ERR syntax fake".to_string()
}

fn serve_fake(
    sock: TcpStream,
    store: Arc<Mutex<FakeStore>>,
    delay: Duration,
    wrong: bool,
    inserts: Option<Arc<AtomicU64>>,
) {
    sock.set_nodelay(true).unwrap();
    let mut reader = BufReader::new(sock.try_clone().unwrap());
    let mut writer = sock;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let sql = line.trim_end();
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let reply = {
            let mut s = store.lock().unwrap();
            answer_fake(&mut s, sql, wrong)
        };
        if let Some(n) = &inserts {
            if sql.starts_with("INSERT INTO ") && reply.starts_with("OK ") {
                n.fetch_add(1, Ordering::Relaxed);
            }
        }
        if writer
            .write_all(reply.as_bytes())
            .and_then(|()| writer.write_all(b"\n"))
            .is_err()
        {
            return;
        }
    }
}

/// The reference fake is fast; the candidate sleeps 2 ms per statement.
/// W1 and W4 must see that as a score below 0.5.
#[test]
fn r45_f1_slow_candidate_scores_low() {
    let reference = fake_engine(Duration::ZERO, false);
    let candidate = fake_engine(Duration::from_millis(2), false);
    let cfg = PerfConfig::small_for_tests();
    let score = run_pair(&reference.to_string(), &candidate.to_string(), 1, &cfg);
    for id in ["W1", "W4"] {
        let w = score
            .workloads
            .iter()
            .find(|w| w.id == id)
            .unwrap_or_else(|| panic!("missing {id}"));
        assert!(
            w.ratio < 0.5,
            "{id} ratio {:.3} (correct {}): {:?}",
            w.ratio,
            w.correct,
            w.numbers
        );
    }
}

/// R50 F5: one whole disk is read; a device over two disks is refused.
#[cfg(unix)]
#[test]
fn r50_f5_flush_counter_one_disk() {
    use std::os::unix::fs::MetadataExt;
    fn stat16(v: u64) -> String {
        format!("{} {v}\n", "0 ".repeat(15))
    }
    let base = std::env::temp_dir().join(format!("sdb-flush-{}-{}", std::process::id(), nonce()));
    let _ = std::fs::remove_dir_all(&base);
    let data = base.join("data");
    let sysfs = base.join("sysfs");
    std::fs::create_dir_all(&data).unwrap();
    let dev = std::fs::metadata(&data).unwrap().dev();
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    let block = sysfs.join("devices/block");
    let dm = sysfs.join("devices/virtual/block/dm-0");
    std::fs::create_dir_all(dm.join("slaves")).unwrap();
    std::fs::create_dir_all(block.join("sda/sda1")).unwrap();
    std::fs::write(block.join("sda/stat"), stat16(4242)).unwrap();
    std::fs::write(dm.join("stat"), stat16(7)).unwrap();
    std::os::unix::fs::symlink(block.join("sda/sda1"), dm.join("slaves/sda1")).unwrap();
    std::fs::create_dir_all(sysfs.join("dev/block")).unwrap();
    std::os::unix::fs::symlink(&dm, sysfs.join(format!("dev/block/{major}:{minor}"))).unwrap();

    let one = DeviceFlushCounter::for_dir_at(&data, &sysfs).expect("one disk");
    assert_eq!(
        one.flushes().unwrap(),
        4242,
        "the disk's field 16, not dm's"
    );

    std::fs::create_dir_all(block.join("sdb/sdb1")).unwrap();
    std::fs::write(block.join("sdb/stat"), stat16(99)).unwrap();
    std::os::unix::fs::symlink(block.join("sdb/sdb1"), dm.join("slaves/sdb1")).unwrap();
    let err = DeviceFlushCounter::for_dir_at(&data, &sysfs)
        .expect_err("two disks must refuse")
        .to_string();
    assert!(err.contains("spans 2 disks"), "{err}");
    let _ = std::fs::remove_dir_all(&base);
}

/// R49 F2: a deterministic flush source for the W4 tests. The first
/// snapshot is 0; every later one is `after`.
struct StepFlush {
    after: u64,
    calls: AtomicU64,
}

impl StepFlush {
    fn new(after: u64) -> Self {
        Self {
            after,
            calls: AtomicU64::new(0),
        }
    }
}

impl FlushCounter for StepFlush {
    fn flushes(&self) -> Result<u64, String> {
        Ok(if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
            0
        } else {
            self.after
        })
    }
}

/// R50 F6: a flush counter fed by the fake engine's acknowledged
/// inserts, one per side.
struct EngineFlush(Arc<AtomicU64>);

impl FlushCounter for EngineFlush {
    fn flushes(&self) -> Result<u64, String> {
        Ok(self.0.load(Ordering::Relaxed))
    }
}

/// R49-3: both sides durable over two rounds means every 1-client
/// window's delta tracks its own commits; W4 is correct and the phase
/// keeps a score. StepFlush failed here because its second window read
/// zero.
#[test]
fn r50_f6_durable_pair_two_rounds() {
    let ref_inserts = Arc::new(AtomicU64::new(0));
    let cand_inserts = Arc::new(AtomicU64::new(0));
    let reference = fake_engine_counted(Duration::ZERO, false, Some(ref_inserts.clone()));
    let candidate = fake_engine_counted(Duration::ZERO, false, Some(cand_inserts.clone()));
    let cfg = PerfConfig {
        rounds: 2,
        ..PerfConfig::small_for_tests()
    };
    let score = run_pair_with_flush(
        &reference.to_string(),
        &candidate.to_string(),
        1,
        &cfg,
        Some(&EngineFlush(ref_inserts)),
        Some(&EngineFlush(cand_inserts)),
    );
    let w4 = score
        .workloads
        .iter()
        .find(|w| w.id == "W4")
        .expect("phase 1 runs W4");
    assert!(w4.correct, "the durable pair must pass W4: {:?}", w4.note);
    assert!(
        w4.numbers
            .get("candidate_1_client_commits")
            .copied()
            .unwrap_or(0.0)
            > 0.0
    );
    assert!(
        score.score.is_some(),
        "a durable pair keeps the phase scored: {:?}",
        score.error
    );
}

struct BrokenFlush;

impl FlushCounter for BrokenFlush {
    fn flushes(&self) -> Result<u64, String> {
        Err("no counter on this host".to_string())
    }
}

/// R49 F2: a candidate with fewer flushes than 0.9 x its acknowledged
/// 1-client commits floors W4; the reference passing keeps the phase
/// scored.
#[test]
fn r49_f2_flush_check_floors_unsynced() {
    let reference = fake_engine(Duration::ZERO, false);
    let candidate = fake_engine(Duration::ZERO, false);
    let cfg = PerfConfig::small_for_tests();
    let ref_flush = StepFlush::new(10_000_000);
    let cand_flush = StepFlush::new(1);
    let score = run_pair_with_flush(
        &reference.to_string(),
        &candidate.to_string(),
        1,
        &cfg,
        Some(&ref_flush),
        Some(&cand_flush),
    );
    let w4 = score
        .workloads
        .iter()
        .find(|w| w.id == "W4")
        .expect("phase 1 runs W4");
    assert!(!w4.correct, "the unsynced candidate must not pass W4");
    let note = w4.note.as_deref().unwrap_or("");
    assert!(note.contains("not durable"), "{note}");
    assert!(
        score.score.is_some(),
        "a candidate floor keeps the phase scored"
    );
}

/// R49 F2: an unreadable counter is a harness fault, not a floor.
#[test]
fn r49_f2_unreadable_counter_is_harness_fault() {
    let reference = fake_engine(Duration::ZERO, false);
    let candidate = fake_engine(Duration::ZERO, false);
    let cfg = PerfConfig::small_for_tests();
    let score = run_pair_with_flush(
        &reference.to_string(),
        &candidate.to_string(),
        1,
        &cfg,
        Some(&BrokenFlush),
        Some(&StepFlush::new(10_000_000)),
    );
    assert!(
        score.score.is_none(),
        "an unreadable counter must give no score: {:?}",
        score.score
    );
    let err = score.error.as_deref().unwrap_or("");
    assert!(err.contains("flush"), "{err}");
}

/// R49 F1: a perf data root on tmpfs/ramfs is a harness fault (no
/// score), because `fsync` would be free and W4 would measure memory.
#[test]
fn r49_f1_perf_data_not_in_memory() {
    let shm = std::path::PathBuf::from("/dev/shm").join(format!(
        "sdb-perf-mem-{}-{}",
        std::process::id(),
        nonce()
    ));
    let ws = std::env::temp_dir().join(format!("sdb-perf-ws-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("start.sh"), "#!/bin/sh\n").unwrap();
    let score = run_workspace_at(&ws, 4, &PerfConfig::small_for_tests(), &shm)
        .expect("the memory check must not need Docker");
    assert!(
        score.score.is_none(),
        "a tmpfs data root must not score: {:?}",
        score.score
    );
    let err = score.error.as_deref().unwrap_or("");
    assert!(err.contains("memory"), "{err}");
    let _ = std::fs::remove_dir_all(&shm);
    let _ = std::fs::remove_dir_all(&ws);
}

/// R45-2: a reference that never answers is a harness failure, not a
/// candidate floor. The block carries no score at all.
#[test]
fn r45_r2_reference_failure_is_not_a_score() {
    let candidate = fake_engine(Duration::ZERO, false);
    let cfg = PerfConfig::small_for_tests();
    let score = run_pair("127.0.0.1:1", &candidate.to_string(), 1, &cfg);
    assert!(
        score.score.is_none(),
        "a dead reference must not score the candidate: {:?}",
        score.score
    );
    let err = score.error.as_deref().unwrap_or("");
    assert!(err.contains("reference"), "reason: {err}");
    assert!(
        score.workloads.iter().any(|w| w.harness_fault),
        "the failed workload must say who failed: {:?}",
        score.workloads
    );
}

/// R45-2: a reference that answers wrong is our bug, not the
/// candidate's; it leaves no score.
#[test]
fn r45_r2_wrong_reference_is_not_a_score() {
    let reference = fake_engine(Duration::ZERO, true);
    let candidate = fake_engine(Duration::ZERO, false);
    let cfg = PerfConfig::small_for_tests();
    let score = run_pair(&reference.to_string(), &candidate.to_string(), 3, &cfg);
    assert!(
        score.score.is_none(),
        "a wrong reference must not score the candidate: {:?}",
        score.score
    );
    let err = score.error.as_deref().unwrap_or("");
    assert!(err.contains("reference"), "reason: {err}");
}

/// R50 F7 (R49-3's neighbour): a fast but wrong W3 stores the floor as
/// its ratio, keeps the measured ratio under `raw_ratio`, and says the
/// answer was wrong. The score is the same as before the change
/// (`from_workloads` already floored on `correct`).
#[test]
fn r50_f7_wrong_workload_stores_the_floor() {
    let reference = fake_engine(Duration::from_millis(1), false);
    let candidate = fake_engine(Duration::ZERO, true);
    let cfg = PerfConfig::small_for_tests();
    let score = run_pair(&reference.to_string(), &candidate.to_string(), 3, &cfg);
    let w3 = score
        .workloads
        .iter()
        .find(|w| w.id == "W3")
        .expect("phase 3 runs W3");
    assert!(!w3.correct, "a wrong answer must floor W3");
    assert_eq!(w3.ratio, WRONG_FLOOR, "the stored ratio is the floor");
    let raw = w3
        .numbers
        .get("raw_ratio")
        .copied()
        .expect("the measured ratio is kept");
    assert!(raw > 1.0, "raw_ratio {raw} must be the measured ratio");
    let note = w3.note.as_deref().unwrap_or("");
    assert!(
        note.contains("answered wrong") && note.contains("query"),
        "note: {note}"
    );
    // Same score as the pre-change block: from_workloads floors a wrong
    // workload whatever ratio the field holds.
    let mut before = score.workloads.clone();
    for w in &mut before {
        if let Some(v) = w.numbers.get("raw_ratio") {
            w.ratio = *v;
        }
    }
    assert_eq!(PerfScore::from_workloads(before).score, score.score);
}

/// R50 F7: a right but too-slow candidate gets the deadline note, not a
/// wrong-answer one.
#[test]
fn r50_f7_deadline_note_says_slow() {
    let reference = fake_engine(Duration::ZERO, false);
    let candidate = fake_engine(Duration::from_millis(5), false);
    let cfg = PerfConfig {
        rounds: 1,
        lookups: 1000,
        workload_deadline: Duration::from_millis(50),
        ..PerfConfig::small_for_tests()
    };
    let score = run_pair(&reference.to_string(), &candidate.to_string(), 3, &cfg);
    let w1 = score
        .workloads
        .iter()
        .find(|w| w.id == "W1")
        .expect("phase 1 runs W1");
    assert!(!w1.correct, "the deadline must floor W1");
    let note = w1.note.as_deref().unwrap_or("");
    assert!(
        note.starts_with("too slow:"),
        "the note says the server was slow: {note}"
    );
    assert!(note.contains("deadline"), "{note}");
    assert!(!note.contains("answered wrong"), "{note}");
}

/// A fast engine that answers one wrong value per workload scores the
/// floor on every workload, whatever its speed.
#[test]
fn r45_f1_wrong_answer_is_floor() {
    let reference = fake_engine(Duration::ZERO, false);
    let candidate = fake_engine(Duration::ZERO, true);
    let cfg = PerfConfig::small_for_tests();
    let score = run_pair(&reference.to_string(), &candidate.to_string(), 3, &cfg);
    assert_eq!(score.workloads.len(), 5);
    for w in &score.workloads {
        assert!(!w.correct, "{} was graded correct", w.id);
    }
    let value = score
        .score
        .expect("a wrong candidate still gets a floored score");
    assert!((value - WRONG_FLOOR).abs() < 1e-12, "score {value:.3}");
}
