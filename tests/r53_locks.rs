//! R53 F1: two real binary processes hold two workers' campaign locks at
//! once, in a temp runs dir.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn r53_f1_two_processes_two_locks() {
    let runs = std::env::temp_dir().join(format!(
        "r53_two_locks_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&runs);
    std::fs::create_dir_all(&runs).unwrap();
    let bin = env!("CARGO_BIN_EXE_subdollarbench");
    let spawn = |worker: u8, id: &str| {
        Kill(
            Command::new(bin)
                .args([
                    "campaign",
                    "lock-hold",
                    "--worker",
                    &worker.to_string(),
                    "--id",
                    id,
                    "--seconds",
                    "20",
                ])
                .env("SDB_RUNS_DIR", &runs)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn lock-hold"),
        )
    };
    let _a = spawn(0, "c0");
    let _b = spawn(1, "c1");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline
        && !(runs.join(".campaign.lock.w0").is_file() && runs.join(".campaign.lock.w1").is_file())
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    let holders = subdollarbench::campaign::live_lock_holders_in(&runs);
    assert_eq!(holders.len(), 2, "{holders:?}");
    let workers: Vec<u8> = holders.iter().map(|h| h.worker).collect();
    assert!(workers.contains(&0) && workers.contains(&1), "{workers:?}");
    let _ = std::fs::remove_dir_all(&runs);
}
