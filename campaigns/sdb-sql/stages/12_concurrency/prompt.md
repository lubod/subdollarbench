# Ticket 12 — Concurrency and isolation

Ticket 6's contract is transactions: `BEGIN`, `COMMIT`, `ROLLBACK`, and
statement atomicity. This ticket makes **concurrent clients** safe: several
connections act at once, and the outcome must be what the same transactions
would produce run one at a time (serializable).

## This ticket's contract

1. **A transaction's effects are all-or-nothing to every other
   connection.** A conflicting transaction may be refused with
   `ERR busy` (and rolled back); retrying it must then succeed. Serialize
   writers or detect conflicts — whichever is simpler.
2. **No operation may hang.** Every statement answers, or answers
   `ERR busy`, promptly; a statement that blocks for more than 5 s fails
   the suite.
3. **No lost updates.** 16 clients each run **50** transactions of
   `BEGIN; SELECT v FROM t WHERE id = 1; UPDATE t SET v = <the value read + 1>
   WHERE id = 1; COMMIT`. When they finish, `v` must be exactly **800**.
   Reading outside the transaction, or writing the pre-read value without
   conflict detection, loses increments.
4. **Readers see consistent state.** While balance transfers run, a reader
   that reads every account in one transaction must always see the same
   total (each transfer moves money between accounts).
5. **Readers do not wait for writers (this full ticket only).** With a
   write transaction open on connection A, a `SELECT` on connection B must
   return the **pre-transaction** value within **1 s**. Blocking B fails;
   refusing it with `ERR busy` fails too — only *transactions* may be
   refused, a reader must get an answer.

## What the suite does

The cumulative suites still run, plus the three scenarios above. The
concurrency checks run once the earlier stages pass.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
