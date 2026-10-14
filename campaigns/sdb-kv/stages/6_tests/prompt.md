# Ticket 6 — Tests & CI gate

The server has grown through five tickets. From now on, every change must
ship with tests — and CI must enforce it. This ticket adds the test suite
and the gate; later tickets will keep them green.

## Deliverable

1. **`./test.sh` at the repo root**: runs your test suite and exits 0 on
   success, non-zero on any failure. It must:
   - run **offline** (no network access at all),
   - be **deterministic** (the harness runs it twice and both must pass),
   - finish within **2 minutes** per run.
2. **Tests that actually catch regressions.** The harness will copy your
   repo, revert an earlier feature (the expiry change from ticket 1) and
   run `./test.sh` there — **it must fail**. A suite that stays green with
   a feature missing is decoration, not tests.
3. **Coverage gate: >= 80% of your implementation's lines.**
   - Go projects: the harness measures with
     `go test ./... -coverprofile` + `go tool cover -func` — your tests
     must be Go tests so this works.
   - Other languages: `./test.sh` must write `coverage.txt` containing a
     line `coverage: NN.N%` with your measured line coverage.

## What to test

At minimum, cover the behaviors of the tickets you already shipped: the
protocol basics (handshake, key-value, counters), expiry semantics, pub/sub
delivery, durability/recovery, and the INFO counters. Test through the
wire protocol (raw sockets) like a client would.

## Constraints (unchanged)

Standard library only for the SERVER; your test suite may use any tooling
that is already present in the environment (bash, python3, redis-cli,
go/node), but it must not need network access. Keep tickets 0-5 green.

## When done

Run `./test.sh` yourself and confirm it is green, then confirm the
scratch-revert behavior locally if you can (e.g. copy the repo, undo your
expiry handling, watch the suite fail), then create `/workspace/READY`.
