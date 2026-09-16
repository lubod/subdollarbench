use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "subdollarbench")]
#[command(about = "Autonomous Under-$1 System-Building Benchmark for Budget LLMs", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run an autonomous benchmark on a model using OMP harness
    Run {
        /// Model identifier on OpenRouter (e.g. openrouter/google/gemini-2.5-flash)
        #[arg(short, long)]
        model: String,

        /// Task to run
        #[arg(short, long, value_enum, default_value_t = TaskType::Redis)]
        task: TaskType,

        /// Reasoning effort / thinking level (off, minimal, low, medium, high, xhigh, max)
        #[arg(short = 'e', long, default_value = "high")]
        effort: String,

        /// Maximum turns/steps allowed for OMP
        #[arg(long, default_value_t = 100)]
        max_turns: u32,

        /// Maximum cost budget in USD (e.g. 1.00 for $1.00)
        #[arg(short = 'b', long, default_value_t = 1.00)]
        budget_usd: f64,

        /// Maximum execution time in minutes before stopping agent (e.g. 30 for 30m)
        #[arg(long, default_value_t = 30)]
        timeout_min: u64,

        /// OpenRouter API Key (optional, defaults to OPENROUTER_API_KEY env var)
        #[arg(long, env = "OPENROUTER_API_KEY")]
        api_key: Option<String>,

        /// Directory for candidate workspace
        #[arg(long, default_value = "./workspace")]
        workdir: String,

        /// Skip OMP agent run and evaluate existing server directly
        #[arg(long, default_value_t = false)]
        eval_only: bool,

        /// Number of repeated trials for statistical variance (Pass@k)
        #[arg(long, default_value_t = 10)]
        trials: u32,

        /// Do not save results or archive run artifacts to disk
        #[arg(long, default_value_t = false)]
        no_save: bool,

        /// Model used for the end-of-run code review judge (defaults to openrouter/meta/muse-spark-1.3-contributor)
        #[arg(long)]
        review_model: Option<String>,
    },

    /// Directly run verification test suite against a running server
    Eval {
        /// Task to verify
        #[arg(short, long, value_enum)]
        task: TaskType,

        /// Target port
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Display current benchmark leaderboard from saved results
    Leaderboard {
        /// Directory containing result JSON files
        #[arg(short, long, default_value = "./results")]
        results_dir: String,

        /// Filter by task ('http', 'redis', 'dns', or 'all' for per-task + consolidated)
        #[arg(short, long, default_value = "all")]
        task: String,
    },

    /// Export a static read-only portal (leaderboard + run pages) for VPS hosting
    RegenReviews {
        /// Regenerate LLM code reviews for existing runs with the current prompt
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// Only regenerate this single run id
        #[arg(long)]
        run_id: Option<String>,

        /// Judge model for the reviews
        #[arg(long, default_value = "openrouter/meta/muse-spark-1.3-contributor")]
        model: String,

        /// OpenRouter API Key (optional, defaults to OPENROUTER_API_KEY env var)
        #[arg(long, env = "OPENROUTER_API_KEY")]
        api_key: Option<String>,

        /// Overwrite existing review.json files
        #[arg(long, default_value_t = false)]
        force: bool,

        /// Path to results directory (for the portal refresh)
        #[arg(long, default_value = "./results")]
        results_dir: String,

        /// Output directory for the static portal refresh
        #[arg(long, default_value = "./portal")]
        out: String,

        /// Public base URL, used for absolute links (optional)
        #[arg(long, default_value = "")]
        base_url: String,

        /// Maximum number of runs to include (newest first)
        #[arg(long, default_value_t = 200)]
        keep: usize,
    },
    Portal {
        /// Path to runs directory
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// Path to results directory
        #[arg(long, default_value = "./results")]
        results_dir: String,

        /// Output directory for the static portal
        #[arg(long, default_value = "./portal")]
        out: String,

        /// Public base URL, used for absolute links (optional)
        #[arg(long, default_value = "")]
        base_url: String,

        /// Maximum number of runs to include (newest first)
        #[arg(long, default_value_t = 200)]
        keep: usize,
    },

    /// Launch web-based GUI for configuring and running benchmarks
    /// Manage the Docker sandbox image and OMP agent
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommands,
    },
    Ui {
        /// Port to bind the web server
        #[arg(short, long, default_value_t = 3000)]
        port: u16,

        /// Host address to bind
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },
}

#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum SandboxCommands {
    /// Check current installed vs latest available OMP version
    Check,
    /// Automatically update subdollar-sandbox image to latest (or specified) OMP version
    Update {
        /// Target OMP version (optional, defaults to latest on npm)
        #[arg(short, long)]
        version: Option<String>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskType {
    Redis,
    Http,
    Dns,
    Pdf,
    Wal,
    Proxy,
    Pubsub,
}

impl TaskType {
    pub fn total_stages(&self) -> u32 {
        match self {
            TaskType::Redis => 6,
            TaskType::Http => 5,
            TaskType::Dns => 7,
            TaskType::Pdf => 6,
            TaskType::Wal => 5,
            TaskType::Proxy => 5,
            TaskType::Pubsub => 4,
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            TaskType::Redis => 6379,
            TaskType::Http => 8080,
            TaskType::Dns => 5353,
            TaskType::Pdf => 8090,
            TaskType::Wal => 6390,
            TaskType::Proxy => 8091,
            TaskType::Pubsub => 8092,
        }
    }

    pub fn is_udp(&self) -> bool {
        matches!(self, TaskType::Dns)
    }
}

impl std::fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskType::Redis => write!(f, "redis"),
            TaskType::Http => write!(f, "http"),
            TaskType::Dns => write!(f, "dns"),
            TaskType::Pdf => write!(f, "pdf"),
            TaskType::Wal => write!(f, "wal"),
            TaskType::Proxy => write!(f, "proxy"),
            TaskType::Pubsub => write!(f, "pubsub"),
        }
    }
}

pub fn get_repo_root() -> PathBuf {
    if let Ok(sdb_home) =
        std::env::var("SDB_HOME").or_else(|_| std::env::var("SUBDOLLAR_BENCH_HOME"))
    {
        let p = PathBuf::from(sdb_home);
        if p.exists() {
            return p;
        }
    }

    // 1. Check current executable ancestor directory first
    if let Ok(exe) = std::env::current_exe() {
        let mut cur = exe.parent();
        while let Some(p) = cur {
            if p.join("Cargo.toml").exists() && p.join("tasks").exists() {
                return p.to_path_buf();
            }
            cur = p.parent();
        }
    }

    // 2. Check current working directory and its parent
    if let Ok(cur) = std::env::current_dir() {
        if cur.join("Cargo.toml").exists() && cur.join("tasks").exists() {
            return cur;
        }
        if let Some(parent) = cur.parent() {
            if parent.join("Cargo.toml").exists() && parent.join("tasks").exists() {
                return parent.to_path_buf();
            }
        }
    }

    let default_vm = PathBuf::from("/home/ubuntu/subdollarbench");
    if default_vm.exists() {
        return default_vm;
    }

    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_type_display_and_serde() {
        assert_eq!(format!("{}", TaskType::Redis), "redis");
        assert_eq!(format!("{}", TaskType::Http), "http");
        assert_eq!(format!("{}", TaskType::Dns), "dns");

        assert_eq!(TaskType::Redis.total_stages(), 6);
        assert_eq!(TaskType::Http.total_stages(), 5);
        assert_eq!(TaskType::Dns.total_stages(), 7);
        assert_eq!(TaskType::Pdf.total_stages(), 6);
        assert_eq!(TaskType::Wal.total_stages(), 5);
        assert_eq!(TaskType::Proxy.total_stages(), 5);
        assert_eq!(TaskType::Pubsub.total_stages(), 4);

        assert_eq!(TaskType::Redis.default_port(), 6379);
        assert_eq!(TaskType::Http.default_port(), 8080);
        assert_eq!(TaskType::Dns.default_port(), 5353);

        assert!(!TaskType::Redis.is_udp());
        assert!(!TaskType::Http.is_udp());
        assert!(TaskType::Dns.is_udp());

        let json_redis = serde_json::to_string(&TaskType::Redis).unwrap();
        assert_eq!(json_redis, "\"redis\"");
        let de_redis: TaskType = serde_json::from_str(&json_redis).unwrap();
        assert_eq!(de_redis, TaskType::Redis);

        let json_dns = serde_json::to_string(&TaskType::Dns).unwrap();
        assert_eq!(json_dns, "\"dns\"");
        let de_dns: TaskType = serde_json::from_str(&json_dns).unwrap();
        assert_eq!(de_dns, TaskType::Dns);

        let json_http = serde_json::to_string(&TaskType::Http).unwrap();
        assert_eq!(json_http, "\"http\"");
        let de_http: TaskType = serde_json::from_str(&json_http).unwrap();
        assert_eq!(de_http, TaskType::Http);
    }

    #[test]
    fn test_get_repo_root() {
        let root = get_repo_root();
        assert!(root.exists());
    }

    #[test]
    fn test_cli_parsing_run() {
        let args = [
            "subdollarbench",
            "run",
            "--model",
            "test-model",
            "--task",
            "redis",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Run {
                model,
                task,
                effort,
                budget_usd,
                max_turns,
                timeout_min,
                eval_only,
                ..
            } => {
                assert_eq!(model, "test-model");
                assert_eq!(task, TaskType::Redis);
                assert_eq!(effort, "high");
                assert_eq!(budget_usd, 1.00);
                assert_eq!(max_turns, 100);
                assert_eq!(timeout_min, 30);
                assert!(!eval_only);
            }
            _ => panic!("Expected Run command"),
        }

        let args_http = [
            "subdollarbench",
            "run",
            "--model",
            "test-model-2",
            "--task",
            "http",
            "--effort",
            "max",
            "--budget-usd",
            "0.20",
            "--max-turns",
            "10",
            "--timeout-min",
            "5",
            "--eval-only",
        ];
        let cli_http = Cli::try_parse_from(args_http).unwrap();
        match cli_http.command {
            Commands::Run {
                model,
                task,
                effort,
                budget_usd,
                max_turns,
                timeout_min,
                eval_only,
                ..
            } => {
                assert_eq!(model, "test-model-2");
                assert_eq!(task, TaskType::Http);
                assert_eq!(effort, "max");
                assert_eq!(budget_usd, 0.20);
                assert_eq!(max_turns, 10);
                assert_eq!(timeout_min, 5);
                assert!(eval_only);
            }
            _ => panic!("Expected Run command"),
        }

        let args_key = [
            "subdollarbench",
            "run",
            "--model",
            "m",
            "--task",
            "redis",
            "--api-key",
            "sk-test-123",
        ];
        let cli_key = Cli::try_parse_from(args_key).unwrap();
        match cli_key.command {
            Commands::Run { api_key, .. } => {
                assert_eq!(api_key, Some("sk-test-123".to_string()));
            }
            _ => panic!("Expected Run command"),
        }

        std::env::set_var("OPENROUTER_API_KEY", "sk-env-key-123");
        let args_env = ["subdollarbench", "run", "--model", "m", "--task", "redis"];
        let cli_env = Cli::try_parse_from(args_env).unwrap();
        match cli_env.command {
            Commands::Run { api_key, .. } => {
                assert_eq!(api_key, Some("sk-env-key-123".to_string()));
            }
            _ => panic!("Expected Run command"),
        }
        std::env::remove_var("OPENROUTER_API_KEY");

        let args_short = [
            "subdollarbench",
            "run",
            "-m",
            "test-model-3",
            "-t",
            "http",
            "-e",
            "max",
            "-b",
            "0.20",
        ];
        let cli_short = Cli::try_parse_from(args_short).unwrap();
        match cli_short.command {
            Commands::Run {
                model,
                task,
                effort,
                budget_usd,
                ..
            } => {
                assert_eq!(model, "test-model-3");
                assert_eq!(task, TaskType::Http);
                assert_eq!(effort, "max");
                assert_eq!(budget_usd, 0.20);
            }
            _ => panic!("Expected Run command"),
        }
    }

    #[test]
    fn test_cli_parsing_eval() {
        let args = [
            "subdollarbench",
            "eval",
            "--task",
            "redis",
            "--port",
            "6379",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Eval { task, port } => {
                assert_eq!(task, TaskType::Redis);
                assert_eq!(port, Some(6379));
            }
            _ => panic!("Expected Eval command"),
        }

        let args2 = ["subdollarbench", "eval", "--task", "http"];
        let cli2 = Cli::try_parse_from(args2).unwrap();
        match cli2.command {
            Commands::Eval { task, port } => {
                assert_eq!(task, TaskType::Http);
                assert_eq!(port, None);
            }
            _ => panic!("Expected Eval command"),
        }
    }

    #[test]
    fn test_cli_parsing_leaderboard() {
        let args = [
            "subdollarbench",
            "leaderboard",
            "--results-dir",
            "./custom_results",
            "--task",
            "redis",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Leaderboard { results_dir, task } => {
                assert_eq!(results_dir, "./custom_results");
                assert_eq!(task, "redis");
            }
            _ => panic!("Expected Leaderboard command"),
        }
    }

    #[test]
    fn test_cli_parsing_portal() {
        let args = [
            "subdollarbench",
            "portal",
            "--out",
            "./public",
            "--keep",
            "50",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Portal {
                runs_dir,
                results_dir,
                out,
                base_url,
                keep,
            } => {
                assert_eq!(runs_dir, "./runs");
                assert_eq!(results_dir, "./results");
                assert_eq!(out, "./public");
                assert_eq!(base_url, "");
                assert_eq!(keep, 50);
            }
            _ => panic!("Expected Portal command"),
        }
    }

    #[test]
    fn test_cli_parsing_ui() {
        let args = [
            "subdollarbench",
            "ui",
            "--port",
            "8080",
            "--host",
            "127.0.0.1",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Ui { port, host } => {
                assert_eq!(port, 8080);
                assert_eq!(host, "127.0.0.1");
            }
            _ => panic!("Expected Ui command"),
        }

        let args_default = ["subdollarbench", "ui"];
        let cli_default = Cli::try_parse_from(args_default).unwrap();
        match cli_default.command {
            Commands::Ui { port, host } => {
                assert_eq!(port, 3000);
                assert_eq!(host, "127.0.0.1");
            }
            _ => panic!("Expected Ui command"),
        }
    }

    #[test]
    fn test_cli_parsing_sandbox() {
        let args_check = ["subdollarbench", "sandbox", "check"];
        let cli_check = Cli::try_parse_from(args_check).unwrap();
        match cli_check.command {
            Commands::Sandbox { command } => {
                assert_eq!(command, SandboxCommands::Check);
            }
            _ => panic!("Expected Sandbox check command"),
        }

        let args_update = ["subdollarbench", "sandbox", "update", "--version", "18.1.21"];
        let cli_update = Cli::try_parse_from(args_update).unwrap();
        match cli_update.command {
            Commands::Sandbox { command } => {
                assert_eq!(
                    command,
                    SandboxCommands::Update {
                        version: Some("18.1.21".to_string())
                    }
                );
            }
            _ => panic!("Expected Sandbox update command"),
        }
    }
}
