use chrono::Utc;
use std::collections::{HashMap, HashSet};
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subruns: Vec<BenchmarkRunResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_trials: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_at_1: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_at_k: Option<f64>,
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

    /// Merges multiple subruns into a single consolidated BenchmarkRunResult
    /// with averaged metrics, while preserving the individual subrun records.
    pub fn merge_subruns(parent_id: &str, subruns: Vec<BenchmarkRunResult>) -> Option<BenchmarkRunResult> {
        if subruns.is_empty() {
            return None;
        }
        if subruns.len() == 1 {
            let mut single = subruns.into_iter().next().unwrap();
            single.id = parent_id.to_string();
            let p1 = if single.pass_rate >= 100.0 { 100.0 } else { 0.0 };
            single.total_trials = Some(1);
            single.pass_at_1 = Some(p1);
            single.pass_at_k = Some(p1);
            return Some(single);
        }

        let n = subruns.len();
        let n_f64 = n as f64;

        let first = &subruns[0];
        let model = first.model.clone();
        let task = first.task.clone();
        let language = first.language.clone();
        let effort = first.effort.clone();
        let reference_tp = first.reference_throughput_req_sec;
        let method_ver = first.method_version.clone();
        let total_stages = first.total_stages;

        let passing_trials = subruns.iter().filter(|r| r.pass_rate >= 100.0).count();
        let pass_at_1 = compute_pass_at_k(n, passing_trials, 1) * 100.0;
        let pass_at_k = compute_pass_at_k(n, passing_trials, n) * 100.0;

        let avg_pass_rate = subruns.iter().map(|r| r.pass_rate).sum::<f64>() / n_f64;
        let avg_passed_stages = (subruns.iter().map(|r| r.passed_stages as f64).sum::<f64>() / n_f64).round() as u32;

        let tp_count = subruns.iter().filter_map(|r| r.throughput_req_sec).count();
        let avg_tp = if tp_count > 0 {
            Some(subruns.iter().filter_map(|r| r.throughput_req_sec).sum::<f64>() / tp_count as f64)
        } else {
            None
        };

        let avg_prompt_tokens = (subruns.iter().map(|r| r.prompt_tokens as f64).sum::<f64>() / n_f64).round() as u64;
        let avg_cached_tokens = (subruns.iter().map(|r| r.cached_tokens as f64).sum::<f64>() / n_f64).round() as u64;
        let avg_completion_tokens = (subruns.iter().map(|r| r.completion_tokens as f64).sum::<f64>() / n_f64).round() as u64;
        let avg_total_tokens = (subruns.iter().map(|r| r.total_tokens as f64).sum::<f64>() / n_f64).round() as u64;

        let avg_total_cost = subruns.iter().map(|r| r.total_cost_usd).sum::<f64>() / n_f64;
        let eff_cost_count = subruns.iter().filter_map(|r| r.effective_cost_usd).count();
        let avg_effective_cost = if eff_cost_count > 0 {
            Some(subruns.iter().filter_map(|r| r.effective_cost_usd).sum::<f64>() / eff_cost_count as f64)
        } else {
            None
        };

        let dur_count = subruns.iter().filter_map(|r| r.duration_seconds).count();
        let avg_duration = if dur_count > 0 {
            Some(subruns.iter().filter_map(|r| r.duration_seconds).sum::<f64>() / dur_count as f64)
        } else {
            None
        };

        let turns_count = subruns.iter().filter_map(|r| r.turns).count();
        let avg_turns = if turns_count > 0 {
            Some((subruns.iter().filter_map(|r| r.turns).map(|t| t as f64).sum::<f64>() / turns_count as f64).round() as u32)
        } else {
            None
        };

        let avg_savings_percent = subruns.iter().map(|r| r.savings_percent).sum::<f64>() / n_f64;
        let avg_cost_for_score = avg_effective_cost.unwrap_or(avg_total_cost);
        let recomputed_eff_score = crate::cost::calculator::ModelPricing::calculate_efficiency_score(
            avg_pass_rate,
            avg_cost_for_score,
        );
        let recomputed_tp_score = crate::cost::calculator::ModelPricing::calculate_throughput_score_for_task(
            avg_pass_rate,
            avg_cost_for_score,
            avg_tp,
            reference_tp,
            Some(&task),
        );

        let latest_timestamp = subruns.iter().map(|r| &r.timestamp).max().cloned().unwrap_or_else(|| Utc::now().to_rfc3339());

        Some(BenchmarkRunResult {
            id: parent_id.to_string(),
            model,
            task,
            language,
            effort,
            pass_rate: avg_pass_rate,
            passed_stages: avg_passed_stages,
            total_stages,
            throughput_req_sec: avg_tp,
            reference_throughput_req_sec: reference_tp,
            prompt_tokens: avg_prompt_tokens,
            cached_tokens: avg_cached_tokens,
            completion_tokens: avg_completion_tokens,
            total_tokens: avg_total_tokens,
            total_cost_usd: avg_total_cost,
            effective_cost_usd: avg_effective_cost,
            duration_seconds: avg_duration,
            turns: avg_turns,
            savings_percent: avg_savings_percent,
            efficiency_score: recomputed_eff_score,
            throughput_score: Some(recomputed_tp_score),
            timestamp: latest_timestamp,
            method_version: method_ver,
            subruns,
            total_trials: Some(n as u32),
            pass_at_1: Some(pass_at_1),
            pass_at_k: Some(pass_at_k),
        })
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

    /// Consolidates subruns of a multi-trial run into a single canonical result,
    /// writing a properly labeled merged manifest.json, preserving the best trial's
    /// workspace and review disclosure, and removing loose trial files.
    pub fn consolidate_multi_trial_run(
        parent_run_id: &str,
        subrun_results: &[BenchmarkRunResult],
        save_results: bool,
    ) -> Option<BenchmarkRunResult> {
        if subrun_results.is_empty() {
            return None;
        }

        let merged = BenchmarkRunResult::merge_subruns(parent_run_id, subrun_results.to_vec())?;

        if save_results {
            let repo_root = crate::config::get_repo_root();
            let results_dir = repo_root.join("results");
            let _ = Self::save_result(results_dir.to_str().unwrap_or("./results"), &merged);

            // Clean up individual trial JSON files from results/ so only the merged parent appears
            for sub in subrun_results {
                let _ = fs::remove_file(results_dir.join(format!("{}.json", sub.id)));
            }

            // Select the best trial (highest pass rate, then highest score)
            let best_sub = subrun_results
                .iter()
                .max_by(|a, b| {
                    a.pass_rate
                        .partial_cmp(&b.pass_rate)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| {
                            let s_a = a.throughput_score.unwrap_or(a.efficiency_score);
                            let s_b = b.throughput_score.unwrap_or(b.efficiency_score);
                            s_a.partial_cmp(&s_b).unwrap_or(std::cmp::Ordering::Equal)
                        })
                })
                .unwrap_or(&subrun_results[0]);

            let runs_dir = crate::report::RunArchiver::resolve_runs_dir();
            let parent_dir = runs_dir.join(parent_run_id);
            let _ = fs::create_dir_all(&parent_dir);

            let best_sub_dir = runs_dir.join(&best_sub.id);

            // 1. Build genuine merged parent manifest (not a trial-1 clone)
            if let Ok(content) = fs::read_to_string(best_sub_dir.join("manifest.json")) {
                if let Ok(mut manifest) = serde_json::from_str::<crate::report::RunManifest>(&content) {
                    manifest.run_id = parent_run_id.to_string();
                    manifest.status = if merged.pass_rate >= 100.0 {
                        "completed".to_string()
                    } else {
                        "failed_tests".to_string()
                    };
                    manifest.pass_rate = merged.pass_rate;
                    manifest.passed_stages = merged.passed_stages;
                    manifest.total_stages = merged.total_stages;
                    manifest.duration_seconds = merged.duration_seconds.unwrap_or(0.0);
                    manifest.turns = merged.turns;
                    manifest.throughput_req_sec = merged.throughput_req_sec;
                    manifest.reference_throughput_req_sec = merged.reference_throughput_req_sec;
                    manifest.cost_usd = merged.total_cost_usd;
                    manifest.effective_cost_usd = merged.effective_cost_usd;
                    manifest.savings_percent = merged.savings_percent;
                    manifest.efficiency_score = merged.efficiency_score;
                    manifest.throughput_score = merged.throughput_score;
                    manifest.tokens = crate::report::archive::RunTokenUsage {
                        prompt_tokens: merged.prompt_tokens,
                        cached_tokens: merged.cached_tokens,
                        completion_tokens: merged.completion_tokens,
                        total_tokens: merged.total_tokens,
                    };
                    manifest.completed_at = merged.timestamp.clone();
                    if let Ok(m_json) = serde_json::to_string_pretty(&manifest) {
                        let _ = fs::write(parent_dir.join("manifest.json"), m_json);
                    }
                }
            }

            // 2. Copy env.json
            if let Ok(content) = fs::read_to_string(best_sub_dir.join("env.json")) {
                let _ = fs::write(parent_dir.join("env.json"), content);
            }

            // 3. Review with multi-trial context disclosure
            if let Ok(content) = fs::read_to_string(best_sub_dir.join("review.json")) {
                if let Ok(mut rev_val) = serde_json::from_str::<serde_json::Value>(&content) {
                    let p1 = merged.pass_at_1.unwrap_or(100.0);
                    let pk = merged.pass_at_k.unwrap_or(100.0);
                    let n = subrun_results.len();
                    let passing = subrun_results.iter().filter(|r| r.pass_rate >= 100.0).count();
                    rev_val["multi_trial_disclosure"] = serde_json::Value::String(format!(
                        "Evaluated best workspace of {} subruns ({}/{} passed, Pass@1: {:.0}%, Pass@{}: {:.0}%). Best subrun: {}",
                        n, passing, n, p1, n, pk, best_sub.id
                    ));
                    let _ = fs::write(parent_dir.join("review.json"), serde_json::to_string_pretty(&rev_val).unwrap_or(content));
                } else {
                    let _ = fs::write(parent_dir.join("review.json"), content);
                }
            }

            // 4. Copy workspace from the best trial
            let _ = crate::report::RunArchiver::copy_dir_recursive(
                &best_sub_dir.join("workspace"),
                &parent_dir.join("workspace"),
            );

            // 5. Create multi-trial header in parent console.log
            let mut parent_log = format!(
                "=== Multi-Trial Benchmark Execution ===\nModel: {}\nTask: {}\nSubruns: {}\nPass@1: {:.1}%\nPass@{}: {:.1}%\nAverage Throughput: {:.0} req/s\nAverage Cost: ${:.4}\nBest Subrun: {}\n=========================================\n\n",
                merged.model,
                merged.task,
                subrun_results.len(),
                merged.pass_at_1.unwrap_or(100.0),
                subrun_results.len(),
                merged.pass_at_k.unwrap_or(100.0),
                merged.throughput_req_sec.unwrap_or(0.0),
                merged.total_cost_usd,
                best_sub.id
            );
            if let Ok(best_log) = fs::read_to_string(best_sub_dir.join("console.log")) {
                parent_log.push_str(&best_log);
            }
            let _ = fs::write(parent_dir.join("console.log"), parent_log);

            // 6. Write parent run.json
            let merged_data = serde_json::to_string_pretty(&merged).unwrap_or_default();
            let _ = fs::write(parent_dir.join("run.json"), merged_data);
        }

        Some(merged)
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

        // Filter out any child subrun whose ID is already embedded in a parent run
        let mut child_subrun_ids = HashSet::new();
        for r in &list {
            for sub in &r.subruns {
                child_subrun_ids.insert(sub.id.clone());
            }
        }
        list.retain(|r| !child_subrun_ids.contains(&r.id));

        // Group loose trial runs (e.g. ..._trial1_...) into consolidated merged rows
        let mut groups: HashMap<String, Vec<BenchmarkRunResult>> = HashMap::new();
        let mut consolidated = Vec::new();
        let trial_re = regex::Regex::new(r"_trial\d+").unwrap();

        for r in list {
            if r.subruns.is_empty() && r.id.contains("_trial") {
                let group_key = trial_re.replace(&r.id, "").to_string();
                groups.entry(group_key).or_default().push(r);
            } else {
                consolidated.push(r);
            }
        }

        for (parent_id, mut subruns) in groups {
            if subruns.len() > 1 {
                subruns.sort_by(|a, b| a.id.cmp(&b.id));
                if let Some(merged) = BenchmarkRunResult::merge_subruns(&parent_id, subruns) {
                    consolidated.push(merged);
                }
            } else {
                consolidated.extend(subruns);
            }
        }

        list = consolidated;
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

            let pass_str = if !r.subruns.is_empty() {
                if let (Some(p1), Some(pk)) = (r.pass_at_1, r.pass_at_k) {
                    format!("{:.0}% (P@1: {:.0}%, P@{}: {:.0}%)", r.pass_rate, p1, r.subruns.len(), pk)
                } else {
                    format!("{:.0}% ({} trials)", r.pass_rate, r.subruns.len())
                }
            } else {
                format!(
                    "{:.0}% ({}/{})",
                    r.pass_rate, r.passed_stages, r.total_stages
                )
            };

            let model_display = if !r.subruns.is_empty() {
                format!("{} [{} trials avg]", r.model, r.subruns.len())
            } else {
                r.model.clone()
            };
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
                Cell::new(&model_display).fg(Color::Cyan),
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
    #[test]
    fn test_merge_subruns_computes_averages_and_pass_at_k() {
        let make_run = |id: &str, tp: f64, dur: f64, turns: u32, cost: f64, pass: f64| BenchmarkRunResult {
            id: id.to_string(),
            model: "test-model".to_string(),
            task: "dns".to_string(),
            language: "Python".to_string(),
            effort: Some("auto".to_string()),
            pass_rate: pass,
            passed_stages: if pass >= 100.0 { 6 } else { 3 },
            total_stages: 6,
            throughput_req_sec: Some(tp),
            reference_throughput_req_sec: Some(19000.0),
            prompt_tokens: 1000,
            cached_tokens: 2000,
            completion_tokens: 500,
            total_tokens: 3500,
            total_cost_usd: cost,
            effective_cost_usd: Some(cost * 2.0),
            duration_seconds: Some(dur),
            turns: Some(turns),
            savings_percent: 50.0,
            efficiency_score: 100.0,
            throughput_score: Some(tp / 10.0),
            timestamp: "2026-09-12T08:00:00Z".to_string(),
            method_version: Some("0.2.0".to_string()),
            subruns: Vec::new(),
            total_trials: None,
            pass_at_1: None,
            pass_at_k: None,
        };

        let t1 = make_run("run_trial1", 5000.0, 40.0, 8, 0.001, 100.0);
        let t2 = make_run("run_trial2", 6000.0, 30.0, 7, 0.002, 100.0);
        let t3 = make_run("run_trial3", 7000.0, 50.0, 9, 0.003, 100.0);

        let merged = BenchmarkRunResult::merge_subruns("run_parent", vec![t1, t2, t3]).unwrap();
        assert_eq!(merged.id, "run_parent");
        assert_eq!(merged.total_trials, Some(3));
        assert_eq!(merged.pass_rate, 100.0);
        assert_eq!(merged.pass_at_1, Some(100.0));
        assert_eq!(merged.pass_at_k, Some(100.0));
        assert_eq!(merged.subruns.len(), 3);
        assert_eq!(merged.throughput_req_sec.unwrap(), 6000.0);
        assert_eq!(merged.duration_seconds.unwrap(), 40.0);
        assert_eq!(merged.turns.unwrap(), 8);
        assert_eq!(merged.total_cost_usd, 0.002);
    }

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
            subruns: Vec::new(),
            total_trials: None,
            pass_at_1: None,
            pass_at_k: None,
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
            subruns: Vec::new(),
            total_trials: None,
            pass_at_1: None,
            pass_at_k: None,
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
            subruns: Vec::new(),
            total_trials: None,
            pass_at_1: None,
            pass_at_k: None,
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
