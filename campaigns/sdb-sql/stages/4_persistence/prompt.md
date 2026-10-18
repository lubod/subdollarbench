# Ticket 4 — Persistence

Everything your server has written so far lives only as long as the process
(tickets 0-3). This ticket makes it survive a restart. The harness stops the
server **cleanly** (`SIGTERM`), starts it again on the same directory, and
every row written before the stop must still answer exactly the same.

## Launch contract (unchanged, but now it matters)

- `./start.sh --port <p> --data <dir>`; defaults port `5480` and `./data`.
- **`--data` is your store.** The harness owns that directory: it hands you
  the *same* path across restarts and across tickets, and it may start you on
  a directory that a previous ticket already wrote. Keep all state there;
  never wipe it on startup, and never keep the only copy in memory or in a
  scratch directory the harness does not own.
- A clean stop is `SIGTERM` (delivered to every process in the
  container, so a non-`exec` `start.sh` still reaches the server); you
  have a few seconds to flush. (Ticket 5 adds
  `SIGKILL`, torn writes and double restarts.)

## What the acceptance suite does

The differential suite of the earlier tickets that passed still runs (cumulative), plus:

1. **Persistence**: the harness writes a seeded dataset, stops the server,
   starts it again on the same `--data`, and re-reads the data. Answers are
   compared byte for byte, so a format that loses rows, changes order or
   mangles values fails.
2. **Upgrade check (starts here)**: from this ticket on, the data your server
   writes is re-checked by every later ticket's verification. A build that
   cannot read its own earlier store is a regression of *this* ticket.

A failing verification comes with a minimal statement list: run it from an
empty database and compare your answers with the expected ones.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries (the
  scanner checks every file, tests included).
- `SPEC.md` in the workspace holds the whole contract so far.
- When done: create `/workspace/READY`.
