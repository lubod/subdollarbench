# Ticket 16 — Larger than memory

The engine answers correct queries on small stores. This ticket makes it
survive a store far larger than its memory.

## This ticket's contract

The harness restarts your server on a fresh scratch data directory under a
**64 MiB memory cap**, then loads about **400 MiB of rows** (400,000 rows
with ~900-byte text cells) and checks exact answers:

1. **Full-table aggregate**: `SELECT COUNT(*), SUM(v), MIN(k), MAX(k)
   FROM big` over all 400,000 rows.
2. **External sort**: `SELECT id FROM big ORDER BY t DESC, id DESC LIMIT
   10`, where `t` is a 900-byte text column with no index. The sort must stay
   within the cap: a 10-row heap for the LIMIT is fine, while buffering the
   whole table or the whole sort dies under the cap.
3. **Indexed join**: after `CREATE INDEX ... ON big (k)`, a join with the
   1,000-row `dim` table, aggregated to one row. A nested loop over
   400,000 × 1,000 rows will not finish inside the read timeout.
4. **Stay alive**: the server must answer every statement and never be
   OOM-killed. Memory use must not grow with the table; the store lives in
   `--data`, and spill files must not corrupt it.

The harness writes the rows through the protocol in batched transactions
and throws the scratch store away afterwards; your real store is untouched.
The cumulative suites still pass; this scenario runs after them.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- The memory cap is a hard cgroup limit, not an advisory number.
- When done: create `/workspace/READY`.
