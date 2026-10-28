use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Screening cap for the stop-on-pass capability protocol: a cell runs at
/// most this many trials, stopping at the first 100% pass. Calibrated on
/// wave data (cap 4 catches every observed late solve — all first-passed at
/// trial 4 — while keeping ~80% trial savings vs uniform n=9).
pub const SCREEN_CAP_TRIALS: u32 = 4;

/// Env var naming the wave-level auto-extend trial budget. Narrow auto-extend
/// (0/cap with every trial a harness fault) draws from this envelope and
/// stops when it's spent; 0/absent (default) means manual-only.
pub const AUTO_EXTEND_BUDGET_ENV: &str = "SDB_AUTO_EXTEND_TRIALS";

#[derive(Parser, Debug)]
#[command(name = "subdollarbench")]
#[command(about = "Autonomous Under-$1 System-Building Benchmark for Budget LLMs", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum CampaignAction {
    /// List bundled campaigns
    List,
    /// Print the stage plan (no spend, no engine)
    DryRun {
        /// Campaign name (directory under ./campaigns)
        #[arg(long)]
        name: String,
        /// Model, for display only in dry-run
        #[arg(long, default_value = "(unset)")]
        model: String,
    },
    /// Run a campaign end-to-end from the CLI (foreground, no engine needed)
    Run {
        /// Optional wall-clock cap in minutes (sdb-sql runs are multi-hour;
        /// the ladder stops with over_wall when reached).
        #[arg(long)]
        wall_min: Option<u64>,
        /// Campaign name (directory under ./campaigns)
        #[arg(long)]
        name: String,
        /// Model identifier on OpenRouter
        #[arg(long)]
        model: String,
        /// Reasoning effort / thinking level
        #[arg(short = 'e', long, default_value = "high")]
        effort: String,
        /// Agent harness (omp | opencode)
        #[arg(long, value_enum, default_value_t = Harness::Omp)]
        harness: Harness,
        /// Campaign budget cap in USD; the ladder stops (over_budget) once
        /// cumulative effective cost reaches it
        #[arg(short = 'b', long, default_value_t = 1.00)]
        budget_usd: f64,
        /// Per-stage agent timeout backstop (minutes; cost is the real bound)
        #[arg(long, default_value_t = 60)]
        timeout_min: u64,
        /// OpenRouter API key (defaults to OPENROUTER_API_KEY)
        #[arg(long, env = "OPENROUTER_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        /// Skip tickets entirely and verify the existing workspace per stage
        /// (no API calls; useful for suite development)
        #[arg(long, default_value_t = false)]
        eval_only: bool,
        /// Seed the campaign workspace from this directory (contents copied,
        /// .git excluded). Eval-only replays of archived candidates use this;
        /// future brownfield campaigns will too.
        #[arg(long)]
        workspace: Option<String>,
        /// Career phase to run (P1); phases after 1 require --chain.
        #[arg(long)]
        phase: Option<u32>,
        /// Previous phase cell id to continue from (P1 career chaining).
        #[arg(long)]
        chain: Option<String>,
        /// R1: deliberately fork a chain an existing cell already continued;
        /// requires --chain and a reason.
        #[arg(long)]
        refork: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum Commands {
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

        /// Print the career variance report (points/$ ranges and rank ties)
        #[arg(long, default_value_t = false)]
        variance: bool,
    },

    /// Manage the Docker sandbox image and OMP agent
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommands,
    },
    /// Launch web-based GUI for configuring and running benchmarks
    Ui {
        /// Port to bind the web server
        #[arg(short, long, default_value_t = 3000)]
        port: u16,

        /// Host address to bind
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Engine base URL for GUI proxy mode (e.g. http://127.0.0.1:3000).
        /// When set, this process serves the GUI + read APIs locally and
        /// proxies queue/run mutations to the engine. Restart freely:
        /// the engine keeps running trials. When empty, single-process
        /// mode (GUI + engine together, as before).
        #[arg(long, default_value = "")]
        engine_url: String,
    },

    /// [operate] Follow a live run's console log (stage markers highlighted)
    #[command(hide = true)]
    Watch {
        /// Run id or unambiguous prefix (see `runs`)
        run: String,

        /// Print current log and exit instead of following
        #[arg(long, default_value_t = false)]
        no_follow: bool,

        /// Only show stage-result lines
        #[arg(long, default_value_t = false)]
        stages_only: bool,

        /// Path to runs directory
        #[arg(long, default_value = "./runs")]
        runs_dir: String,
    },

    /// [operate] Campaign mode: ordered tickets on one living workspace,
    /// cumulative acceptance, cost-first (see campaigns/ and TODO_CAMPAIGNS.md)
    Campaign {
        #[command(subcommand)]
        action: CampaignAction,
    },

    /// [maintain] Remove leaked subdollar Docker containers and networks
    Prune {
        /// Actually delete (without it, only prints the plan)
        #[arg(long, default_value_t = false)]
        yes: bool,
    },

    /// [maintain] Report-only integrity audit (orphans, junk rows, row-file drift).
    /// Exit 1 on findings. Also runs warn-only on every engine boot.
    MigrateCheck,

    /// Full-screen read-only TUI (htop-like) of the campaign board
    Tui {
        /// `ui` engine base URL (as status; SDB_SERVER default)
        #[arg(long)]
        server: Option<String>,

        /// Poll interval in seconds (minimum 1)
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },

    /// [operate] Live engine status: idle/running, current + last run (GET /api/status)
    Status {
        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,

        /// Emit raw JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// [operate] Live queue contents (GET /api/queue)
    Queue {
        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,

        /// Emit raw JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// [operate] Remove one pending queue item (DELETE /api/queue/:id)
    Dequeue {
        /// Queue id (e.g. q4)
        id: String,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [operate] Drop finished queue history (DELETE /api/queue; pending work untouched)
    QueueClear {
        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [operate] Stop live run(s): cancel flag + abort + sandbox cleanup (POST /api/run/stop).
    /// Optional run id (or queue id) stops one cell; empty stops all workers.
    Stop {
        /// Run id or queue id to stop (empty = all workers).
        #[arg(default_value = "")]
        run: String,
        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [operate] Live console tail from the engine buffer (GET /api/console)
    Log {
        /// Run id or prefix is NOT needed: the engine streams the active run.
        /// Kept for symmetry with `watch`; positional is ignored.
        #[arg(default_value = "")]
        run: String,

        /// Lines to show
        #[arg(long, default_value_t = 100)]
        tail: usize,

        /// Only show stage-result lines
        #[arg(long, default_value_t = false)]
        stages_only: bool,

        /// Follow: poll with the server cursor until idle
        #[arg(long, default_value_t = false)]
        follow: bool,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [analyze] Task prompt from the engine (GET /api/prompt/:task)
    Prompt {
        /// Task name
        task: String,

        /// Save edited prompt back (POST /api/prompt/:task); reads stdin
        #[arg(long, default_value_t = false)]
        save: bool,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [maintain] Host environment from the engine (GET /api/env)
    Env {
        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [operate] Model catalog under $1/M with prices and effort levels (GET /api/models)
    Models {
        /// Filter by family or substring ('glm', 'claude', 'qwen', 'flash', ...)
        #[arg(long)]
        family: Option<String>,

        /// Free-text search over id and name
        #[arg(long)]
        search: Option<String>,

        /// Sort order: cheapest (default), newest, expensive, name
        #[arg(long, default_value = "cheapest")]
        sort: String,

        /// Maximum rows to show
        #[arg(long, default_value_t = 60)]
        limit: usize,

        /// Emit raw JSON
        #[arg(long, default_value_t = false)]
        json: bool,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [maintain] OMP sandbox status via the engine (GET /api/sandbox/omp)
    Omp {
        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [maintain] Update the sandbox image via the engine (POST /api/sandbox/omp/update)
    OmpUpdate {
        /// Target OMP version (optional, defaults to latest on npm)
        #[arg(short, long)]
        version: Option<String>,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [maintain] Rebuild the static portal via the engine (POST /api/portal/rebuild)
    PortalRebuild {
        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [maintain] Mark/unmark a run poisoned = infra-killed, feeds no board (POST /api/runs/:id/poison)
    Poison {
        /// Run id or unambiguous prefix (resolved locally)
        run: String,

        /// Cure instead of poison
        #[arg(long, default_value_t = false)]
        cure: bool,

        /// Path to runs directory (for prefix resolution)
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [maintain] Show or hide a run in the portal (POST /api/runs/:id/portal)
    Hide {
        /// Run id or unambiguous prefix (resolved locally)
        run: String,

        /// Unhide instead of hide
        #[arg(long, default_value_t = false)]
        show: bool,

        /// Path to runs directory (for prefix resolution)
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [maintain] Delete a run and its result (DELETE /api/runs/:id)
    Rm {
        /// Run id or unambiguous prefix (resolved locally)
        run: String,

        /// Confirm deletion (no prompt otherwise)
        #[arg(long, default_value_t = false)]
        yes: bool,

        /// Path to runs directory (for prefix resolution)
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [maintain] Pre-flight checks: docker, images, API key, ports, leaks (read-only)
    Doctor,
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
    Pdf,
    Wal,
    Proxy,
    Pubsub,
    Binproto,
    /// Job queue broker (leases / ack / nack / retries / DLQ). The campaign
    /// is its primary home; the single-shot suite is the same 9 stages.
    Queue,
    /// Differential SQL database (docs/PROPOSAL-sdb-sql.md). The campaign
    /// is its only home: 21 ladder tickets, verified differentially against
    /// an in-process SQLite oracle.
    Sql,
}

impl TaskType {
    pub fn total_stages(&self) -> u32 {
        match self {
            TaskType::Redis => 6,
            TaskType::Http => 5,
            TaskType::Pdf => 6,
            TaskType::Wal => 5,
            TaskType::Proxy => 5,
            TaskType::Pubsub => 4,
            TaskType::Binproto => 5,
            TaskType::Queue => 9,
            // The full sdb-sql ladder (tickets 0-20); the campaign is its
            // only home, so the single-shot count is the whole ladder.
            TaskType::Sql => 21,
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            TaskType::Redis => 6379,
            TaskType::Http => 8080,
            TaskType::Pdf => 8090,
            TaskType::Wal => 6390,
            TaskType::Proxy => 8091,
            TaskType::Pubsub => 8092,
            TaskType::Binproto => 6395,
            TaskType::Queue => 6385,
            TaskType::Sql => 5480,
        }
    }

    /// Name-to-task mapping. Unknown names (including the removed `dns`
    /// task) are `None`.
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s.to_lowercase().as_str() {
            "redis" => Self::Redis,
            "http" => Self::Http,
            "pdf" => Self::Pdf,
            "wal" => Self::Wal,
            "proxy" => Self::Proxy,
            "pubsub" => Self::Pubsub,
            "binproto" => Self::Binproto,
            "queue" => Self::Queue,
            "sql" => Self::Sql,
            _ => return None,
        })
    }

    /// Single entry point for turning a task name into a `TaskType`.
    ///
    /// Both the CLI and the queue engine resolve task names, and they used to
    /// carry separate `match` blocks — so any gate applied to one was bypassed
    /// by the other, and the engine is the path a wave actually takes. Unknown
    /// names (including the removed `dns` task) are an error here.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Self::from_name(s).ok_or_else(|| anyhow::anyhow!("unknown task '{}'", s.to_lowercase()))
    }
}

impl std::fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskType::Redis => write!(f, "redis"),
            TaskType::Http => write!(f, "http"),
            TaskType::Pdf => write!(f, "pdf"),
            TaskType::Wal => write!(f, "wal"),
            TaskType::Proxy => write!(f, "proxy"),
            TaskType::Pubsub => write!(f, "pubsub"),
            TaskType::Binproto => write!(f, "binproto"),
            TaskType::Queue => write!(f, "queue"),
            TaskType::Sql => write!(f, "sql"),
        }
    }
}

/// Agent harness running the trial: OMP (Oh My Pi, default) or OpenCode.
/// Recorded on manifests/results; ranks never mix harnesses (same rule as
/// effort: compare within one harness, see `validate_comparison_wave`).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Omp,
    Opencode,
}

impl Harness {
    pub fn as_str(&self) -> &'static str {
        match self {
            Harness::Omp => "omp",
            Harness::Opencode => "opencode",
        }
    }

    /// Parse queue/API/extend strings; unknown (incl. legacy blanks) falls
    /// back to OMP so pre-harness rows keep grading as OMP rows.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "opencode" => Harness::Opencode,
            _ => Harness::Omp,
        }
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
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

/// Rewrite candidate-local port mentions for a parallel worker, leaving
/// in-network reference aliases untouched.
/// - Shielded: in-network alias tokens (upstream1:80, ref-redis -p 6379)
///   keep their fixed ports; every other port mention is rewritten.
/// - Rewritten with word boundaries: localhost:PORT, 0.0.0.0:PORT, -p PORT,
///   port PORT, :PORT, exposes PORT. Bare-substring replace would corrupt
///   coincidental digit runs (e.g. 16379) and ref lines.
pub fn template_ports_for_worker(content: &str, base_port: u16, worker: u8) -> String {
    if worker == 0 {
        return content.to_string();
    }
    let base = base_port.to_string();
    let off = crate::web::server::worker_port(base_port, worker).to_string();
    // Word-boundary pattern around the bare port number.
    let word_pat = ::regex::Regex::new(&format!(r"\b{}\b", ::regex::escape(&base))).unwrap();
    // In-network aliases (upstream1:80, ref-redis -p 6379): their ports are
    // fixed inside the run network and must survive rewriting — but any
    // OTHER port on the same line (candidate-local) still must be rewritten.
    // The old whole-line skip on "upstream" froze candidate ports too
    // (measured: proxy prompt told the model 8391 AND 8091; the model
    // picked 8091 and missed the same-port probe). Shield alias tokens
    // (with colon- or flag-attached ports), rewrite the rest, restore.
    // NOTE: adjacency matters — only ports directly attached to the alias
    // (`alias:port`, `alias -p port`) are shielded; bare prose ("the
    // upstreams") never matches, so candidate ports on alias lines are
    // still rewritten.
    let alias_pat = ::regex::Regex::new(
        r"(?i)(ref-redis|ref-http|upstream1|upstream2|upstream3)((\s*(-p|--port)\s*[= ]?)?\s*:?\s*\d+)?",
    )
    .unwrap();
    content
        .lines()
        .map(|line| {
            let mut stash: Vec<String> = Vec::new();
            let shielded = alias_pat
                .replace_all(line, |caps: &::regex::Captures| {
                    stash.push(caps[0].to_string());
                    format!("@@SDBALIAS{}@@", stash.len() - 1)
                })
                .to_string();
            let rewritten = word_pat.replace_all(&shielded, off.as_str()).to_string();
            let mut out = rewritten;
            for (i, orig) in stash.iter().enumerate() {
                out = out.replace(&format!("@@SDBALIAS{}@@", i), orig);
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Probe host-published upstream ports; false on first refusal. Pure
/// dial check (no docker): unit-testable against a bound listener.
/// The recreate step stays docker-gated in the caller.
pub fn upstreams_reachable(ports: &[u16]) -> bool {
    for p in ports {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], *p));
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).is_err() {
            return false;
        }
    }
    true
}

/// Trial number from a `<parent>_trial<N>` id suffix. None when the id
/// carries no numeric trial suffix (parent ids, legacy shapes).
/// Shared helper (config.rs, no new import edges): the queue and the
/// scoring merge both key on trial order, so they parse ids one way.
pub fn trial_no_of(id: &str) -> Option<u32> {
    let (_, suffix) = id.rsplit_once("_trial")?;
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u32>().ok()
}

/// Display name for a stage log line: verifier stage names already embed
/// their own `"Stage N: "` prefix, so logging `"Stage {n}: {name}"` reads
/// `"Stage 1: Stage 1: ..."`. Strip the redundant prefix at display time;
/// stored names (manifests, pages) stay untouched.
pub fn stage_label(stage: u32, name: &str) -> String {
    let prefix = format!("Stage {stage}: ");
    name.strip_prefix(&prefix)
        .map(|s| s.to_string())
        .unwrap_or_else(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_stage_label_strips_redundant_prefix() {
        assert_eq!(stage_label(1, "Stage 1: Basic routing"), "Basic routing");
        assert_eq!(
            stage_label(6, "Stage 6: AOF crash recovery"),
            "AOF crash recovery"
        );
        // No/wrong prefix passes through untouched.
        assert_eq!(stage_label(1, "Basic routing"), "Basic routing");
        assert_eq!(
            stage_label(2, "Stage 1: Basic routing"),
            "Stage 1: Basic routing"
        );
        assert_eq!(stage_label(1, ""), "");
    }

    #[test]
    fn test_upstreams_reachable_against_live_listener() {
        // Healthy: bound listener answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("port").port();
        assert!(upstreams_reachable(&[port]));
        assert!(upstreams_reachable(&[port, port]));
        drop(listener);
        // Refused: reserve-then-drop guarantees nothing listens.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let dead_port = dead.local_addr().expect("port").port();
        drop(dead);
        assert!(!upstreams_reachable(&[dead_port]));
        assert!(!upstreams_reachable(&[port, dead_port]));
        assert!(upstreams_reachable(&[]));
    }

    #[test]
    fn test_template_ports_for_worker() {
        // Ref-alias lines untouched at any worker.
        let redis_ref = "redis-cli -h ref-redis -p 6379 <cmd>";
        assert_eq!(template_ports_for_worker(redis_ref, 6379, 2), redis_ref);
        let ups = "dial upstream1:80, mirrors 127.0.0.1:8101";
        assert_eq!(template_ports_for_worker(ups, 8091, 3), ups);
        // Candidate-local lines rewritten with word boundaries.
        let own = "listening on `0.0.0.0:6379`, test `curl localhost:6379`, `-p 6379`, port 6379";
        let got = template_ports_for_worker(own, 6379, 1);
        assert!(got.contains("6479"), "{got}");
        assert!(!got.contains("6379"), "{got}");
        // No coincidental digit-run corruption.
        let tricky = "id 16379 and 63790 stay";
        assert_eq!(template_ports_for_worker(tricky, 6379, 1), tricky);
        // Worker 0 verbatim.
        assert_eq!(template_ports_for_worker(own, 6379, 0), own);
        // Proxy contradiction (measured 2026-10-10): bare "upstream(s)"
        // prose must NOT shield candidate ports — only alias tokens with
        // attached ports are protected.
        let proxy_listen =
            "Your proxy listens on `0.0.0.0:8091` and forwards client `GET /` requests to the upstreams.";
        let got = template_ports_for_worker(proxy_listen, 8091, 3);
        assert!(got.contains("8391"), "{got}");
        assert!(!got.contains("8091"), "{got}");
        let proxy_docker = "1. **Working Dockerfile (Recommended)**: exposes port 8091.";
        assert!(template_ports_for_worker(proxy_docker, 8091, 3).contains("8391"));
        // Mixed line: candidate port rewritten, alias:port preserved.
        let mixed = "serve on :8091, origin upstream1:80 end";
        let got = template_ports_for_worker(mixed, 8091, 3);
        assert!(got.contains(":8391"), "{got}");
        assert!(got.contains("upstream1:80"), "{got}");
    }

    #[test]
    fn test_all_task_prompts_template_cleanly_per_worker() {
        // Every graded prompt, every worker: no stale canonical port may
        // survive outside alias contexts. An alias line may keep base
        // digits (fixed in-network ports); anything else is a prompt
        // contradiction that sends the model to the wrong port.
        use TaskType;
        let alias_pat =
            ::regex::Regex::new(r"(?i)(ref-redis|ref-http|upstream1|upstream2|upstream3)").unwrap();
        for task in [
            TaskType::Redis,
            TaskType::Http,
            TaskType::Pdf,
            TaskType::Proxy,
            TaskType::Pubsub,
            TaskType::Wal,
            TaskType::Binproto,
            TaskType::Queue,
        ] {
            let path = get_repo_root()
                .join("tasks")
                .join(task.to_string())
                .join("prompt.md");
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let base = task.default_port().to_string();
            let base_pat =
                ::regex::Regex::new(&format!(r"\b{}\b", ::regex::escape(&base))).unwrap();
            for worker in [1u8, 2, 3] {
                let out = template_ports_for_worker(&content, task.default_port(), worker);
                for line in out.lines() {
                    if base_pat.is_match(line) && !alias_pat.is_match(line) {
                        panic!(
                            "stale canonical port {base} on non-alias line (task {task}, worker {worker}): {line}"
                        );
                    }
                }
            }
        }
    }

    /// The dns task was removed 2026-10-05 (data archived): its name no
    /// longer parses, and every live task parses to itself.
    #[test]
    fn test_removed_dns_task_does_not_parse() {
        let err = TaskType::parse("dns").unwrap_err().to_string();
        assert!(
            err.contains("unknown task"),
            "expected an unknown-task error, got: {err}"
        );
        assert!(TaskType::from_name("dns").is_none());

        for t in ["redis", "http", "pdf", "wal", "proxy", "pubsub", "binproto"] {
            assert!(TaskType::parse(t).is_ok(), "{t} should be runnable");
        }
        assert!(TaskType::parse("nope").is_err());
    }

    /// Every `TaskType` Display name must round-trip through `parse`.
    #[test]
    fn test_every_task_parses_to_itself() {
        for t in [
            TaskType::Redis,
            TaskType::Http,
            TaskType::Pdf,
            TaskType::Wal,
            TaskType::Proxy,
            TaskType::Pubsub,
            TaskType::Binproto,
        ] {
            let name = t.to_string();
            assert_eq!(TaskType::parse(&name).unwrap(), t, "{name} should parse");
        }
    }

    #[test]
    fn test_task_type_display_and_serde() {
        assert_eq!(format!("{}", TaskType::Redis), "redis");
        assert_eq!(format!("{}", TaskType::Http), "http");
        assert_eq!(format!("{}", TaskType::Pdf), "pdf");

        assert_eq!(TaskType::Redis.total_stages(), 6);
        assert_eq!(TaskType::Http.total_stages(), 5);
        assert_eq!(TaskType::Pdf.total_stages(), 6);
        assert_eq!(TaskType::Wal.total_stages(), 5);
        assert_eq!(TaskType::Proxy.total_stages(), 5);
        assert_eq!(TaskType::Pubsub.total_stages(), 4);
        assert_eq!(TaskType::Binproto.total_stages(), 5);

        assert_eq!(TaskType::Redis.default_port(), 6379);
        assert_eq!(TaskType::Http.default_port(), 8080);
        assert_eq!(TaskType::Binproto.default_port(), 6395);

        let json_redis = serde_json::to_string(&TaskType::Redis).unwrap();
        assert_eq!(json_redis, "\"redis\"");
        let de_redis: TaskType = serde_json::from_str(&json_redis).unwrap();
        assert_eq!(de_redis, TaskType::Redis);

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
                variance: _,
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
            Commands::Ui { port, host, .. } => {
                assert_eq!(port, 8080);
                assert_eq!(host, "127.0.0.1");
            }
            _ => panic!("Expected Ui command"),
        }

        let args_default = ["subdollarbench", "ui"];
        let cli_default = Cli::try_parse_from(args_default).unwrap();
        match cli_default.command {
            Commands::Ui { port, host, .. } => {
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

        let args_update = [
            "subdollarbench",
            "sandbox",
            "update",
            "--version",
            "18.1.21",
        ];
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
