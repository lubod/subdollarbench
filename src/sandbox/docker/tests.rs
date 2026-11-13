/// Free loopback port for tests: fixed 5998x/5999x literals let
/// concurrent test processes (and this process's own parallel tests)
/// steal each other's binds. Stays above the live port map (<10000) and
/// below the legacy fixed-test range (59980).
fn free_port() -> u16 {
    loop {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        if p > 20000 && p < 59980 {
            return p;
        }
    }
}

/// Dashboard plumbing: extra ports must publish 1:1 on loopback, and
/// the ENOSPC extras must stay single-purpose.
#[test]
fn test_candidate_extras_args() {
    use super::{CandidateRunExtras, SandboxManager};
    let extras = CandidateRunExtras {
        tmpfs: vec!["/data:size=8m".to_string()],
        env: vec![("SDB_DATA_DIR".to_string(), "/data".to_string())],
        ports: vec![8090],
        memory: Some("64m".to_string()),
        cpuset: None,
        binds: vec![("/tmp/host-data".to_string(), "/data".to_string())],
        launch_args: Vec::new(),
        pids_limit: None,
    };
    let args = SandboxManager::candidate_extras_args(&extras);
    assert!(args.windows(2).any(|w| w == ["--tmpfs", "/data:size=8m"]));
    assert!(args.windows(2).any(|w| w == ["-e", "SDB_DATA_DIR=/data"]));
    assert!(args.windows(2).any(|w| w == ["-p", "127.0.0.1:8090:8090"]));
    // Disk-residency/compaction stages bind a host dir for footprint reads.
    assert!(args.windows(2).any(|w| w == ["-v", "/tmp/host-data:/data"]));
    let mut pinned = extras.clone();
    pinned.cpuset = Some("0-3".to_string());
    let args = SandboxManager::candidate_extras_args(&pinned);
    assert!(args.windows(2).any(|w| w == ["--cpuset-cpus", "0-3"]));
    let none = CandidateRunExtras::default();
    assert!(SandboxManager::candidate_extras_args(&none).is_empty());
}

/// Ticket 10 launch contract: node args publish 1:1 on loopback, node 1
/// is the initial master and carries the dashboard port, nodes 2/3
/// point at ha1, and the rejoin form points wherever asked.
#[test]
fn test_ha_node_args_launch_contract() {
    use super::SandboxManager;
    let dir = std::path::Path::new("/tmp/ws");
    let (args1, launch1) = SandboxManager::ha_node_args(
        "id",
        "ha1-id",
        "net",
        dir,
        1,
        7001,
        None,
        Some(8090),
        &[],
        None,
    );
    assert!(launch1.contains("--port 7001 --data /data1"), "{launch1}");
    assert!(!launch1.contains("--replicaof"), "{launch1}");
    assert!(args1.windows(2).any(|w| w == ["-p", "127.0.0.1:7001:7001"]));
    assert!(args1.windows(2).any(|w| w == ["-p", "127.0.0.1:8090:8090"]));
    assert!(args1.windows(2).any(|w| w == ["--network-alias", "ha1"]));

    let (args2, launch2) = SandboxManager::ha_node_args(
        "id",
        "ha2-id",
        "net",
        dir,
        2,
        7002,
        Some(("ha1", 7001)),
        None,
        &[],
        None,
    );
    assert!(launch2.contains("--replicaof ha1:7001"), "{launch2}");
    assert!(args2.windows(2).any(|w| w == ["-p", "127.0.0.1:7002:7002"]));
    assert!(!args2.windows(2).any(|w| w == ["-p", "127.0.0.1:8090:8090"]));

    // Rejoin form: node 1 pointed at the new leader alias/port.
    let (_args, launch_rejoin) = SandboxManager::ha_node_args(
        "id",
        "ha1-id",
        "net",
        dir,
        1,
        7001,
        Some(("ha3", 7003)),
        None,
        &[],
        None,
    );
    assert!(
        launch_rejoin.contains("--replicaof ha3:7003"),
        "{launch_rejoin}"
    );
}

#[test]
fn test_egress_proxy_allowlist_blocks_exfil() {
    use super::egress_allowed;
    for h in ["openrouter.ai", "api.openrouter.ai", "a.api.openrouter.ai"] {
        assert!(egress_allowed(h), "{}", h);
    }
    for h in [
        "evil.com",
        "1.2.3.4",
        "openrouter.ai.evil.com",
        "notopenrouter.ai",
        "crates.io",
        "pypi.org",
        "github.com",
        "",
    ] {
        assert!(!egress_allowed(h), "{}", h);
    }
}

/// R81 F2: the allowlist is exactly the two model-API hosts.
#[test]
fn r81_f2_allowlist_is_api_only() {
    assert_eq!(
        super::EGRESS_ALLOWLIST,
        &["openrouter.ai", "api.openrouter.ai"]
    );
    for h in ["openrouter.ai", "api.openrouter.ai"] {
        assert!(super::egress_allowed(h), "{h}");
    }
    for h in [
        "crates.io",
        "static.crates.io",
        "index.crates.io",
        "registry.npmjs.org",
        "proxy.golang.org",
        "pypi.org",
        "files.pythonhosted.org",
        "github.com",
        "objects.githubusercontent.com",
        "codeload.github.com",
    ] {
        assert!(!super::egress_allowed(h), "{h} must be blocked");
    }
}

/// R81 F2: through the per-run proxy, the registries and GitHub answer
/// 403 while the model API connects. This runs the real proxy sidecar on
/// the real run network, so it proves the env the agent gets, not just
/// the host matcher.
#[test]
fn r81_f2_registries_blocked() {
    use std::process::Command;

    let sm = SandboxManager::with_id(&format!("r81-f2-{}", std::process::id()));
    let _guard = RunGuard::new(SandboxManager::with_id(&format!(
        "r81-f2-{}",
        std::process::id()
    )));
    sm.cleanup();
    let proxy = sm
        .start_egress_proxy()
        .expect("the egress proxy must start");
    assert!(proxy.contains("egress-proxy:3128"), "{proxy}");
    let net = sm.network_name();

    let curl = |url: &str| -> (bool, String) {
        let out = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                &net,
                "-e",
                "https_proxy=http://egress-proxy:3128",
                "-e",
                "HTTPS_PROXY=http://egress-proxy:3128",
                "--entrypoint",
                "curl",
                "subdollar-sandbox",
                "-sS",
                "--max-time",
                "30",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                url,
            ])
            .output()
            .expect("docker run curl");
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    };

    for blocked in [
        "https://crates.io/",
        "https://pypi.org/",
        "https://github.com/",
    ] {
        let (ok, out) = curl(blocked);
        assert!(!ok, "{blocked} must be refused by the proxy: {out}");
        assert!(
            out.contains("403") || out.contains("Forbidden"),
            "{blocked} must show the proxy's 403: {out}"
        );
    }
    let (ok, out) = curl("https://openrouter.ai/");
    assert!(ok, "openrouter.ai must connect through the proxy: {out}");

    sm.cleanup();
}

use super::*;
use tokio::net::TcpListener;

#[test]
fn test_ensure_runnable_candidate_scenarios() {
    let temp_dir = std::env::temp_dir().join(format!("test_docker_sb_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let sm = SandboxManager::new();

    // R81 F3: only the root ./start.sh is a runnable candidate.
    assert!(!sm.ensure_runnable_candidate(&temp_dir).unwrap());

    fs::write(temp_dir.join("Dockerfile"), "FROM alpine").unwrap();
    assert!(!sm.ensure_runnable_candidate(&temp_dir).unwrap());
    let _ = fs::remove_file(temp_dir.join("Dockerfile"));

    let nested = temp_dir.join("subdir/nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("Dockerfile"), "FROM alpine").unwrap();
    fs::write(
        nested.join("start.sh"),
        "#!/bin/sh\nexec ./target/release/s\n",
    )
    .unwrap();
    assert!(!sm.ensure_runnable_candidate(&temp_dir).unwrap());
    assert!(!temp_dir.join("Dockerfile").exists());
    let _ = fs::remove_dir_all(&nested);

    fs::write(
        temp_dir.join("start.sh"),
        "#!/bin/bash\nexec ./target/release/s\n",
    )
    .unwrap();
    assert!(sm.ensure_runnable_candidate(&temp_dir).unwrap());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_wait_for_port() {
    let sm = SandboxManager::new();

    // 1. Unbound port should return false
    let ok = sm.wait_for_port(free_port(), 1).await;
    assert!(!ok);

    // 2. Active port should return true
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let ok2 = sm.wait_for_port(port, 2).await;
    assert!(ok2);

    // 3. Port that accepts and immediately closes (simulating dead backend across proxy)
    let listener_rst = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_rst = listener_rst.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener_rst.accept().await {
            drop(stream); // Immediate close / EOF
        }
    });
    let ok3 = sm.wait_for_port(port_rst, 1).await;
    assert!(
        !ok3,
        "Port that drops connections immediately should not be considered ready"
    );
}

#[test]
fn test_docker_cleanup_and_logs() {
    let sm = SandboxManager::with_id("test-cleanup");
    let _guard = RunGuard::new(SandboxManager::with_id("test-cleanup"));
    sm.cleanup();
    let logs = sm.get_candidate_logs();
    assert!(!logs.is_empty());
}

/// The sdb-sql reference must answer the text protocol through its
/// `ref-sql` alias on the run network (that is how the agent uses it).
#[test]
fn test_run_network_sql_reference_alias_reachable() {
    let sm = SandboxManager::with_id("net-sql-alias-test");
    let _guard = RunGuard::new(SandboxManager::with_id("net-sql-alias-test"));
    sm.cleanup();
    assert!(sm.ensure_network().is_ok());
    assert!(sm.start_reference_sql(free_port()).is_ok());
    // Give the reference a moment to accept connections.
    std::thread::sleep(std::time::Duration::from_millis(2500));

    let net = sm.network_name();
    let probe = "import socket; s = socket.create_connection(('ref-sql', 5480), 5); \
                     s.sendall(b'SELECT 1\\n'); print(s.recv(200).decode())";
    let out = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            &net,
            EGRESS_PROXY_IMAGE,
            "python3",
            "-c",
            probe,
        ])
        .output();
    sm.cleanup();

    let stdout = out
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    assert!(
        stdout.contains("ROWS 1") && stdout.contains("END 1"),
        "expected a ROWS frame via ref-sql, got: {stdout:?}"
    );
}

/// The sdb-sql launch contract: `--port`/`--data` reach the candidate's
/// argv and the harness-owned bind mount is writable from the container
/// (proved by reading the file back on the host).
#[test]
fn test_candidate_launch_args_and_data_bind() {
    use super::CandidateRunExtras;
    let sm = SandboxManager::with_id("launch-args-test");
    let _guard = RunGuard::new(SandboxManager::with_id("launch-args-test"));
    sm.cleanup();
    let work = std::env::temp_dir().join(format!("test_launch_args_{}", std::process::id()));
    let data = std::env::temp_dir().join(format!("test_launch_data_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        work.join("start.sh"),
        "#!/bin/bash\nprintf '%s\\n' \"$@\" > /workspace/args.txt\n\
             echo hi > /sdb-data/marker\nexec sleep 300\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            work.join("start.sh"),
            std::fs::Permissions::from_mode(0o755),
        );
    }
    let extras = CandidateRunExtras {
        binds: vec![(data.display().to_string(), "/sdb-data".to_string())],
        launch_args: vec![
            "--port".to_string(),
            "5480".to_string(),
            "--data".to_string(),
            "/sdb-data".to_string(),
        ],
        ..Default::default()
    };
    assert!(sm
        .start_candidate_in_docker_with(&work, free_port(), &extras)
        .is_ok());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline
        && !(work.join("args.txt").exists() && data.join("marker").exists())
    {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let args = std::fs::read_to_string(work.join("args.txt")).unwrap_or_default();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        vec!["--port", "5480", "--data", "/sdb-data"],
        "the candidate must receive the launch contract args"
    );
    assert!(
        data.join("marker").exists(),
        "the harness-owned data dir must be writable from the container"
    );
    sm.cleanup();
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&data);
}

/// The recommended `exec` form: the server *is* PID 1, so `kill(-1)`
/// never reaches it (`docker stop -t 1` gave it a single second after
/// a wasted 5 s wait). The graceful stop must signal PID 1 directly,
/// then give it the whole timeout to flush.
#[test]
fn test_graceful_stop_reaches_an_exec_pid1_server() {
    use super::CandidateRunExtras;
    let sm = SandboxManager::with_id("exec-stop-test");
    let _guard = RunGuard::new(SandboxManager::with_id("exec-stop-test"));
    sm.cleanup();
    let work = std::env::temp_dir().join(format!("test_exec_{}", std::process::id()));
    let data = std::env::temp_dir().join(format!("test_exec_data_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        work.join("trap_server.py"),
        r#"import signal, sys, time

def bye(signum, frame):
    time.sleep(2)  # a slow flush, well over `docker stop -t 1`
    open('/sdb-data/flushed.txt', 'w').write('bye')
    sys.exit(0)

signal.signal(signal.SIGTERM, bye)
while True:
    time.sleep(0.2)
"#,
    )
    .unwrap();
    // The recommended form: `exec`, so python is PID 1.
    std::fs::write(
        work.join("start.sh"),
        "#!/bin/bash\nexec python3 /workspace/trap_server.py\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            work.join("start.sh"),
            std::fs::Permissions::from_mode(0o755),
        );
    }
    let extras = CandidateRunExtras {
        binds: vec![(data.display().to_string(), "/sdb-data".to_string())],
        ..Default::default()
    };
    assert!(sm
        .start_candidate_in_docker_with(&work, free_port(), &extras)
        .is_ok());
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let started = std::time::Instant::now();
    assert!(sm.stop_candidate_gracefully(10).is_ok());
    let took = started.elapsed();
    assert!(
        data.join("flushed.txt").exists(),
        "PID 1 must receive SIGTERM and get its time to flush"
    );
    assert!(
        took < std::time::Duration::from_secs(6),
        "the stop waited the deadline ({took:?}) instead of the signal"
    );
    sm.cleanup();
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&data);
}

/// A `start.sh` *without* `exec` leaves the server as a child of a
/// shell that is PID 1; `docker stop` would signal only that shell, and
/// PID 1 ignores an unhandled SIGTERM. The graceful stop must reach the
/// server anyway (the flush marker is written).
#[test]
fn test_graceful_stop_reaches_a_non_exec_start_sh() {
    use super::CandidateRunExtras;
    let sm = SandboxManager::with_id("noexec-stop-test");
    let _guard = RunGuard::new(SandboxManager::with_id("noexec-stop-test"));
    sm.cleanup();
    let work = std::env::temp_dir().join(format!("test_noexec_{}", std::process::id()));
    let data = std::env::temp_dir().join(format!("test_noexec_data_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        work.join("trap_server.py"),
        r#"import signal, sys, time

def bye(signum, frame):
    open('/sdb-data/flushed.txt', 'w').write('bye')
    sys.exit(0)

signal.signal(signal.SIGTERM, bye)
while True:
    time.sleep(0.2)
"#,
    )
    .unwrap();
    // No `exec`: bash stays PID 1 and waits for python.
    std::fs::write(
        work.join("start.sh"),
        "#!/bin/bash\npython3 /workspace/trap_server.py\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            work.join("start.sh"),
            std::fs::Permissions::from_mode(0o755),
        );
    }
    let extras = CandidateRunExtras {
        binds: vec![(data.display().to_string(), "/sdb-data".to_string())],
        ..Default::default()
    };
    assert!(sm
        .start_candidate_in_docker_with(&work, free_port(), &extras)
        .is_ok());
    std::thread::sleep(std::time::Duration::from_millis(1500));
    // The fixture must really be serving before the stop.
    let ps = Command::new("docker")
        .args(["exec", &sm.candidate_container_name(), "ps", "-ef"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    assert!(
        ps.contains("trap_server.py"),
        "the fixture is not running: {ps}"
    );
    assert!(sm.stop_candidate_gracefully(5).is_ok());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !data.join("flushed.txt").exists() {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(
        data.join("flushed.txt").exists(),
        "SIGTERM must reach the server behind a non-exec start.sh"
    );
    sm.cleanup();
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&data);
}

/// SIGTERM reaches the candidate on a graceful stop (ticket 4's
/// "a few seconds to flush" contract); `stop_candidate` is `rm -f`, so
/// only the graceful stop may produce the flush marker.
#[test]
fn test_graceful_stop_lets_the_candidate_flush() {
    use super::CandidateRunExtras;
    let sm = SandboxManager::with_id("graceful-stop-test");
    let _guard = RunGuard::new(SandboxManager::with_id("graceful-stop-test"));
    sm.cleanup();
    let work = std::env::temp_dir().join(format!("test_graceful_{}", std::process::id()));
    let data = std::env::temp_dir().join(format!("test_graceful_data_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        work.join("start.sh"),
        "#!/bin/bash\ntrap 'echo bye > /sdb-data/flushed.txt; exit 0' TERM\n\
             while true; do sleep 0.2; done\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            work.join("start.sh"),
            std::fs::Permissions::from_mode(0o755),
        );
    }
    let extras = CandidateRunExtras {
        binds: vec![(data.display().to_string(), "/sdb-data".to_string())],
        ..Default::default()
    };
    assert!(sm
        .start_candidate_in_docker_with(&work, free_port(), &extras)
        .is_ok());
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(sm.stop_candidate_gracefully(5).is_ok());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !data.join("flushed.txt").exists() {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(
        data.join("flushed.txt").exists(),
        "SIGTERM must reach the candidate before it is removed"
    );
    sm.cleanup();
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn test_relax_workspace_perms_opens_any_uid_write() {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir().join(format!("test_relax_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let sub = base.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("aof.log"), b"x").unwrap();
    std::fs::set_permissions(sub.join("aof.log"), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hostname", sub.join("link")).ok();
    std::fs::write(sub.join("exec.sh"), b"x").ok();
    std::fs::set_permissions(sub.join("exec.sh"), std::fs::Permissions::from_mode(0o755)).ok();
    SandboxManager::relax_workspace_perms(&base);
    let fmode = std::fs::metadata(sub.join("aof.log"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        fmode, 0o666,
        "data files must be world-writable (any candidate UID)"
    );
    let xmode = std::fs::metadata(sub.join("exec.sh"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        xmode, 0o777,
        "executables must keep +x while gaining rw (forced 0666 broke exec)"
    );
    let dmode = std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        dmode, 0o777,
        "dirs must be world-rwx (runtime file creation)"
    );
    // Symlink itself must survive untouched (never followed into).
    assert!(std::fs::symlink_metadata(sub.join("link"))
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn test_start_candidate_in_docker_start_sh_only() {
    let temp_dir = std::env::temp_dir().join(format!("test_docker_run_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let sm = SandboxManager::with_id("test-cand");
    let _guard = RunGuard::new(SandboxManager::with_id("test-cand"));

    // R81 F3: ./start.sh is the only entrypoint.
    fs::write(temp_dir.join("start.sh"), "#!/bin/bash\nexec sleep 300\n").unwrap();
    let res_sh = sm.start_candidate_in_docker(&temp_dir, free_port());
    assert!(res_sh.is_ok());
    sm.cleanup();

    // A Dockerfile is no longer a runnable candidate: the container still
    // starts (the harness only starts what ensure_runnable_candidate
    // accepted), but the entrypoint check is false.
    let _ = fs::remove_file(temp_dir.join("start.sh"));
    fs::write(
        temp_dir.join("Dockerfile"),
        "FROM alpine\nCMD [\"echo\", \"done\"]\n",
    )
    .unwrap();
    assert!(!sm.ensure_runnable_candidate(&temp_dir).unwrap());

    let _ = fs::remove_dir_all(&temp_dir);
}
#[test]
fn test_prune_skips_attached_networks_by_container_count() {
    // Old gate retired by network_is_stale (count + age); coverage moved
    // to test_prune_age_gate below. Kept as documentation of intent.
}

#[test]
fn test_prune_age_gate() {
    // 0 containers + old timestamp = stale.
    assert!(SandboxManager::network_is_stale(
        "0 2020-01-01 00:00:00.000 +0000 UTC"
    ));
    // Attached containers never stale, however old.
    assert!(!SandboxManager::network_is_stale(
        "2 2020-01-01 00:00:00.000 +0000 UTC"
    ));
    // Young empty network = not yet attached (parallel workers), keep.
    // Far-future timestamp is always young.
    assert!(!SandboxManager::network_is_stale(
        "0 2999-01-01 00:00:00.000 +0000 UTC"
    ));
    // Unparseable fails closed.
    assert!(!SandboxManager::network_is_stale(""));
    assert!(!SandboxManager::network_is_stale("<no value>"));
    assert!(!SandboxManager::network_is_stale("0"));
}

#[test]
fn test_pool_exhaustion_wordings() {
    // Stock dockerd + distro/LXD rewordings all match; unknown fails open.
    assert!(SandboxManager::is_pool_exhausted(
            "Error response from daemon: could not find an available, non-overlapping IPv4 address pool among the defaults to assign to the network"
        ));
    assert!(SandboxManager::is_pool_exhausted(
        "Error response from daemon: all predefined address pools have been fully subnetted"
    ));
    assert!(SandboxManager::is_pool_exhausted("no available network"));
    // Subnet collision is a different retry path (wheel bump), not exhaustion.
    assert!(!SandboxManager::is_pool_exhausted(
        "Pool overlaps with other one on this address space"
    ));
    assert!(!SandboxManager::is_pool_exhausted(
        "Error response from daemon: network not found"
    ));
    assert!(!SandboxManager::is_pool_exhausted(""));
}

#[test]
fn test_run_subnet_deterministic_and_spread() {
    // Distinct nets spread (no mass collision onto one /24).
    let mut set = std::collections::HashSet::new();
    for i in 0..50 {
        set.insert(SandboxManager::run_subnet_attempt(
            &format!("subdollar-net-run{i}"),
            0,
        ));
    }
    assert!(set.len() > 25, "subnets collapse: {}", set.len());
    // Attempt salt moves the subnet (collision retry is real).
    assert_ne!(
        SandboxManager::run_subnet_attempt("subdollar-net-abc", 0),
        SandboxManager::run_subnet_attempt("subdollar-net-abc", 1)
    );
    // All inside our own wheel.
    for sn in &set {
        assert!(sn.starts_with("10.201."), "foreign subnet: {sn}");
        assert!(sn.ends_with("/24"), "bad mask: {sn}");
    }
}

#[test]
fn test_prune_parses_network_id_list() {
    assert!(SandboxManager::parse_network_ids("").is_empty());
    assert!(SandboxManager::parse_network_ids("   \n").is_empty());
    let ids = SandboxManager::parse_network_ids("abc123\ndef456\n");
    assert_eq!(ids, vec!["abc123", "def456"]);
}

#[test]
fn test_owned_names_cover_sql_egress_and_agent() {
    let sm = SandboxManager::with_id("parent_20261002_120000_trial2");
    let names = sm.owned_container_names();
    // Round 43 retired the pdf ref and the upstream pool; the SQL
    // reference, the egress sidecar and the agent must still be named
    // so trial cleanup reaps them.
    for want in [
        "subdollar-ref-sql-parent_20261002_120000_trial2",
        "subdollar-egress-parent_20261002_120000_trial2",
        "subdollar-candidate-parent_20261002_120000_trial2",
        "subdollar-anchor-parent_20261002_120000_trial2",
        "subdollar-agent-nonexistent-should-not-matter",
    ] {
        if want.contains("nonexistent") {
            assert!(!names.iter().any(|n| n == want));
            continue;
        }
        assert!(names.iter().any(|n| n == want), "missing {want}");
    }
    // Trial cleanup must NEVER name the batchref share: the bracket
    // outlives every trial, and trial N's exit murdered it (gate cells:
    // post-batch "network ..._batchref not found" on both cells).
    assert!(
        names.iter().all(|n| !n.ends_with("_batchref")),
        "trial owns batchref share!"
    );
}

#[test]
fn test_owned_names_do_not_truncate_non_trial_ids() {
    // An id that merely contains "_trial" without the trailing
    // `<parent>_trial<N>` shape must not sprout batchref names.
    let sm = SandboxManager::with_id("my_trial_run");
    assert!(sm
        .owned_container_names()
        .iter()
        .all(|n| !n.ends_with("_batchref")));
    let plain = SandboxManager::with_id("parent_20261002_120000");
    assert!(plain
        .owned_container_names()
        .iter()
        .all(|n| !n.ends_with("_batchref")));
}

#[test]
fn test_label_filter_is_exact_instance_value() {
    // Labels replace the name-grep: the filter value is the full
    // instance id, so `<parent>` never matches `<parent>_trialN` or
    // `<parent>_batchref`. Pin the exact-match construction.
    let sm = SandboxManager::with_id("parent_20261002_120000_trial2");
    assert_eq!(
        sm.label_filter(false),
        vec!["label=sdb.run=parent_20261002_120000_trial2"]
    );
    let b = SandboxManager::with_id("parent_20261002_120000_batchref");
    assert_eq!(
        b.label_filter(true),
        vec!["label=sdb.run=parent_20261002_120000_batchref"]
    );
}

/// Live-docker: a guard drop reaps its labeled container; the sibling
/// trial's container (different label value) survives. Ignored so the
/// default suite stays hermetic.
#[test]
#[ignore = "requires docker; guard-drop reaping proof"]
fn guard_drop_reaps_own_label_only() {
    let a = "guardproof-a-456";
    let b = "guardproof-b-789";
    for id in [a, b] {
        // Pre-clean: a previous failed run leaves name + net behind, and
        // `docker run --name` fails with nonzero status (not Err), which
        // .output().unwrap() swallows — the probe then "never starts".
        // NOTE: do NOT use SandboxManager here: ensure_network arms
        // needs_cleanup, so the temp manager's Drop at loop-end calls
        // cleanup() and reaps the probe just created (this faked
        // "never started" for three runs). Bare docker calls instead.
        let net = format!("subdollar-net-{}", id);
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &format!("guardprobe-{}", id)])
            .output();
        let _ = std::process::Command::new("docker")
            .args(["network", "rm", &net])
            .output();
        let co = std::process::Command::new("docker")
            .args([
                "network",
                "create",
                "--label",
                &format!("sdb.run={}", id),
                &net,
            ])
            .output()
            .unwrap();
        assert!(
            co.status.success(),
            "net {id}: {}",
            String::from_utf8_lossy(&co.stderr)
        );
        let out = std::process::Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--label",
                &format!("sdb.run={}", id),
                "--label",
                "sdb.role=probe",
                "--name",
                &format!("guardprobe-{}", id),
                "--network",
                &net,
                "alpine:3",
                "sleep",
                "300",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "probe {id} failed to start: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // Drop a guard for A: only A's container may die.
    // Precondition: both probes actually running (alpine pull / start
    // race would otherwise fake a pass or a confusing fail).
    for id in [a, b] {
        let running = std::process::Command::new("docker")
            .args(["ps", "--format", "{{.Names}}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        assert!(
            running.contains(&format!("guardprobe-{}", id)),
            "probe {id} never started; cannot prove isolation"
        );
    }
    {
        let _g = RunGuard::new(SandboxManager::with_id(a));
    }
    let left = std::process::Command::new("docker")
        .args(["ps", "-a", "--format", "{{.Names}}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    assert!(
        !left.contains(&format!("guardprobe-{}", a)),
        "own container must be reaped"
    );
    assert!(
        left.contains(&format!("guardprobe-{}", b)),
        "sibling container must survive"
    );
    // Full teardown through the guard path (nets carry the label now):
    // B's guard reaps container + zero-container net; A's net likewise.
    {
        let _gb = RunGuard::new(SandboxManager::with_id(b));
    }
    for id in [a, b] {
        let _ = std::process::Command::new("docker")
            .args(["network", "rm", &format!("subdollar-net-{}", id)])
            .output();
    }
}

#[test]
fn test_sweep_pattern_is_exact_instance_match() {
    // The retired `^subdollar-.*-{id}$` pattern matched a parent id
    // against `<parent>_trial<N>` containers: trial 1's cleanup reaped
    // trial 2's live candidate. The narrowed alternation fires on the
    // exact id only — never the `_batchref` sibling (BatchRef::finish
    // owns that share; trial N's exit murdered the bracket through it).
    let pat = |id: &str| format!("^subdollar-(candidate|ref-sql|omp-agent|egress)-{}($)", id);
    let re = ::regex::Regex::new(&pat("parent_20261002_120000_trial2")).unwrap();
    assert!(re.is_match("subdollar-candidate-parent_20261002_120000_trial2"));
    assert!(!re.is_match("subdollar-ref-sql-parent_20261002_120000_batchref"));
    assert!(!re.is_match("subdollar-candidate-parent_20261002_120000_trial22"));
}

/// R81 F1: the sandbox is Rust-only and the image matches the Dockerfile
/// pins. The Dockerfile half is what `scripts/seen-failing.sh` removes
/// (the image is built by CI / the round, not by the test); the image
/// half proves the built layer: the pinned toolchain, no other language
/// toolchain, and a no-dependency crate building offline while any
/// dependency fails.
#[test]
fn r81_f1_sandbox_is_rust_only() {
    use std::process::Command;

    let root = crate::config::get_repo_root();
    let dockerfile = std::fs::read_to_string(root.join("Dockerfile.sandbox"))
        .expect("Dockerfile.sandbox must be readable");
    for needle in [
        "ARG RUST_VERSION=1.100.0",
        "ARG RUSTUP_VERSION=1.29.3",
        "ARG RUSTUP_INIT_SHA256=b1c8cb14e30889e7e2999e6b69e62d9e9b9ffc0f4df8180564d306d9680e6c55",
        "--component llvm-tools-preview",
        "ENV CARGO_NET_OFFLINE=true",
        "replace-with = \"sdb-offline\"",
        "directory = \"/home/ubuntu/.cargo/offline-registry\"",
    ] {
        assert!(
            dockerfile.contains(needle),
            "Dockerfile.sandbox lacks {needle:?}"
        );
    }
    for gone in ["golang-go", "clang", "cmake", "python3-pip", "python3-dev"] {
        assert!(
            !dockerfile.contains(gone),
            "Dockerfile.sandbox still installs {gone}"
        );
    }
    // Bun is omp's private runtime, never the candidate's: no
    // /usr/local/bin/bun, and the omp wrapper names the private path.
    assert!(!dockerfile.contains("/usr/local/bin/bun"));
    assert!(dockerfile.contains("/opt/omp-runtime/bin/bun"));

    let run = |cmd: &str| -> (bool, String) {
        let out = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                "none",
                "--entrypoint",
                "bash",
                "subdollar-sandbox",
                "-lc",
                cmd,
            ])
            .output()
            .expect("docker run");
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    };

    let (ok, out) = run("rustc --version && cargo --version");
    assert!(ok, "rustc/cargo must run: {out}");
    assert!(out.contains("rustc 1.100.0"), "pinned rustc: {out}");
    assert!(out.contains("cargo 1.100.0"), "pinned cargo: {out}");

    for gone in ["go", "bun", "clang", "cmake", "pip3"] {
        let (ok, out) = run(&format!("command -v {gone}"));
        assert!(!ok, "{gone} must be absent, found: {out}");
    }
    for present in [
        "python3 --version",
        "node --version",
        "opencode --version",
        "omp --version",
    ] {
        let (ok, out) = run(present);
        assert!(ok, "{present} must run: {out}");
    }
    let (ok, out) = run(
        "TOOLS=\"$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin\"; \
         \"$TOOLS/llvm-profdata\" --version && \"$TOOLS/llvm-cov\" --version",
    );
    assert!(ok, "llvm-tools-preview must be installed: {out}");

    let base = std::env::temp_dir().join(format!("r81-f1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let nodep = base.join("nodep");
    let dep = base.join("dep");
    std::fs::create_dir_all(nodep.join("src")).unwrap();
    std::fs::create_dir_all(dep.join("src")).unwrap();
    std::fs::write(
        nodep.join("Cargo.toml"),
        "[package]\nname = \"sdb-nodep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(nodep.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
        nodep.join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\n\
         # It is not intended for manual editing.\n\
         version = 4\n\n\
         [[package]]\nname = \"sdb-nodep\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dep.join("Cargo.toml"),
        "[package]\nname = \"sdb-dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nserde = \"1\"\n",
    )
    .unwrap();
    std::fs::write(dep.join("src/main.rs"), "fn main() {}\n").unwrap();

    let in_workspace = |dir: &std::path::Path, cmd: &str| -> (bool, String) {
        let out = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                "none",
                "-v",
                &format!("{}:/workspace", dir.display()),
                "-w",
                "/workspace",
                "--entrypoint",
                "bash",
                "subdollar-sandbox",
                "-lc",
                cmd,
            ])
            .output()
            .expect("docker run");
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    };

    let (ok, out) = in_workspace(
        &nodep,
        "cargo build --release --offline --locked && test -x target/release/sdb-nodep",
    );
    assert!(ok, "a dependency-free crate must build offline: {out}");
    let (ok, out) = in_workspace(&dep, "cargo build --release --offline 2>&1");
    assert!(!ok, "a crate with a dependency must fail offline: {out}");
    assert!(
        out.contains("no matching package")
            || out.contains("failed to select a version")
            || out.contains("not found in the offline"),
        "the dependency failure must name the offline registry: {out}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// R81 F3: the build step without the SQL lock, so CI (which skips the
/// live proofs) covers it. A dependency-free crate builds; a crate with
/// a compiler error gets the first-error line.
#[test]
fn r81_f3_build_step_unit() {
    let sm = SandboxManager::with_id(&format!("r81-f3-build-{}", std::process::id()));
    let _guard = RunGuard::new(SandboxManager::with_id(&format!(
        "r81-f3-build-{}",
        std::process::id()
    )));
    sm.cleanup();
    let base = std::env::temp_dir().join(format!("r81-f3-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);

    let write = |dir: &std::path::Path, name: &str, main_rs: &str| {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("Cargo.lock"),
            format!(
                "# This file is automatically @generated by Cargo.\n\
                 # It is not intended for manual editing.\n\
                 version = 4\n\n\
                 [[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), main_rs).unwrap();
    };

    let good = base.join("good");
    write(&good, "sdb-build", "fn main() {}\n");
    sm.build_rust_candidate(&good, 120)
        .expect("a dependency-free crate builds offline");

    let bad = base.join("bad");
    write(
        &bad,
        "sdb-build-bad",
        "fn main() { let _: i32 = \"not an int\"; }\n",
    );
    let err = sm
        .build_rust_candidate(&bad, 120)
        .expect_err("a compiler error must fail the build");
    assert!(
        err.contains("[BUILD]") && err.contains("error"),
        "the line names the compiler error: {err}"
    );

    sm.cleanup();
    let _ = std::fs::remove_dir_all(&base);
}

/// R81 F3: the running-server check without the SQL lock, so CI covers
/// it. A rustc-built listener passes; a Node process beside it is
/// excluded by its harness root and fails without it.
#[test]
fn r81_f3_process_check_unit() {
    use std::process::Command;

    let sm = SandboxManager::with_id(&format!("r81-f3-proc-{}", std::process::id()));
    let _guard = RunGuard::new(SandboxManager::with_id(&format!(
        "r81-f3-proc-{}",
        std::process::id()
    )));
    sm.cleanup();
    let base = std::env::temp_dir().join(format!("r81-f3-proc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let work = base.join("server");
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::write(
        work.join("Cargo.toml"),
        "[package]\nname = \"sdb-proc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        work.join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\n\
         # It is not intended for manual editing.\n\
         version = 4\n\n\
         [[package]]\nname = \"sdb-proc\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        work.join("src/main.rs"),
        "use std::net::TcpListener;\n\
         fn main() {\n\
             let mut port = 5480u16;\n\
             let args: Vec<String> = std::env::args().collect();\n\
             let mut i = 1;\n\
             while i < args.len() {\n\
                 if args[i] == \"--port\" && i + 1 < args.len() {\n\
                     port = args[i + 1].parse().unwrap_or(5480);\n\
                 }\n\
                 i += 1;\n\
             }\n\
             let listener = TcpListener::bind((\"0.0.0.0\", port)).expect(\"bind\");\n\
             for stream in listener.incoming() {\n\
                 if let Ok(s) = stream {\n\
                     std::thread::spawn(move || {\n\
                         let _held = s;\n\
                         std::thread::sleep(std::time::Duration::from_secs(60));\n\
                     });\n\
                 }\n\
             }\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        work.join("start.sh"),
        "#!/bin/sh\nexec ./target/release/sdb-proc \"$@\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            work.join("start.sh"),
            std::fs::Permissions::from_mode(0o755),
        );
    }
    let port = free_port();
    sm.build_rust_candidate(&work, 120).expect("the build");
    sm.start_candidate_in_docker_with(
        &work,
        port,
        &CandidateRunExtras {
            launch_args: vec!["--port".to_string(), port.to_string()],
            ..Default::default()
        },
    )
    .expect("the start");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    sm.check_rust_server_processes(port, &[])
        .expect("the rustc-built listener passes");

    let name = sm.candidate_container_name();
    let out = Command::new("docker")
        .args([
            "exec",
            "-d",
            "-w",
            "/workspace",
            &name,
            "node",
            "-e",
            "setInterval(()=>{},1000)",
            "sdb-r81-unit-harness",
        ])
        .output()
        .expect("docker exec node");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut harness = 0u32;
    for _ in 0..50 {
        let out = Command::new("docker")
            .args(["exec", &name, "pgrep", "-f", "sdb-r81-unit-harness"])
            .output()
            .expect("pgrep");
        if let Ok(pid) = String::from_utf8_lossy(&out.stdout).trim().parse::<u32>() {
            harness = pid;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(harness > 0, "the harness node must appear");
    sm.check_rust_server_processes(port, &[harness])
        .expect("the harness tree is excluded");
    let escaped = sm.check_rust_server_processes(port, &[]);
    assert!(escaped.is_err(), "a node beside the server must fail");

    sm.cleanup();
    let _ = std::fs::remove_dir_all(&base);
}
