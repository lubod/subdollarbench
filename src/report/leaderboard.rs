use anyhow::Result;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, Color, Row, Table};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkRunResult {
    pub id: String,
    pub model: String,
    pub task: String,
    pub language: String,
    pub effort: Option<String>,
    pub pass_rate: f64,
    pub passed_stages: u32,
    pub total_stages: u32,
    pub throughput_req_sec: Option<f64>,
    #[serde(default)]
    pub reference_throughput_req_sec: Option<f64>,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    #[serde(default)]
    pub effective_cost_usd: Option<f64>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub turns: Option<u32>,
    pub savings_percent: f64,
    pub efficiency_score: f64,
    #[serde(default)]
    pub throughput_score: Option<f64>,
    pub timestamp: String,
    pub method_version: Option<String>,
}

pub fn format_duration(secs: f64) -> String {
    if secs <= 0.0 {
        "-".to_string()
    } else if secs < 60.0 {
        format!("{:.0}s", secs)
    } else {
        let mins = (secs / 60.0).floor() as u64;
        let rem_secs = (secs % 60.0).round() as u64;
        format!("{}m {}s", mins, rem_secs)
    }
}

impl BenchmarkRunResult {
    pub fn effective_cost(&self) -> f64 {
        self.effective_cost_usd.unwrap_or(self.total_cost_usd)
    }
}

pub fn compute_pass_at_k(n: usize, c: usize, k: usize) -> f64 {
    if n == 0 || c == 0 || k == 0 || c > n || k > n {
        if n > 0 && c == n {
            return 1.0;
        }
        return 0.0;
    }
    if n - c < k {
        return 1.0;
    }
    let mut prod = 1.0;
    for i in 0..k {
        prod *= (n - c - i) as f64 / (n - i) as f64;
    }
    1.0 - prod
}

pub struct LeaderboardManager;

impl LeaderboardManager {
    /// Detects the candidate implementation language by scanning source file extensions in `workdir`.
    ///
    /// Uses a deterministic priority hierarchy: `Rust` > `Go` > `C/C++` > `Node.js` > `Python`.
    /// Priority ordering ensures deterministic categorization for polyglot workspaces
    /// (e.g. wrapper/build scripts), favoring compiled systems languages over scripting runtimes.
    pub fn detect_language(workdir: &Path) -> String {
        let mut has_c = false;
        let mut has_rust = false;
        let mut has_go = false;
        let mut has_py = false;
        let mut has_js = false;

        Self::scan_extensions(
            workdir,
            &mut has_c,
            &mut has_rust,
            &mut has_go,
            &mut has_py,
            &mut has_js,
        );

        if has_rust {
            "Rust".to_string()
        } else if has_go {
            "Go".to_string()
        } else if has_c {
            "C/C++".to_string()
        } else if has_js {
            "Node.js".to_string()
        } else if has_py {
            "Python".to_string()
        } else {
            "Unknown".to_string()
        }
    }

    fn scan_extensions(
        dir: &Path,
        has_c: &mut bool,
        has_rust: &mut bool,
        has_go: &mut bool,
        has_py: &mut bool,
        has_js: &mut bool,
    ) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.') && name != "target" && name != "node_modules" {
                        Self::scan_extensions(&path, has_c, has_rust, has_go, has_py, has_js);
                    }
                } else if path.is_file() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name == "Cargo.toml" || name.ends_with(".rs") {
                        *has_rust = true;
                    } else if name == "go.mod" || name.ends_with(".go") {
                        *has_go = true;
                    } else if name.ends_with(".c")
                        || name.ends_with(".cpp")
                        || name.ends_with(".cc")
                    {
                        *has_c = true;
                    } else if name.ends_with(".py") || name == "requirements.txt" {
                        *has_py = true;
                    } else if name.ends_with(".js")
                        || name.ends_with(".ts")
                        || name == "package.json"
                    {
                        *has_js = true;
                    }
                }
            }
        }
    }

    pub fn save_result(results_dir: &str, result: &BenchmarkRunResult) -> Result<()> {
        let path = Path::new(results_dir);
        fs::create_dir_all(path)?;

        let filename = format!("{}.json", result.id);
        let filepath = path.join(filename);

        let data = serde_json::to_string_pretty(result)?;
        fs::write(filepath, data)?;
        Ok(())
    }

    pub fn load_all(results_dir: &str) -> Vec<BenchmarkRunResult> {
        let path = Path::new(results_dir);
        let mut list = Vec::new();

        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|s| s.to_str()) == Some("json") {
                    if let Ok(data) = fs::read_to_string(p) {
                        if let Ok(mut res) = serde_json::from_str::<BenchmarkRunResult>(&data) {
                            if res.total_tokens == 0 {
                                res.total_tokens = res.prompt_tokens + res.cached_tokens + res.completion_tokens;
                            }
                            list.push(res);
                        }
                    }
                }
            }
        }
        list.sort_by(|a, b| {
            let a_pass_100 = a.pass_rate >= 100.0;
            let b_pass_100 = b.pass_rate >= 100.0;
            b_pass_100
                .cmp(&a_pass_100)
                .then_with(|| {
                    b.pass_rate
                        .partial_cmp(&a.pass_rate)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    let b_score = b.throughput_score.unwrap_or(b.efficiency_score);
                    let a_score = a.throughput_score.unwrap_or(a.efficiency_score);
                    b_score
                        .partial_cmp(&a_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    let a_cost = a.effective_cost();
                    let b_cost = b.effective_cost();
                    a_cost
                        .partial_cmp(&b_cost)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    let a_tp = a.throughput_req_sec.unwrap_or(0.0);
                    let b_tp = b.throughput_req_sec.unwrap_or(0.0);
                    b_tp.partial_cmp(&a_tp).unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        list
    }

    pub fn print_table(results: &[BenchmarkRunResult]) {
        Self::print_table_with_title("Benchmark Leaderboard", results);
    }

    pub fn print_table_with_title(title: &str, results: &[BenchmarkRunResult]) {
        if results.is_empty() {
            println!("\n=== {} ===\nNo results to display.\n", title);
            return;
        }

        println!("\n=== {} ===", title);
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                "Rank",
                "Model",
                "Effort",
                "Task",
                "Lang",
                "Pass Rate",
                "Duration",
                "Turns",
                "Throughput",
                "Cost ($)",
                "Cache Savings",
                "Score / ¢",
            ]);

        for (idx, r) in results.iter().enumerate() {
            let rank_str = match idx {
                0 => "🥇 1".to_string(),
                1 => "🥈 2".to_string(),
                2 => "🥉 3".to_string(),
                _ => format!("{}", idx + 1),
            };

            let tp = match (r.throughput_req_sec, r.reference_throughput_req_sec) {
                (Some(cand_tp), Some(ref_tp)) if ref_tp > 0.0 => {
                    let ratio = (cand_tp / ref_tp) * 100.0;
                    format!("{:.0} req/s ({:.0}% ref)", cand_tp, ratio)
                }
                (Some(cand_tp), _) => format!("{:.0} req/s", cand_tp),
                _ => "N/A".to_string(),
            };

            let pass_str = format!(
                "{:.0}% ({}/{})",
                r.pass_rate, r.passed_stages, r.total_stages
            );
            let savings_str = format!("{:.0}%", r.savings_percent);
            let eff_str = r.effort.clone().unwrap_or_else(|| "auto".to_string());
            let dur_str = r
                .duration_seconds
                .map(format_duration)
                .unwrap_or_else(|| "-".to_string());
            let turns_str = r
                .turns
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string());

            table.add_row(Row::from(vec![
                Cell::new(rank_str).fg(Color::Yellow),
                Cell::new(&r.model).fg(Color::Cyan),
                Cell::new(eff_str).fg(Color::Yellow),
                Cell::new(&r.task),
                Cell::new(&r.language).fg(Color::Green),
                Cell::new(pass_str).fg(if r.pass_rate == 100.0 {
                    Color::Green
                } else {
                    Color::Yellow
                }),
                Cell::new(dur_str).fg(Color::White),
                Cell::new(turns_str).fg(Color::Cyan),
                Cell::new(tp),
                Cell::new(format!("${:.4}", r.total_cost_usd)).fg(Color::Magenta),
                Cell::new(savings_str).fg(Color::Green),
                Cell::new(format!("{:.1}", r.throughput_score.unwrap_or(r.efficiency_score))).fg(Color::Cyan),
            ]));
        }

        println!("\n{}", table);
    }

    pub fn print_leaderboard_views(results: &[BenchmarkRunResult], task_filter: &str) {
        let trimmed = task_filter.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
            // Group distinct tasks preserving order
            let mut tasks: Vec<String> = Vec::new();
            for r in results {
                if !tasks.contains(&r.task) {
                    tasks.push(r.task.clone());
                }
            }

            // Print per-task leaderboards first
            for task in &tasks {
                let task_results: Vec<BenchmarkRunResult> = results
                    .iter()
                    .filter(|r| r.task.eq_ignore_ascii_case(task))
                    .cloned()
                    .collect();

                let title = match task.to_lowercase().as_str() {
                    "redis" => "⚡ Task: In-Memory Redis Server Leaderboard",
                    "http" => "🌐 Task: HTTP/1.1 Web Server Leaderboard",
                    "dns" => "📡 Task: DNS Server Leaderboard",
                    _ => &format!("Task: {} Leaderboard", task),
                };
                Self::print_table_with_title(title, &task_results);
            }

            // Print consolidated global leaderboard
            Self::print_table_with_title(
                "🏆 Consolidated Global Leaderboard (All Tasks)",
                results,
            );
        } else {
            let task_results: Vec<BenchmarkRunResult> = results
                .iter()
                .filter(|r| r.task.eq_ignore_ascii_case(trimmed))
                .cloned()
                .collect();

            if task_results.is_empty() {
                println!("\nNo benchmark results found for task '{}'.\n", trimmed);
            } else {
                let title = match trimmed.to_lowercase().as_str() {
                    "redis" => "⚡ Task: In-Memory Redis Server Leaderboard",
                    "http" => "🌐 Task: HTTP/1.1 Web Server Leaderboard",
                    "dns" => "📡 Task: DNS Server Leaderboard",
                    _ => &format!("Task: {} Leaderboard", trimmed),
                };
                Self::print_table_with_title(title, &task_results);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_pass_at_k() {
        assert_eq!(compute_pass_at_k(0, 0, 1), 0.0);
        assert_eq!(compute_pass_at_k(5, 0, 1), 0.0);
        assert_eq!(compute_pass_at_k(5, 5, 1), 1.0);
        assert_eq!(compute_pass_at_k(5, 5, 5), 1.0);
        assert_eq!(compute_pass_at_k(3, 1, 3), 1.0);
        let p1 = compute_pass_at_k(3, 1, 1);
        assert!((p1 - 0.3333).abs() < 0.01);
    }

    #[test]
    fn test_language_detection() {
        let temp = std::env::temp_dir().join(format!("test_lang_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);

        // Go
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("main.go"), "package main").unwrap();
        assert_eq!(LeaderboardManager::detect_language(&temp), "Go");
        let _ = fs::remove_dir_all(&temp);

        // Rust nested
        fs::create_dir_all(temp.join("rust-pkg/src")).unwrap();
        fs::write(temp.join("rust-pkg/Cargo.toml"), "[package]").unwrap();
        fs::write(temp.join("rust-pkg/src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(LeaderboardManager::detect_language(&temp), "Rust");
        let _ = fs::remove_dir_all(&temp);

        // Python
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("server.py"), "print('hi')").unwrap();
        assert_eq!(LeaderboardManager::detect_language(&temp), "Python");
        let _ = fs::remove_dir_all(&temp);

        // Node
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("index.js"), "console.log(1)").unwrap();
        assert_eq!(LeaderboardManager::detect_language(&temp), "Node.js");
        let _ = fs::remove_dir_all(&temp);

        // Empty dir
        fs::create_dir_all(&temp).unwrap();
        assert_eq!(LeaderboardManager::detect_language(&temp), "Unknown");
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_language_detection_c_and_more() {
        let temp = std::env::temp_dir().join(format!("test_c_lang_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);

        // C/C++
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("main.c"), "int main() {}").unwrap();
        assert_eq!(LeaderboardManager::detect_language(&temp), "C/C++");
        let _ = fs::remove_dir_all(&temp);

        // Go with go.mod
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("go.mod"), "module test").unwrap();
        assert_eq!(LeaderboardManager::detect_language(&temp), "Go");
        let _ = fs::remove_dir_all(&temp);

        // Python with requirements.txt
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("requirements.txt"), "flask").unwrap();
        assert_eq!(LeaderboardManager::detect_language(&temp), "Python");
        let _ = fs::remove_dir_all(&temp);

        // Rust with Cargo.toml
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(LeaderboardManager::detect_language(&temp), "Rust");
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_print_table_runs_without_panic() {
        let r = BenchmarkRunResult {
            id: "run_test".to_string(),
            model: "model_test".to_string(),
            task: "redis".to_string(),
            language: "Rust".to_string(),
            effort: Some("auto".to_string()),
            pass_rate: 100.0,
            passed_stages: 4,
            total_stages: 4,
            throughput_req_sec: Some(60000.0),
            reference_throughput_req_sec: Some(71000.0),
            prompt_tokens: 1000,
            cached_tokens: 500,
            completion_tokens: 100,
            total_tokens: 1600,
            total_cost_usd: 0.005,
            effective_cost_usd: Some(0.008),
            duration_seconds: Some(60.0),
            turns: Some(4),
            savings_percent: 50.0,
            efficiency_score: 200.0,
            throughput_score: Some(538.0),
            timestamp: "2026-09-04T12:00:00Z".to_string(),
            method_version: Some("0.1.0".to_string()),
        };
        LeaderboardManager::print_table(&[r]);
        LeaderboardManager::print_table(&[]);
    }

    #[test]
    fn test_leaderboard_sorting_and_persistence() {
        let temp = std::env::temp_dir().join(format!("test_lb_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);

        // r1 has lower pass rate (50%), but high efficiency score (300.0) due to low cost
        let r1 = BenchmarkRunResult {
            id: "run_partial_pass".to_string(),
            model: "model_b".to_string(),
            task: "redis".to_string(),
            language: "Python".to_string(),
            effort: Some("low".to_string()),
            pass_rate: 50.0,
            passed_stages: 2,
            total_stages: 4,
            throughput_req_sec: None,
            reference_throughput_req_sec: None,
            prompt_tokens: 1000,
            cached_tokens: 500,
            completion_tokens: 200,
            total_tokens: 1700,
            total_cost_usd: 0.001,
            effective_cost_usd: Some(0.003),
            duration_seconds: Some(40.0),
            turns: Some(2),
            savings_percent: 25.0,
            efficiency_score: 0.0,
            throughput_score: Some(0.0),
            timestamp: "2026-09-04T06:00:00Z".to_string(),
            method_version: Some("0.1.0".to_string()),
        };

        // r2 has 100% pass rate, efficiency score (200.0)
        let r2 = BenchmarkRunResult {
            id: "run_full_pass".to_string(),
            model: "model_a".to_string(),
            task: "redis".to_string(),
            language: "Go".to_string(),
            effort: Some("high".to_string()),
            pass_rate: 100.0,
            passed_stages: 4,
            total_stages: 4,
            throughput_req_sec: Some(50000.0),
            reference_throughput_req_sec: Some(71000.0),
            prompt_tokens: 2000,
            cached_tokens: 1500,
            completion_tokens: 300,
            total_tokens: 3800,
            total_cost_usd: 0.005,
            effective_cost_usd: Some(0.008),
            duration_seconds: Some(50.0),
            turns: Some(3),
            savings_percent: 60.0,
            efficiency_score: 200.0,
            throughput_score: Some(481.0),
            timestamp: "2026-09-04T06:05:00Z".to_string(),
            method_version: Some("0.1.0".to_string()),
        };

        LeaderboardManager::save_result(temp.to_str().unwrap(), &r1).unwrap();
        LeaderboardManager::save_result(temp.to_str().unwrap(), &r2).unwrap();

        let list = LeaderboardManager::load_all(temp.to_str().unwrap());
        assert_eq!(list.len(), 2);
        // Full pass (100%) must rank higher than partial pass (50%) regardless of efficiency score!
        assert_eq!(list[0].id, "run_full_pass");
        assert_eq!(list[1].id, "run_partial_pass");

        let _ = fs::remove_dir_all(&temp);
    }
    #[test]
    fn test_leaderboard_total_tokens_fallback_when_omitted() {
        let json_data = r#"{
            "id": "legacy_run",
            "model": "legacy_model",
            "task": "redis",
            "language": "Rust",
            "pass_rate": 100.0,
            "passed_stages": 4,
            "total_stages": 4,
            "prompt_tokens": 1200,
            "cached_tokens": 800,
            "completion_tokens": 300,
            "total_cost_usd": 0.01,
            "savings_percent": 40.0,
            "efficiency_score": 100.0,
            "timestamp": "2026-09-04T12:00:00Z"
        }"#;

        let res: BenchmarkRunResult = serde_json::from_str(json_data).unwrap();
        assert_eq!(res.total_tokens, 0); // Defaulted by serde

        let temp = std::env::temp_dir().join(format!("test_lb_fallback_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp);
        fs::write(temp.join("legacy_run.json"), json_data).unwrap();

        let loaded = LeaderboardManager::load_all(temp.to_str().unwrap());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].total_tokens, 2300); // 1200 + 800 + 300

        let _ = fs::remove_dir_all(&temp);
    }
}
