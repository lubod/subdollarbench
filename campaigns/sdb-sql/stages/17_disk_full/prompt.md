# Ticket 17 — Incident: disk full

Your data directory (`--data`) is on a small volume. The harness fills it
while the server is running; the store must survive the incident.

## This ticket's contract

The harness restarts you on a fresh **16 MiB tmpfs** at `--data` (a foreign
filler file may appear in the directory; your store must ignore files that
are not its own), seeds rows, then fills the volume. With the volume full:

1. **Reads keep serving**: every acked seed row still reads back exactly.
2. **Writes fail cleanly**: a write larger than the free space answers
   `ERR full`, is never acked, and is not partially applied (the row count
   is unchanged). The connection stays usable — no hang, no crash, no
   silent success.
3. **No corruption**: the seed rows are byte-for-byte intact.
4. **Recovery**: the harness removes the filler file. The same write then
   succeeds **without a restart**, and the new row reads back exactly.

The cumulative suites still pass; this scenario runs after them on a fresh
scratch tmpfs. Your real store is untouched.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- Errors are wire classes: the ENOSPC write must be `ERR full`.
- When done: create `/workspace/READY`.
