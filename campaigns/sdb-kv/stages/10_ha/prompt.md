# Ticket 10 — Three-node HA: the enterprise finale

Last ticket. The board wants the store to survive a machine dying: three
nodes, one leader, synchronous acknowledgement, automatic failover, and a
clean rejoin. Keep everything earlier green — the acceptance suite re-runs
the accumulated RESP tests and your `test.sh` against the cluster's
*current master*.

## Launch contract (new)

`./start.sh --port <p> --data <dir> [--replicaof <host>:<port>]`

- **Bare `./start.sh`** must keep the single-node behavior every earlier
  ticket relies on (RESP on 6379). The cluster flags are opt-in.
- `--port <p>`: serve RESP on that port.
- `--data <dir>`: that node's persistent directory (create it if needed;
  each node keeps its own state — do not share files between nodes).
- `--replicaof <host>:<port>`: start as a replica of that node's leader.
- `ROLE` must answer a RESP reply whose first element is `master` or
  `replica` (an array like real Redis, or a simple string — the harness
  accepts either).

## Contract additions

1. **Replication.** Writes applied on the master must be readable on every
   replica within **3 seconds**; replicas serve reads themselves.
2. **Sync-ack.** A write may only be acknowledged *after at least one
   replica has confirmed it* — the harness writes N keys to the master,
   then kills it, and requires every acknowledged write on the survivor.
3. **Automatic failover.** The harness `SIGKILL`s the master. Within
   **10 seconds exactly one** of the two survivors must report
   `ROLE master` (the harness accepts either survivor — pick a
   deterministic rule, e.g. highest replication offset), hold every
   acknowledged write, and accept new writes that must reach the other
   node within 3s. Two masters is a failure.
4. **Rejoin.** The harness restarts the old master pointed at the new
   leader (`--replicaof`). It must return as a **replica** — never a
   co-master — and converge to the current data set.
5. **Regression.** The accumulated RESP suite (tickets 0-5) and your
   `./test.sh` (ticket 6) must stay green against the cluster's current
   master.

## What the acceptance suite does

Starts ha1 (master) + ha2/ha3 (replicas) on ports 7001/7002/7003 with
separate `--data` dirs; checks `ROLE` on all three; writes 6 keys to the
master and requires at least one replica to have them within 3s;
SIGKILLs ha1; polls both survivors and requires exactly one master within
10s with all 6 acked keys present; writes 4 more and checks the other
node; restarts ha1 with `--replicaof` at the new leader and requires a
replica role plus full convergence within 10s; then re-runs the RESP
suite and `./test.sh` against the current master.

## Constraints

- Standard library only; raw sockets.
- The single-node path must not regress (bare `./start.sh` on 6379).
- Everything earlier must stay green.
