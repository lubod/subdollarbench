use anyhow::Result;
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};

use crate::report::archive::{RunArchiver, RunManifest};
use crate::report::leaderboard::format_duration;
use crate::report::environment::EnvironmentInfo;

pub struct SummaryGenerator;

impl SummaryGenerator {
    pub fn update_summary_file(repo_root: &Path, runs_dir: &Path) -> Result<PathBuf> {
        let mut runs = RunArchiver::list_runs(runs_dir);
        // Sort runs: pass rate DESC -> efficiency score DESC -> cost ASC -> throughput DESC
        runs.sort_by(|a, b| {
            b.pass_rate
                .partial_cmp(&a.pass_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    b.efficiency_score
                        .partial_cmp(&a.efficiency_score)
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
                    let t_a = a.throughput_req_sec.unwrap_or(0.0);
                    let t_b = b.throughput_req_sec.unwrap_or(0.0);
                    t_b.partial_cmp(&t_a).unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        let env = EnvironmentInfo::detect();
        let markdown = Self::generate_summary_markdown(&runs, &env);

        let summary_path = repo_root.join("SUMMARY.md");
        fs::write(&summary_path, markdown)?;
        Ok(summary_path)
    }

    pub fn generate_summary_markdown(runs: &[RunManifest], env: &EnvironmentInfo) -> String {
        let now = Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();

        let mut md = String::new();
        md.push_str("# 🏆 SubDollarBench: Autonomous Under-$1 LLM Systems Benchmark\n\n");
        md.push_str("> **Measuring under-$1/1M token LLMs on autonomous, production-grade SWE systems engineering tasks (Redis & HTTP/1.1) in isolated Docker sandboxes.**\n\n");
        md.push_str(&format!("*Last Updated: {}*\n\n", now));

        md.push_str("## 📑 Quick Navigation\n");
        md.push_str("- [📊 Global Leaderboard](#-global-leaderboard)\n");
        md.push_str(
            "- [⚡ Task 1: In-Memory Redis Server](#-task-1-in-memory-redis-server-task-redis)\n",
        );
        md.push_str("- [🌐 Task 2: HTTP/1.1 Web Server](#-task-2-http11-web-server-task-http)\n");
        md.push_str(
            "- [🖥️ Testbed Environment Specs](#%EF%B8%8F-benchmark-testbed-environment-specs)\n",
        );
        md.push_str(
            "- [🚀 How to Reproduce & Submit Results](#-how-to-reproduce--submit-your-results)\n\n",
        );

        md.push_str("---\n\n");
        md.push_str("## 📊 Global Leaderboard\n\n");

        if runs.is_empty() {
            md.push_str("*No benchmark runs recorded yet. Run a benchmark to populate the leaderboard!*\n\n");
        } else {
            md.push_str("| Rank | Model | Effort | Task | Lang | Pass Rate | Duration | Turns | Throughput | Cost (USD) | Efficiency | Full Trace & Code |\n");
            md.push_str("|:---:|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|\n");

            for (idx, r) in runs.iter().enumerate() {
                let rank = match idx {
                    0 => "🥇 1".to_string(),
                    1 => "🥈 2".to_string(),
                    2 => "🥉 3".to_string(),
                    n => format!("{}", n + 1),
                };

                let eff_str = r.effort.clone().unwrap_or_else(|| "auto".to_string());
                let task_display = match r.task.as_str() {
                    "redis" => "Redis",
                    "http" => "HTTP/1.1",
                    other => other,
                };

                let pass_str = format!(
                    "{:.0}% ({}/{})",
                    r.pass_rate, r.passed_stages, r.total_stages
                );
                let dur_str = format_duration(r.duration_seconds);
                let turns_str = r.turns.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string());
                let tp_str = match (r.throughput_req_sec, r.reference_throughput_req_sec) {
                    (Some(cand), Some(refr)) if refr > 0.0 => {
                        format!("{:.0} req/s *({:.0}% ref)*", cand, (cand / refr) * 100.0)
                    }
                    (Some(cand), _) => format!("{:.0} req/s", cand),
                    _ => "N/A".to_string(),
                };

                let eff_c = r.effective_cost();
                let cost_str = if (eff_c - r.cost_usd).abs() > 0.0001 {
                    format!("${:.4} *(eff: ${:.4})*", r.cost_usd, eff_c)
                } else {
                    format!("${:.4}", r.cost_usd)
                };
                let score_str = if let Some(ts) = r.throughput_score {
                    if (ts - r.efficiency_score).abs() > 0.1 && r.pass_rate >= 100.0 {
                        format!("**{:.1}** pts/¢ *(base: {:.1})*", ts, r.efficiency_score)
                    } else {
                        format!("**{:.1}** pts/¢", ts)
                    }
                } else {
                    format!("**{:.1}** pts/¢", r.efficiency_score)
                };
                let link_str = format!("[Inspect](runs/{}/)", r.run_id);

                md.push_str(&format!(
                    "| {} | `{}` | `{}` | {} | `{}` | {} | {} | {} | {} | {} | {} | {} |\n",
                    rank,
                    r.model,
                    eff_str,
                    task_display,
                    r.language,
                    pass_str,
                    dur_str,
                    turns_str,
                    tp_str,
                    cost_str,
                    score_str,
                    link_str
                ));
            }
            md.push('\n');
        }

        md.push_str("---\n\n");
        md.push_str("## ⚡ Task 1: In-Memory Redis Server (`task: redis`)\n\n");
        md.push_str("The candidate LLM is instructed to build a production-grade, concurrent Redis clone from scratch.\n");
        md.push_str("- **Wire Protocol**: Full RESP2 parser handling pipelined TCP streams, bulk strings, integers, simple strings, and errors.\n");
        md.push_str("- **Supported Commands**: `PING`, `ECHO`, `SET` (with `PX` millisecond expiration, `EX`, `NX`, `XX`), `GET`, `DEL`, `EXISTS`, `INCR`, `DECR`, `COMMAND`, `QUIT`.\n");
        md.push_str("- **Packaging Freedom**: Working multi-stage `Dockerfile` (automatically built and containerized) or `./start.sh`.\n");
        md.push_str("- **Verification**: Automated raw-socket TCP verification test suite + `redis-benchmark -p 6379 -n 5000 -c 10 -t get,set -q`.\n\n");

        let redis_runs: Vec<&RunManifest> = runs.iter().filter(|r| r.task == "redis").collect();
        if !redis_runs.is_empty() {
            md.push_str(
                "| Model | Effort | Lang | Pass Rate | Duration | Turns | Throughput | Cost | Score | Run Archive |\n",
            );
            md.push_str("|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|\n");
            for r in redis_runs {
                let dur_str = format_duration(r.duration_seconds);
                let turns_str = r.turns.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string());
                let tp_str = r
                    .throughput_req_sec
                    .map(|t| format!("{:.0} req/s", t))
                    .unwrap_or_else(|| "N/A".to_string());
                md.push_str(&format!(
                    "| `{}` | `{}` | `{}` | {:.0}% | {} | {} | {} | ${:.4} | {:.1} pts/¢ | [runs/{}/](runs/{}/) |\n",
                    r.model,
                    r.effort.as_deref().unwrap_or("auto"),
                    r.language,
                    r.pass_rate,
                    dur_str,
                    turns_str,
                    tp_str,
                    r.cost_usd,
                    r.efficiency_score,
                    r.run_id,
                    r.run_id
                ));
            }
            md.push('\n');
        }

        md.push_str("---\n\n");
        md.push_str("## 🌐 Task 2: HTTP/1.1 Web Server (`task: http`)\n\n");
        md.push_str("The candidate LLM is instructed to build an RFC 7230/7231 HTTP/1.1 web server from scratch.\n");
        md.push_str("- **Features**: `GET /` (200 OK), `404 Not Found Handling`, `GET /echo/{str}` with dynamic `Content-Length`, `GET /user-agent` header reflection, `POST` and `GET /files/{filename}` file persistence, HTTP/1.1 socket keep-alive reuse.\n");
        md.push_str("- **Packaging Freedom**: Working `Dockerfile` or executable `./start.sh`.\n");
        md.push_str("- **Verification**: 5-stage automated conformance suite + `wrk -t2 -c20 -d3s http://127.0.0.1:8080/`.\n\n");

        let http_runs: Vec<&RunManifest> = runs.iter().filter(|r| r.task == "http").collect();
        if !http_runs.is_empty() {
            md.push_str(
                "| Model | Effort | Lang | Pass Rate | Duration | Turns | Throughput | Cost | Score | Run Archive |\n",
            );
            md.push_str("|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|\n");
            for r in http_runs {
                let dur_str = format_duration(r.duration_seconds);
                let turns_str = r.turns.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string());
                let tp_str = r
                    .throughput_req_sec
                    .map(|t| format!("{:.0} req/s", t))
                    .unwrap_or_else(|| "N/A".to_string());
                md.push_str(&format!(
                    "| `{}` | `{}` | `{}` | {:.0}% | {} | {} | {} | ${:.4} | {:.1} pts/¢ | [runs/{}/](runs/{}/) |\n",
                    r.model,
                    r.effort.as_deref().unwrap_or("auto"),
                    r.language,
                    r.pass_rate,
                    dur_str,
                    turns_str,
                    tp_str,
                    r.cost_usd,
                    r.efficiency_score,
                    r.run_id,
                    r.run_id
                ));
            }
            md.push('\n');
        }

                md.push_str("---\n\n");
        md.push_str("## 📡 Task 3: DNS Server (`task: dns`)\n\n");
        md.push_str("The candidate LLM is instructed to build an RFC 1035 UDP DNS Server from scratch.\n");
        md.push_str("- **Wire Protocol**: Raw UDP query resolver handling RFC 1035 packet headers, question queries, and A-record resolution.\n");
        md.push_str("- **Supported Queries**: A record lookups, standard query flags (QR, Opcode, AA, RD, RA, RCODE), dynamic port listening.\n");
        md.push_str("- **Packaging Freedom**: Working multi-stage `Dockerfile` (automatically built and containerized) or `./start.sh`.\n");
        md.push_str("- **Verification**: 4-stage automated UDP conformance suite + `queryperf` load test.\n\n");

        let dns_runs: Vec<&RunManifest> = runs.iter().filter(|r| r.task == "dns").collect();
        if !dns_runs.is_empty() {
            md.push_str(
                "| Model | Effort | Lang | Pass Rate | Duration | Turns | Throughput | Cost | Score | Run Archive |\n",
            );
            md.push_str("|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|\n");
            for r in dns_runs {
                let dur_str = format_duration(r.duration_seconds);
                let turns_str = r.turns.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string());
                let tp_str = r
                    .throughput_req_sec
                    .map(|t| format!("{:.0} req/s", t))
                    .unwrap_or_else(|| "N/A".to_string());
                md.push_str(&format!(
                    "| `{}` | `{}` | `{}` | {:.0}% | {} | {} | {} | ${:.4} | {:.1} pts/¢ | [runs/{}/](runs/{}/) |\n",
                    r.model,
                    r.effort.as_deref().unwrap_or("auto"),
                    r.language,
                    r.pass_rate,
                    dur_str,
                    turns_str,
                    tp_str,
                    r.cost_usd,
                    r.efficiency_score,
                    r.run_id,
                    r.run_id
                ));
            }
            md.push('\n');
        }

                md.push_str("---\n\n");
        md.push_str("## 📄 Task 4: PDF Extraction Microservice (`task: pdf`)\n\n");
        md.push_str("The candidate LLM is instructed to build an RFC-compliant HTTP microservice from scratch on port 8090 that extracts plain text from binary PDF files.\n");
        md.push_str("- **Protocol**: Raw TCP socket server handling HTTP `GET /` health checks, `POST /extract` PDF payload decoding, and `400 Bad Request` rejection for malformed streams.\n");
        md.push_str("- **Custom PDF Engine**: Parsing the PDF object graph (`xref`, trailer, catalog, page hierarchy, content streams, FlateDecode decompression, and text operators `Tj`/`TJ`/`Td`/`Tm`).\n");
        md.push_str("- **Strict Prohibition**: Third-party PDF libraries (`pypdf`, `pdfminer`, `fitz`, `pdfcpu`, `lopdf`, `pdf.js`) and external binaries (`pdftotext`, `mutool`) are strictly forbidden.\n");
        md.push_str("- **Verification**: 5-stage automated conformance suite + synthetic multi-page document throughput benchmark.\n\n");

        let pdf_runs: Vec<&RunManifest> = runs.iter().filter(|r| r.task == "pdf").collect();
        if !pdf_runs.is_empty() {
            md.push_str(
                "| Model | Effort | Lang | Pass Rate | Duration | Turns | Throughput | Cost | Score | Run Archive |\n",
            );
            md.push_str("|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|\n");
            for r in pdf_runs {
                let dur_str = format_duration(r.duration_seconds);
                let turns_str = r.turns.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string());
                let tp_str = r
                    .throughput_req_sec
                    .map(|t| format!("{:.0} req/s", t))
                    .unwrap_or_else(|| "N/A".to_string());
                md.push_str(&format!(
                    "| `{}` | `{}` | `{}` | {:.0}% | {} | {} | {} | ${:.4} | {:.1} pts/¢ | [runs/{}/](runs/{}/) |\n",
                    r.model,
                    r.effort.as_deref().unwrap_or("auto"),
                    r.language,
                    r.pass_rate,
                    dur_str,
                    turns_str,
                    tp_str,
                    r.cost_usd,
                    r.efficiency_score,
                    r.run_id,
                    r.run_id
                ));
            }
            md.push('\n');
        }

        md.push_str("---\n\n");
        md.push_str("## 🖥️ Benchmark Testbed Environment Specs\n\n");
        md.push_str("All runs recorded in this repository were benchmarked under identical, isolated hardware and sandbox conditions:\n\n");
        md.push_str(&env.to_markdown_table());
        md.push_str("\n\n---\n\n");

        md.push_str("## 🚀 How to Reproduce & Submit Your Results\n\n");
        md.push_str("Anyone can clone this repository, run benchmarks on their own models or hardware; results are mirrored to the public static portal.\n\n");

        md.push_str("### 1. Prerequisites\n");
        md.push_str("- **Docker**: Docker CE 24+ installed and running.\n");
        md.push_str("- **Rust**: Rust toolchain 1.80+ (`cargo`, `rustc`).\n");
        md.push_str(
            "- **OMP Agent**: `oh-my-pi` installed (`bun install -g @oh-my-pi/pi-coding-agent`).\n",
        );
        md.push_str("- **OpenRouter API Key**: Export `OPENROUTER_API_KEY` in your shell.\n\n");

        md.push_str("### 2. Clone the Repository\n");
        md.push_str("```bash\n");
        md.push_str("git clone https://github.com/username/subdollarbench.git\n");
        md.push_str("cd subdollarbench\n");
        md.push_str("export OPENROUTER_API_KEY=\"sk-or-v1-your-key-here\"\n");
        md.push_str("```\n\n");

        md.push_str("### 3. Run the Benchmark\n\n");
        md.push_str("#### Option A: Interactive Web UI (Recommended)\n");
        md.push_str("```bash\n");
        md.push_str("cargo run --release -- ui --port 3000\n");
        md.push_str("# Open http://localhost:3000 in your browser\n");
        md.push_str("```\n");
        md.push_str("1. Select an under-$1/1M token model from the live OpenRouter catalog.\n");
        md.push_str("2. Select task (`redis` or `http`) and reasoning effort (`low`, `medium`, `high`, `max`).\n");
        md.push_str("3. Click **Start Benchmark Run** to watch live thoughts, tool actions, and verifications.\n");
        md.push_str("4. Finished runs are exported to the static portal (./portal/) automatically.\n\n");

        md.push_str("#### Option B: Headless CLI\n");
        md.push_str("```bash\n");
        md.push_str("# Run Redis benchmark\n");
        md.push_str("cargo run --release -- run --task redis --model openrouter/meta/muse-spark-1.3-contributor --effort low\n\n");
        md.push_str("# Run HTTP benchmark\n");
        md.push_str("cargo run --release -- run --task http --model openrouter/meta/muse-spark-1.3-contributor --effort low\n");
        md.push_str("```\n\n");

        md.push_str("### 4. Public Portal Mirror\n");
        md.push_str("Each completed run refreshes the static portal and SUMMARY.md automatically. Sync it to your VPS:\n");
        md.push_str("```bash\n");
        md.push_str("cargo run --release -- portal --out ./portal\n");
        md.push_str("rsync -az --delete ./portal/ <vps>:/srv/bench/\n");
        md.push_str("```\n");
        md.push_str("Your run directory (`runs/<run_id>/`) contains:\n");
        md.push_str("- `workspace/`: The complete candidate codebase produced by the model.\n");
        md.push_str(
            "- `manifest.json`: Full metrics, token consumption, live spend, and test stages.\n",
        );
        md.push_str(
            "- `console.log`: Complete audit trail of thoughts, bash commands, and test outputs.\n",
        );
        md.push_str("- `env.json`: Hardware, operating system, and container specifications.\n");
        md.push_str("- `README.md`: Self-contained markdown report for that specific run.\n\n");

        md.push_str("---\n");
        md.push_str("*Maintained by the SubDollarBench Community. Under-$1 LLMs can build real software.* 🚀\n");

        md
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::archive::RunTokenUsage;

    #[test]
    fn test_summary_markdown_generation() {
        let env = EnvironmentInfo {
            os: "Ubuntu 24.04 LTS".to_string(),
            kernel: "Linux 6.8.0".to_string(),
            arch: "x86_64".to_string(),
            cpu_model: "Intel Core i7".to_string(),
            cpu_cores: 8,
            total_memory: "16.0 GiB".to_string(),
            docker_version: "Docker 27.1.1".to_string(),
            rust_version: "rustc 1.81.0".to_string(),
            python_version: "Python 3.12.3".to_string(),
            node_version: "v20.17.0".to_string(),
            git_commit: "abc1234".to_string(),
            git_branch: "master".to_string(),
            timestamp: "2026-09-04T12:00:00Z".to_string(),
        };

        let run1 = RunManifest {
            run_id: "redis_run_1".to_string(),
            model: "meta/muse-spark".to_string(),
            task: "redis".to_string(),
            status: "completed".to_string(),
            language: "Rust".to_string(),
            effort: Some("max".to_string()),
            started_at: "2026-09-04T08:00:00Z".to_string(),
            completed_at: "2026-09-04T08:02:00Z".to_string(),
            duration_seconds: 120.0,
            turns: Some(5),
            pass_rate: 100.0,
            passed_stages: 4,
            total_stages: 4,
            stages: vec![],
            throughput_req_sec: Some(74000.0),
            reference_throughput_req_sec: Some(71000.0),
            tokens: RunTokenUsage {
                prompt_tokens: 1000,
                cached_tokens: 500,
                completion_tokens: 200,
                total_tokens: 1200,
            },
            cost_usd: 0.01,
            effective_cost_usd: Some(0.015),
            savings_percent: 50.0,
            efficiency_score: 100.0,
            throughput_score: Some(308.5),
            files: vec![],
            env: Some(env.clone()),
            git_commit: Some("abc1234".to_string()),
            is_published: Some(true),
            method_version: Some("0.1.0".to_string()),
        };

        let md = SummaryGenerator::generate_summary_markdown(&[run1], &env);
        assert!(md.contains("# 🏆 SubDollarBench: Autonomous Under-$1 LLM Systems Benchmark"));
        assert!(md.contains("meta/muse-spark"));
        assert!(md.contains("74000 req/s"));
        assert!(md.contains("Ubuntu 24.04 LTS"));
        assert!(md.contains("How to Reproduce & Submit Your Results"));
    }
}
