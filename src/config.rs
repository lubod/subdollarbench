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
        #[arg(long, env = "OPENROUTER_API_KEY", hide_env_values = true)]
        api_key: Option<String>,

        /// Directory for candidate workspace
        #[arg(long, default_value = "./workspace")]
        workdir: String,

        /// Skip OMP agent run and evaluate existing server directly
        #[arg(long, default_value_t = false)]
        eval_only: bool,

        /// Number of repeated trials for statistical variance (Pass@k).
        /// Nine by default: n=3 point estimates span +/-40pt; Wilson
        /// intervals only become interpretable at n>=9.
        #[arg(long, default_value_t = 9)]
        trials: u32,

        /// Do not save results or archive run artifacts to disk
        #[arg(long, default_value_t = false)]
        no_save: bool,

        /// Enqueue on the running `ui` engine instead of running foreground.
        /// Foreground runs fight a live engine over docker and task ports.
        #[arg(long, default_value_t = false)]
        queue: bool,

        /// `ui` engine base URL (only used with --queue). Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,

        /// Model used for the end-of-run code review judge (defaults to openrouter/meta/muse-spark-1.3-contributor)
        #[arg(long)]
        review_model: Option<String>,

        /// Keep adding trials in batches of 3 while the result is unstable
        #[arg(long, default_value_t = false)]
        extend_if_unstable: bool,

        /// Trial cap for auto-extension (total, including the initial batch).
        /// Seven: wave-v2 showed no verdict change 7v9 on 5/5 extended cells.
        #[arg(long, default_value_t = 7)]
        max_trials: u32,
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

        /// Filter by task ('http', 'redis', 'pdf', or 'all' for per-task + consolidated)
        #[arg(short, long, default_value = "all")]
        task: String,
    },

    /// Print the effort x model P@1-lower-bound matrix per task
    MatrixReport {
        /// Directory containing result JSON files
        #[arg(short, long, default_value = "./results")]
        results_dir: String,
    },

    /// Synthesize a whole-run AI review: deterministic stats across all trials
    /// (computed in code) + one judge call for code/architecture synthesis.
    /// Advisory-only, never touches scores. Writes runs/<id>/run-review.md.
    ReviewRun {
        /// Parent run id (multi-trial parent, e.g. redis_..._w0)
        run_id: String,

        /// Path to runs directory
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// Judge model for the synthesis
        #[arg(long, default_value = "openrouter/meta/muse-spark-1.3-contributor")]
        model: String,

        /// OpenRouter API Key (optional, defaults to OPENROUTER_API_KEY env var)
        #[arg(long, env = "OPENROUTER_API_KEY", hide_env_values = true)]
        api_key: Option<String>,

        /// Overwrite existing run-review.md
        #[arg(long, default_value_t = false)]
        force: bool,
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
        #[arg(long, env = "OPENROUTER_API_KEY", hide_env_values = true)]
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

        /// Model-rollup-only portal (one row per model, no per-run pages)
        #[arg(long, default_value_t = false)]
        rollup_only: bool,
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

    /// [operate] List recent runs (newest first; merged-away trials hidden)
    Runs {
        /// Filter by task ('http', 'redis', 'pdf', ...)
        #[arg(short, long)]
        task: Option<String>,

        /// Filter by model substring (e.g. 'muse-spark', 'glm')
        #[arg(long)]
        model: Option<String>,

        /// Filter by effort ('minimal', 'low', 'medium', 'high', 'xhigh', 'max')
        #[arg(long)]
        effort: Option<String>,

        /// Filter by status ('completed', 'failed_tests', 'disqualified_cheat', ...)
        #[arg(long)]
        status: Option<String>,

        /// Maximum rows to show
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Include merged-away trial subruns
        #[arg(long, default_value_t = false)]
        all: bool,

        /// Path to runs directory
        #[arg(long, default_value = "./runs")]
        runs_dir: String,
    },

    /// [analyze] One-screen verdict for a run (exit 0 pass / 1 fail / 2 infra-error)
    Verdict {
        /// Run id or unambiguous prefix
        run: String,

        /// Emit machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,

        /// Path to runs directory
        #[arg(long, default_value = "./runs")]
        runs_dir: String,
    },

    /// [analyze] Stage-by-stage diff of two runs (same task)
    Diff {
        /// Baseline run id or prefix
        a: String,

        /// Candidate run id or prefix
        b: String,

        /// Path to runs directory
        #[arg(long, default_value = "./runs")]
        runs_dir: String,
    },

    /// [analyze] Trial-stability assessment for a consolidated run (exit 0 stable / 1 needs trials)
    Stability {
        /// Parent run id or unambiguous prefix
        parent: String,

        /// Path to results directory
        #[arg(long, default_value = "./results")]
        results_dir: String,
    },

    /// [operate] Plan an effort x model x task comparison wave (prints shell commands)
    #[command(hide = true)]
    Matrix {
        /// Comma-separated models (default: all four cheap models)
        #[arg(
            long,
            default_value = "openrouter/meta/muse-spark-1.3-contributor,openrouter/z-ai/glm-5.3-flash,openrouter/deepseek/deepseek-v4.1-flash,openrouter/qwen/qwen3.8-flash"
        )]
        models: String,

        /// Comma-separated efforts (default: minimal,max — the uncrossed extremes)
        #[arg(long, default_value = "minimal,max")]
        efforts: String,

        /// Comma-separated tasks (default: redis,proxy,pdf — easy, discriminating, GLM-weak)
        #[arg(long, default_value = "redis,proxy,pdf")]
        tasks: String,

        /// Trials per cell
        #[arg(long, default_value_t = 3)]
        trials: u32,

        /// Extra flags appended to every generated `run` command
        #[arg(long, default_value = "")]
        extra: String,

        /// Explicit per-effort-columns mode: efforts are displayed as
        /// separate columns and never pooled into one ranking. Without
        /// this flag a matrix wave requires uniform effort.
        #[arg(long, default_value_t = false)]
        per_effort_columns: bool,

        /// Enqueue cells on the running engine (POST /api/queue) instead of
        /// printing `run` commands. Low-effort cells go first (cheap
        /// capability probe before expensive runs).
        #[arg(long, default_value_t = false)]
        enqueue: bool,

        /// Trials extension: keep adding trials while unstable (enqueue mode).
        #[arg(long, default_value_t = false)]
        extend_if_unstable: bool,

        /// Trial cap for auto-extension (enqueue mode; 7 per wave-v2 evidence).
        #[arg(long, default_value_t = 7)]
        max_trials: u32,

        /// `ui` engine base URL (enqueue mode only).
        #[arg(long)]
        server: Option<String>,
    },

    /// [operate] Run 3 more trials for an unstable parent and re-merge
    Extend {
        /// Parent run id or unambiguous prefix
        parent: String,

        /// Extra trials to run
        #[arg(long, default_value_t = 3)]
        trials: u32,

        /// Override model (defaults to the parent's)
        #[arg(long)]
        model: Option<String>,

        /// Override task (defaults to the parent's)
        #[arg(long)]
        task: Option<TaskType>,

        /// Override effort (defaults to the parent's)
        #[arg(long)]
        effort: Option<String>,

        /// Maximum turns/steps allowed for OMP
        #[arg(long, default_value_t = 100)]
        max_turns: u32,

        /// Maximum cost budget in USD
        #[arg(long, default_value_t = 1.00)]
        budget_usd: f64,

        /// Maximum execution time in minutes before stopping agent
        #[arg(long, default_value_t = 30)]
        timeout_min: u64,

        /// OpenRouter API Key (optional, defaults to OPENROUTER_API_KEY env var)
        #[arg(long, env = "OPENROUTER_API_KEY", hide_env_values = true)]
        api_key: Option<String>,

        /// Directory for candidate workspace
        #[arg(long, default_value = "./workspace")]
        workdir: String,

        /// Skip OMP agent run and evaluate existing server directly
        #[arg(long, default_value_t = false)]
        eval_only: bool,

        /// Model used for the end-of-run code review judge
        #[arg(long)]
        review_model: Option<String>,

        /// Path to results directory
        #[arg(long, default_value = "./results")]
        results_dir: String,
    },

    /// [analyze] Per-stage pass/fail detail for a run
    Stages {
        /// Run id or unambiguous prefix
        run: String,

        /// Trial number to inspect (appends _trial<N> when the id has no trial suffix)
        #[arg(long)]
        trial: Option<u32>,

        /// Emit machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,

        /// Path to runs directory
        #[arg(long, default_value = "./runs")]
        runs_dir: String,
    },

    /// [maintain] Remove leaked subdollar Docker containers and networks
    Prune {
        /// Actually delete (without it, only prints the plan)
        #[arg(long, default_value_t = false)]
        yes: bool,
    },

    /// [maintain] Re-merge all saved parents with current scoring (honest avg
    /// headlines, Wilson intervals, rules_version stamps, unscored refless tp).
    /// Dry run by default; --yes writes.
    Backfill {
        /// Path to results directory
        #[arg(long, default_value = "./results")]
        results_dir: String,

        /// Actually rewrite files (without it, only prints the plan)
        #[arg(long, default_value_t = false)]
        yes: bool,
    },

    /// [maintain] Report-only integrity audit (orphans, junk rows, row-file drift).
    /// Exit 1 on findings. Also runs warn-only on every engine boot.
    MigrateCheck,

    /// [maintain] One-shot import of results/*.json export copies into bench.db.
    /// Dry run by default; --yes writes.
    Migrate {
        /// Path to results directory
        #[arg(long, default_value = "./results")]
        results_dir: String,

        /// Actually write rows to bench.db (without it, only prints the plan)
        #[arg(long, default_value_t = false)]
        yes: bool,
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

    /// [operate] Merged engine view: live queue head + archived manifests with filters
    #[command(hide = true)]
    Ps {
        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,

        /// Filter by model substring
        #[arg(long)]
        model: Option<String>,

        /// Filter by task
        #[arg(short, long)]
        task: Option<String>,

        /// Filter by effort
        #[arg(long)]
        effort: Option<String>,

        /// Filter by status (exact, case-insensitive)
        #[arg(long)]
        status: Option<String>,

        /// Maximum archived rows to show
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Path to runs directory
        #[arg(long, default_value = "./runs")]
        runs_dir: String,
    },

    /// [operate] Enqueue one run behind the live job (POST /api/queue)
    Enqueue {
        /// Model identifier on OpenRouter
        #[arg(short, long)]
        model: String,

        /// Task to run
        #[arg(short, long, value_enum, default_value_t = TaskType::Redis)]
        task: TaskType,

        /// Reasoning effort / thinking level
        #[arg(short = 'e', long, default_value = "high")]
        effort: String,

        /// Maximum turns/steps allowed for OMP
        #[arg(long, default_value_t = 100)]
        max_turns: u32,

        /// Maximum cost budget in USD
        #[arg(short = 'b', long, default_value_t = 1.00)]
        budget_usd: f64,

        /// Maximum execution time in minutes
        #[arg(long, default_value_t = 30)]
        timeout_min: u64,

        /// OpenRouter API Key (optional, defaults to OPENROUTER_API_KEY env var)
        #[arg(long, env = "OPENROUTER_API_KEY", hide_env_values = true)]
        api_key: Option<String>,

        /// Skip OMP agent run and evaluate existing server directly
        #[arg(long, default_value_t = false)]
        eval_only: bool,

        /// Number of repeated trials
        #[arg(long, default_value_t = 9)]
        trials: u32,

        /// Keep adding trials in batches of 3 while the result is unstable
        #[arg(long, default_value_t = false)]
        extend_if_unstable: bool,

        /// Trial cap for auto-extension (7 per wave-v2 evidence).
        #[arg(long, default_value_t = 7)]
        max_trials: u32,

        /// Do not save results or archive run artifacts to disk
        #[arg(long, default_value_t = false)]
        no_save: bool,

        /// Model used for the end-of-run code review judge
        #[arg(long)]
        review_model: Option<String>,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [operate] Queue an extension behind the live job (POST /api/runs/:id/extend)
    #[command(hide = true)]
    QueueExtend {
        /// Parent run id or unambiguous prefix
        parent: String,

        /// Extra trials to run
        #[arg(long, default_value_t = 3)]
        trials: u32,

        /// Override max turns (defaults to the server default)
        #[arg(long)]
        max_turns: Option<u32>,

        /// Override budget in USD (defaults to the server default)
        #[arg(long)]
        budget_usd: Option<f64>,

        /// Override timeout in minutes (defaults to the server default)
        #[arg(long)]
        timeout_min: Option<u64>,

        /// Skip OMP agent run and evaluate existing server directly
        #[arg(long, default_value_t = false)]
        eval_only: bool,

        /// Path to results directory (for prefix resolution)
        #[arg(long, default_value = "./results")]
        results_dir: String,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
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

    /// [analyze] Archived manifest for a run (GET /api/runs/:id)
    Detail {
        /// Run id or unambiguous prefix (resolved locally)
        run: String,

        /// Emit raw JSON
        #[arg(long, default_value_t = false)]
        json: bool,

        /// Path to runs directory (for prefix resolution)
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [analyze] Stage-by-stage server diff of two runs (GET /api/compare)
    #[command(hide = true)]
    Sdiff {
        /// Baseline run id or prefix
        a: String,

        /// Candidate run id or prefix
        b: String,

        /// Path to runs directory (for prefix resolution)
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [analyze] Workspace file list for a run (GET /api/runs/:id/files)
    Ls {
        /// Run id or unambiguous prefix (resolved locally)
        run: String,

        /// Path to runs directory (for prefix resolution)
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [analyze] One workspace file for a run (GET /api/runs/:id/file)
    Cat {
        /// Run id or unambiguous prefix (resolved locally)
        run: String,

        /// Workspace-relative file path
        file: String,

        /// Path to runs directory (for prefix resolution)
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [analyze] Code review judge output for a run (GET /api/runs/:id/review)
    Review {
        /// Run id or unambiguous prefix (resolved locally)
        run: String,

        /// Path to runs directory (for prefix resolution)
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// `ui` engine base URL. Precedence: --server flag > SDB_SERVER env > http://127.0.0.1:3000 default.
        #[arg(long)]
        server: Option<String>,
    },

    /// [analyze] Leaderboard rows from the engine (GET /api/leaderboard)
    #[command(hide = true)]
    Lboard {
        /// Filter by task ('http', 'redis', 'pdf', or 'all')
        #[arg(short, long, default_value = "all")]
        task: String,

        /// Emit raw JSON
        #[arg(long, default_value_t = false)]
        json: bool,

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
    fn test_cli_parsing_matrix() {
        let cli = Cli::try_parse_from(["subdollarbench", "matrix"]).unwrap();
        match cli.command {
            Commands::Matrix {
                models,
                efforts,
                tasks,
                trials,
                extra,
                per_effort_columns,
                ..
            } => {
                assert!(models.contains("muse-spark"));
                assert_eq!(efforts, "minimal,max");
                assert_eq!(tasks, "redis,proxy,pdf");
                assert_eq!(trials, 3);
                assert_eq!(extra, "");
                assert!(!per_effort_columns);
            }
            _ => panic!("Expected Matrix command"),
        }
        let cli2 = Cli::try_parse_from([
            "subdollarbench",
            "matrix",
            "--models",
            "a,b",
            "--efforts",
            "minimal",
            "--tasks",
            "redis",
            "--trials",
            "9",
        ])
        .unwrap();
        match cli2.command {
            Commands::Matrix {
                models,
                efforts,
                tasks,
                trials,
                ..
            } => {
                assert_eq!(models, "a,b");
                assert_eq!(efforts, "minimal");
                assert_eq!(tasks, "redis");
                assert_eq!(trials, 9);
            }
            _ => panic!("Expected Matrix command"),
        }
    }

    #[test]
    fn test_cli_parsing_engine_commands() {
        // `SDB_SERVER` is process-global: hold the shared env guard so
        // parallel env tests (e.g. remote's resolver test) cannot leak in.
        let _env_guard = crate::remote::keep_env();
        let cli = Cli::try_parse_from(["subdollarbench", "status"]).unwrap();
        match cli.command {
            Commands::Status { server, json } => {
                assert_eq!(server, None);
                assert!(!json);
                assert_eq!(
                    crate::remote::resolve_server(server.as_deref()),
                    crate::remote::DEFAULT_SERVER
                );
            }
            _ => panic!("Expected Status command"),
        }
        // SDB_SERVER fills --server when the flag is absent; the flag wins.
        // NOTE: clap `env` is deliberately NOT used on --server (a hard
        // default_value would shadow the env var). The field stays None and
        // the canonical remote::resolve_server applies flag > env > default.
        std::env::set_var("SDB_SERVER", "http://env:4000");
        let cli = Cli::try_parse_from(["subdollarbench", "status"]).unwrap();
        match cli.command {
            Commands::Status { server, .. } => {
                // Clap `env` pickup is disabled, so the field stays None and
                // the canonical resolver applies the env value.
                assert_eq!(server, None);
                assert_eq!(
                    crate::remote::resolve_server(server.as_deref()),
                    "http://env:4000"
                );
                // Direct resolver contract: flag > env > default.
                assert_eq!(
                    crate::remote::resolve_server(Some("http://flag:5000")),
                    "http://flag:5000"
                );
            }
            _ => panic!("Expected Status command"),
        }
        // The explicit flag also wins while the env var is set.
        let cli = Cli::try_parse_from(["subdollarbench", "status", "--server", "http://flag:5000"])
            .unwrap();
        match cli.command {
            Commands::Status { server, .. } => {
                assert_eq!(server, Some("http://flag:5000".to_string()));
                assert_eq!(
                    crate::remote::resolve_server(server.as_deref()),
                    "http://flag:5000"
                );
            }
            _ => panic!("Expected Status command"),
        }
        std::env::remove_var("SDB_SERVER");
        let cli = Cli::try_parse_from(["subdollarbench", "queue", "--json"]).unwrap();
        match cli.command {
            Commands::Queue { json, .. } => assert!(json),
            _ => panic!("Expected Queue command"),
        }
        let cli = Cli::try_parse_from(["subdollarbench", "ps", "--model", "muse", "--limit", "5"])
            .unwrap();
        match cli.command {
            Commands::Ps { model, limit, .. } => {
                assert_eq!(model, Some("muse".to_string()));
                assert_eq!(limit, 5);
            }
            _ => panic!("Expected Ps command"),
        }
        let cli = Cli::try_parse_from([
            "subdollarbench",
            "enqueue",
            "--model",
            "m",
            "--task",
            "redis",
            "--effort",
            "max",
        ])
        .unwrap();
        match cli.command {
            Commands::Enqueue { model, trials, .. } => {
                assert_eq!(model, "m");
                assert_eq!(trials, 9);
            }
            _ => panic!("Expected Enqueue command"),
        }
        let cli =
            Cli::try_parse_from(["subdollarbench", "queue-extend", "wal_m_1", "--trials", "6"])
                .unwrap();
        match cli.command {
            Commands::QueueExtend { parent, trials, .. } => {
                assert_eq!(parent, "wal_m_1");
                assert_eq!(trials, 6);
            }
            _ => panic!("Expected QueueExtend command"),
        }
        let cli = Cli::try_parse_from(["subdollarbench", "dequeue", "q4"]).unwrap();
        match cli.command {
            Commands::Dequeue { id, .. } => assert_eq!(id, "q4"),
            _ => panic!("Expected Dequeue command"),
        }
        for name in [
            "stop",
            "queue-clear",
            "env",
            "omp",
            "portal-rebuild",
            "models",
        ] {
            let cli = Cli::try_parse_from(["subdollarbench", name]).unwrap();
            match cli.command {
                Commands::Stop { .. }
                | Commands::QueueClear { .. }
                | Commands::Env { .. }
                | Commands::Omp { .. }
                | Commands::PortalRebuild { .. }
                | Commands::Models { .. } => {}
                _ => panic!("Expected {name} command"),
            }
        }
        let cli =
            Cli::try_parse_from(["subdollarbench", "run", "--model", "m", "--queue"]).unwrap();
        match cli.command {
            Commands::Run { queue, .. } => assert!(queue),
            _ => panic!("Expected Run command"),
        }
        let cli = Cli::try_parse_from([
            "subdollarbench",
            "runs",
            "--model",
            "glm",
            "--status",
            "completed",
        ])
        .unwrap();
        match cli.command {
            Commands::Runs { model, status, .. } => {
                assert_eq!(model, Some("glm".to_string()));
                assert_eq!(status, Some("completed".to_string()));
            }
            _ => panic!("Expected Runs command"),
        }
        let cli = Cli::try_parse_from([
            "subdollarbench",
            "models",
            "--family",
            "glm",
            "--sort",
            "newest",
        ])
        .unwrap();
        match cli.command {
            Commands::Models { family, sort, .. } => {
                assert_eq!(family, Some("glm".to_string()));
                assert_eq!(sort, "newest");
            }
            _ => panic!("Expected Models command"),
        }
        let cli = Cli::try_parse_from(["subdollarbench", "hide", "abc123"]).unwrap();
        match cli.command {
            Commands::Hide { run, show, .. } => {
                assert_eq!(run, "abc123");
                assert!(!show);
            }
            _ => panic!("Expected Hide command"),
        }
        let cli = Cli::try_parse_from(["subdollarbench", "rm", "abc123", "--yes"]).unwrap();
        match cli.command {
            Commands::Rm { yes, .. } => assert!(yes),
            _ => panic!("Expected Rm command"),
        }
    }

    #[test]
    fn test_cli_parsing_matrix_report() {
        let cli = Cli::try_parse_from(["subdollarbench", "matrix-report"]).unwrap();
        match cli.command {
            Commands::MatrixReport { results_dir } => assert_eq!(results_dir, "./results"),
            _ => panic!("Expected MatrixReport command"),
        }
    }

    #[test]
    fn test_trials_default_nine() {
        let cli = Cli::try_parse_from(["subdollarbench", "run", "--model", "m", "--task", "redis"])
            .unwrap();
        match cli.command {
            Commands::Run {
                trials, max_trials, ..
            } => {
                assert_eq!(trials, 9);
                assert_eq!(max_trials, 7);
            }
            _ => panic!("Expected Run command"),
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
