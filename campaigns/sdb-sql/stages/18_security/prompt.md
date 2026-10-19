# Ticket 18 — Hostile input and limits

The server is correct; this ticket makes it hard to knock over. The harness
restarts you on a fresh scratch store with `SDB_SQL_PASSWORD` set.

## This ticket's contract

1. **AUTH**: every connection must begin with `AUTH <password>`. Without it,
   or with the wrong password, the server answers `ERR auth` (and stays
   sane). The password must never appear in an error message or a log.
2. **Fuzzing**: grammar mutations and raw-byte fuzzing never crash the
   server; after any hostile line, a valid query still answers.
3. **Nesting**: an expression nested **10,000** deep answers `ERR limit`,
   not a stack overflow.
4. **Size**: a statement over **16 MiB** answers `ERR limit`; the connection
   must recover for the next statement.
5. **Cancellation**: a statement that runs for more than **5 s** is
   cancelled and answers `ERR limit`; the connection stays usable.
6. **Slow-loris**: a client that sends a partial line must not starve other
   connections.
7. **Connection cap**: at most **256** connections may be open at once; the
   next is refused cleanly (an `ERR limit` line or a clean close). Closed
   connections release their slot.

The harness authenticates, fuzzes, sends the deep/large/slow statements,
holds a partial-line client, then opens 256 connections. The cumulative
suites still pass; this scenario runs on a scratch store, so your real
store is untouched.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- Limits are wire behavior: `ERR auth` and `ERR limit`, not a crash.
- When done: create `/workspace/READY`.
