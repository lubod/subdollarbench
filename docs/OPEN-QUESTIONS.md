# Open questions needing a product call

Collected 2026-10-14 (end of the campaign build day). Each item has the
data, the options, and a recommendation. Nothing here blocks the current
ladder (tickets 0-9 are built); these shape what comes next.

## Q1. Ticket 10 (three-node HA finale) — failover semantics & partition scope

The proposal (`docs/PROPOSAL-iterative-campaigns.md`, "Stage 10 — HA
contract") already specifies the *shape*: launch contract
`./start.sh --port <p> --data <dir> [--replicaof host:port]`; nodes at
7001/7002/7003 (+ worker offset); `ROLE` master/replica; replication with
reads on replicas within 3s; sync-ack (a write is acknowledged only after
>=1 replica confirms); failover ("within T seconds exactly one survivor
reports `ROLE master`"); rejoin ("restart the old master; it returns as a
replica, never co-master, and converges"); the full accumulated suite
re-runs against the cluster's current master. The partition test is
marked **(stretch)**.

What is still a judgment call:

- **D1 — failover mechanics.** The contract implies *automatic* failover
  (nothing about an operator PROMOTE command). Options:
  - **(a) Automatic, deterministic rule** (recommended): survivors pick
    the replica with the highest replication offset (tie: lowest port),
    and it becomes master within T. Test-friendly: SIGKILL the master,
    poll `ROLE` on both survivors, require exactly one master within T,
    and every acked write present.
  - **(b) Manual promotion**: the harness sends a `PROMOTE` command.
    More deterministic for testing, but adds protocol surface and is a
    weaker HA story ("ops must intervene").
- **T** (the failover deadline): propose **10 seconds** (generous for
  budget models; the harness uses a 45s stage timeout).
- **D2 — partition scope.** Proposed: build the core replica/sync-ack/
  failover/rejoin checks NOW; leave the network-partition stretch check
  (isolate a minority, majority keeps serving, minority cannot elect
  itself) as a follow-up ticket or a lenient advisory probe. Building
  real partition safety (split-brain prevention) is a consensus-sized
  problem; the core contract already distinguishes good from bad clusters.
- **Harness shape**: three candidate containers on the run network
  (aliases `ha1/ha2/ha3`), same workspace image, separate `--data` dirs,
  ports published for the harness. One new sandbox method + a stateful
  verifier; no model-visible difference beyond the launch contract.

**Ask: confirm (a) automatic with the offset rule and T=10s, and
partition-as-follow-up. Then ticket 10 can be built.**

## Q2. s3 (durability) pool calibration — the wall every model hits

Data (2026-10-14): ling failed s3 **four consecutive times** in the same
class — the repair round ran out mid-refactor while fixing compile
errors (runs 4, 5, 7, 8). Its s3 attempts are large refactors needing
~36+ turns total; the ticket pool is 30 (reserve 12 -> initial 18 after
the D6 amendment). Earlier, with 22 initial + 8 repair, ling passed s3
once (run 2, Python path) and failed once (run 4, Go path).

Options:
- **(a) Raise s3's pool to 36** (initial ~24 + repair 12): the model's
  initial attempt gets closer to a working build; cost per attempt rises
  (~20%). Recommended if the goal is to let models reach tickets 4-9
  (which are otherwise never exercised live).
- **(b) Raise the repair floor to 16** (initial 14): more repair room,
  less initial — mismatched to ling's profile (it under-uses repairs for
  surveying).
- **(c) Keep 30**: s3 stays the discriminator; the ladder's upper half
  rarely yields data.

**Ask: (a), (b), or (c)?** Nemotron's cell tonight adds a second model's
data point to this call.

## Q3. Perf baseline visibility (small)

Ticket 9's gate compares against the candidate's own genesis throughput.
The perf prompt cannot currently state the baseline *number* (prompt
files are static). Models optimize blind — fine for v1 (a Go rewrite
clears 1.3x easily), but if the grid shows near-misses, render the
baseline into the ticket prompt (needs a prompt-template hook in the
campaign loader).

## Q4. Scoring v2 (already written up; unchanged)

Throughput curve / SPRT vs top-up / CI coverage gate -- see
`docs/PROPOSAL-scoring-v2.md`. Still awaiting a decision.
