# OpenCode harness — execution TODO (2026-10-07)

Test model: `opencode/ling-3.1-flash-free` (free tier, no spend).

- [ ] 0. Spikes (gates)
  - [x] 0a. Headless `opencode run` in temp dir: exits non-interactively, writes files
  - [x] 0b. Auth headless (stored Console) — no interactive login
  - [x] 0c. Usage stats: `--format json` / `session export` → prompt/cached/completion
  - [x] 0d. Effort mapping (`#variant`?) or request-only verdict
  - [x] 0e. Policy equivalent (`opencode plugin` / project config) or AGENTS.md-only
  - [x] 0f. Session isolation per run (`--session <id>`, storage layout)
- [x] A. Trait abstraction: `AgentSessionStats` + `AgentRunner`, OMP impl, pipeline via trait, tests green
- [x] B. `OpenCodeRunner` (`src/sandbox/opencode.rs`): docker launch, json/stdout reader, `[TELEMETRY]/[LIVE]` lines, limits, shared `task_guidance()`, billing + normalizer reuse
- [x] C. Plumbing: `--harness omp|opencode` (Run/Matrix/Enqueue), `BenchmarkConfig`, `RunRequest`, manifest `harness`/`harness_version`, leaderboard dimension (never mixed ranks), portal note
- [x] D. Image + UI: `Dockerfile.sandbox` `ARG OPENCODE_VERSION`, Setup harness dropdown, status pill, `queueRerun` harness
- [x] E. Validation: 1 trial redis × ling-3.1-flash-free via opencode; manifest stamps; verifier runs; full `cargo test`, `fmt`, `clippy`
