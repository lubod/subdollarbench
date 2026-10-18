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
pub use omp::{AgentExecutionLimits, AgentRunSpec, OmpRunner, OmpSessionStats};
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
mod tests {
    use super::*;

    /// Round-13 R1: a prompt that starts with `-` must arrive as one
    /// positional value after the `--` separator.
    #[test]
    fn test_push_prompt_args_keep_a_dash_leading_prompt_positional() {
        let mut args = vec!["-p".to_string(), "--model=x".to_string()];
        push_prompt_args(
            &mut args,
            "--- harness note (added by the benchmark, not the ticket) ---\nbody",
        );
        assert_eq!(args.len(), 4);
        assert_eq!(args[2], "--", "the separator precedes the prompt");
        assert!(args[3].starts_with("---"), "the prompt arrives verbatim");
        assert_eq!(
            args.iter().filter(|a| a.contains("harness note")).count(),
            1,
            "exactly one argument carries the prompt: {args:?}"
        );
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert!(
            args[..sep].iter().all(|a| a.starts_with('-')),
            "nothing before the separator is the prompt: {args:?}"
        );
    }

    /// Source guard: neither harness builder may hand the prompt to
    /// `.arg(...)` directly, or the 0-turn-agent bug comes straight back.
    #[test]
    fn test_harness_builders_route_the_prompt_through_the_separator() {
        for (name, src) in [
            ("omp.rs", include_str!("omp.rs")),
            ("opencode.rs", include_str!("opencode.rs")),
        ] {
            assert!(
                src.contains("push_prompt_args("),
                "{name} must pass the prompt via push_prompt_args"
            );
            assert!(
                !src.contains(".arg(&effective_prompt)") && !src.contains(".arg(effective_prompt)"),
                "{name} must not pass the prompt as a bare positional"
            );
        }
    }
}
