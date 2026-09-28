use std::fs;
use subdollarbench::cost::ModelPricing;
use subdollarbench::report::{RunArchiver, RunManifest, RunTokenUsage};
use subdollarbench::verifier::StageResult;

#[test]
fn test_traceability_archive_and_retrieval_flow() {
    let base_dir = std::env::temp_dir().join(format!("test_trace_{}", std::process::id()));
    let runs_dir = base_dir.join("runs");
    let ws_dir = base_dir.join("workspace");
    let _ = fs::remove_dir_all(&base_dir);
    fs::create_dir_all(&ws_dir).expect("failed to create workspace dir");

    // 1. Simulate files created by candidate LLM
    let main_go_content = r#"package main
import "fmt"
func main() {
    fmt.Println("Mock Redis Clone Started")
}
"#;
    let start_sh_content = "#!/bin/bash\ngo run main.go\n";
    fs::write(ws_dir.join("main.go"), main_go_content).unwrap();
    fs::write(ws_dir.join("start.sh"), start_sh_content).unwrap();
    fs::write(ws_dir.join("go.mod"), "module bench\ngo 1.22\n").unwrap();

    // 2. Scan files
    let scanned = RunArchiver::scan_workspace_files(&ws_dir);
    assert_eq!(scanned.len(), 3);
    assert!(scanned.iter().any(|f| f.name == "main.go"));
    assert!(scanned.iter().any(|f| f.name == "start.sh"));
    assert!(scanned.iter().any(|f| f.name == "go.mod"));

    // 3. Build manifest
    let run_id = "redis_gemini_2_5_flash_20260904_120000".to_string();
    let stages = vec![
        StageResult {
            stage: 1,
            name: "Stage 1: PING & ECHO Handshake".to_string(),
            passed: true,
            error: None,
        },
        StageResult {
            stage: 2,
            name: "Stage 2: Basic Key-Value (SET/GET/DEL/EXISTS)".to_string(),
            passed: true,
            error: None,
        },
        StageResult {
            stage: 3,
            name: "Stage 3: Expiration (SET ... PX <ms>)".to_string(),
            passed: true,
            error: None,
        },
        StageResult {
            stage: 4,
            name: "Stage 4: INCR & DECR Arithmetic".to_string(),
            passed: true,
            error: None,
        },
    ];

    let manifest = RunManifest {
        run_id: run_id.clone(),
        model: "google/gemini-2.5-flash".to_string(),
        task: "redis".to_string(),
        status: "completed".to_string(),
        language: "Go".to_string(),
        effort: Some("high".to_string()),
        started_at: "2026-09-04T12:00:00Z".to_string(),
        completed_at: "2026-09-04T12:01:15Z".to_string(),
        duration_seconds: 75.0,
        turns: Some(8),
        pass_rate: 100.0,
        passed_stages: 4,
        total_stages: 4,
        stages,
        throughput_req_sec: Some(68500.0),
        reference_throughput_req_sec: Some(71000.0),
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        tokens: RunTokenUsage {
            prompt_tokens: 18500,
            cached_tokens: 14000,
            completion_tokens: 3200,
            total_tokens: 21700,
        },
        cost_usd: 0.0031,
        effective_cost_usd: Some(0.005),
        savings_percent: 63.5,
        efficiency_score: 322.6,
        throughput_score: Some(944.0),
        files: scanned,
        env: None,
        git_commit: None,
        total_trials: None,
        pass_at_1: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        rules_version: None,
        stability: None,
        trial_pass_rates: Vec::new(),
        is_published: None,
        method_version: Some("0.1.0".to_string()),
        temperature: None,
        top_p: None,
        seed: None,
        prompt_sha: None,
    };

    let console_log = r#"[12:00:00] [INIT] Starting benchmark for google/gemini-2.5-flash on task 'redis'...
[12:00:01] [SETUP] Starting official reference server in Docker...
[12:00:03] [OMP] Spawning OMP agent...
[12:00:45] [OMP] Finished! Tokens: prompt=18500, cached=14000, completion=3200
[12:00:46] [SANDBOX] Launching candidate clone in isolated Docker container...
[12:00:50] [TEST] Running Protocol Verification Test Suite...
[12:00:51]   [PASS] Stage 1: PING & ECHO Handshake
[12:00:52]   [PASS] Stage 2: Basic Key-Value (SET/GET/DEL/EXISTS)
[12:00:53]   [PASS] Stage 3: Expiration (SET ... PX <ms>)
[12:00:54]   [PASS] Stage 4: INCR & DECR Arithmetic
[12:00:55] [BENCH] Running Stress & Concurrency Benchmark in Docker...
[12:01:10]   Throughput: 68500 req/sec
[12:01:12] [BILLING] Formula Cost: $0.0031 USD (Prompt Cache Savings: 63.5%)
[12:01:14] [ARCHIVE] Run trace, workspace files & manifest archived to runs/redis_gemini_2_5_flash_20260904_120000/
[12:01:15] [DONE]
"#;

    // 4. Archive run
    let archived_path = RunArchiver::archive_run(&runs_dir, &manifest, &ws_dir, console_log)
        .expect("archiving should succeed");
    assert!(archived_path.exists());
    assert!(archived_path.join("manifest.json").exists());
    assert!(archived_path.join("console.log").exists());
    assert!(archived_path.join("workspace").join("main.go").exists());

    // 5. Verify list_runs
    let runs = RunArchiver::list_runs(&runs_dir);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, run_id);
    assert_eq!(runs[0].model, "google/gemini-2.5-flash");
    assert_eq!(runs[0].effort.as_deref(), Some("high"));
    assert_eq!(runs[0].pass_rate, 100.0);
    assert_eq!(runs[0].files.len(), 3);

    // 6. Verify get_run
    let loaded = RunArchiver::get_run(&runs_dir, &run_id).expect("run should be found");
    assert_eq!(loaded.stages.len(), 4);
    assert_eq!(loaded.stages[0].name, "Stage 1: PING & ECHO Handshake");
    assert!(loaded.stages[0].passed);
    assert_eq!(loaded.throughput_req_sec, Some(68500.0));

    // 7. Verify get_console_log
    let loaded_log = RunArchiver::get_console_log(&runs_dir, &run_id).expect("log should be found");
    assert!(loaded_log.contains("[OMP] Finished!"));
    assert!(loaded_log.contains("Throughput: 68500 req/sec"));

    // 8. Verify get_workspace_file
    let main_content = RunArchiver::get_workspace_file(&runs_dir, &run_id, "main.go")
        .expect("workspace file should be found");
    assert!(main_content.contains("Mock Redis Clone Started"));

    // 9. Verify path traversal safety
    let bad_path = RunArchiver::get_workspace_file(&runs_dir, &run_id, "../../../../etc/passwd");
    assert!(bad_path.is_none());

    // Clean up
    let _ = fs::remove_dir_all(&base_dir);
}

#[test]
fn test_pricing_cost_breakdown_integration() {
    let p_gemini = ModelPricing::for_model("google/gemini-2.5-flash");
    let p_deepseek = ModelPricing::for_model("deepseek/deepseek-chat");

    let b_gemini = p_gemini.compute_cost_with_cache(20_000, 15_000, 3_000);
    let b_deepseek = p_deepseek.compute_cost_with_cache(20_000, 15_000, 3_000);

    assert!(b_deepseek.total_cost_usd < b_gemini.total_cost_usd);
    assert!(b_gemini.savings_percent > 0.0);
    assert!(b_deepseek.savings_percent > 0.0);
}
