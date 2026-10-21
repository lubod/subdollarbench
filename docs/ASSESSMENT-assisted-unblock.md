# Assessment: "assisted unblock" for career phases

**Date:** 2026-10-21. **Status:** not built; assessment only (P2 item 3).

The proposal was to apply a verified per-ticket reference patch when a
phase's catch-up also fails, score the ticket 0, and let the career continue
with its `needs` met. The table below is the half-page feasibility check
requested before anyone builds it.

| Question | Finding |
|---|---|
| What is a "per-ticket reference patch"? | The reference adapters are harness-owned implementations of the *task* (e.g. `ref_server.rs` for sdb-sql), not diffs against a candidate. A per-ticket reference patch would have to be a semantic patch to arbitrary model code, not a git diff against the reference. |
| Can it apply to arbitrary model code? | Rarely. Tickets touch different architectures, file layouts and languages; a reference hunk can apply textually while being semantically wrong, and `git apply --3way` conflicts on most real workspaces. A full reference drop-in would replace the model's work and stop measuring the model. |
| What would the score mean? | The ticket scores 0, but later tickets become runnable; the career total would mix unassisted and assisted points. That needs a separate board class/flag and a career `assisted` field, not a hidden sidebar. |
| Leak risk | Shipping per-ticket reference patches to the agent (or into the workspace) leaks the official implementation; even a post-hoc patch in the cell directory is readable by a later ticket. |
| Alternatives already available | (1) The P2 catch-up slots retry a failed prerequisite once at 0.4 x tier; (2) the P2 hints give the recorded verifier failure, repro, blocked tickets and SPEC.md section with no points cost; (3) smaller prerequisite tickets / more feedback rounds are cheaper and keep the measurement honest. |

**Recommendation: do not build the assisted unblock.** It is likely
infeasible in the general case, it changes what the benchmark measures,
and it creates a leak and comparability problem. If it is ever revisited,
it must be a separate "assisted" career class: apply only to a scratch
branch, never overwrite the model's workspace silently, score the ticket 0
and record the patch id, and keep it out of the unassisted board.

