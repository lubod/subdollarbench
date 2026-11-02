/// Round-13 R1: both harnesses take the task prompt as a *positional*
/// argument and parse options with a leading-dash parser. A prompt that
/// begins with `-` (the campaign's harness note used to start with `---`)
/// was read as an unknown flag, so the agent never ran a turn and the
/// verifier graded the previous ticket's code. Every builder ends its argv
/// with `--` and then the prompt.
pub(crate) fn push_prompt_args(args: &mut Vec<String>, prompt: &str) {
    args.push("--".to_string());
    args.push(prompt.to_string());
}

pub mod docker;
pub mod omp;
pub mod omp_updater;
pub mod opencode;
pub mod tool_normalizer;

pub use docker::SandboxManager;
pub use omp::{AgentExecutionLimits, AgentRunSpec, OmpRunner, OmpSessionStats, ToolStall};
pub use omp_updater::{
    auto_update_if_needed, check_status as check_omp_status, update_sandbox_image, OmpVersionStatus,
};
pub use opencode::{OpenCodeRunner, OPENCODE_VERSION};
pub use tool_normalizer::extract_tool_calls;

/// Harness-agnostic agent runner. OMP and OpenCode both implement this;
/// `pipeline.rs` dispatches on the run's `harness` field. Stats use the
/// shared `OmpSessionStats` shape (prompt/cached/completion); harness
/// adapters map their native usage into it (see `opencode.rs` mapping).
pub trait AgentRunner {
    fn run_agent_with_logger<F>(
        spec: AgentRunSpec<'_>,
        log_fn: F,
    ) -> anyhow::Result<OmpSessionStats>
    where
        F: FnMut(String) + Send + 'static;
}

#[cfg(test)]
mod tests;
