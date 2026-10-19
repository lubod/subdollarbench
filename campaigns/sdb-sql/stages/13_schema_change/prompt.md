# Ticket 13 — Schema change

The schema is fixed at `CREATE TABLE` time. This ticket makes it **change**:
columns and tables can be added, dropped and renamed **after data exists**,
and every change survives a restart.

## This ticket's contract

1. **`ALTER TABLE t ADD [COLUMN] name TYPE [DEFAULT <literal>]`**
   - Every existing row reads the new column as `NULL`, unless it has a
     `DEFAULT`, in which case existing rows read that default.
   - Without `IF NOT EXISTS`, a name that is taken is `ERR exists`; with
     `ADD COLUMN IF NOT EXISTS`, it is a no-op that answers `OK 0`.
   - An unknown table is `ERR no_table`.
2. **`ALTER TABLE t DROP [COLUMN] name`**
   - The column disappears from `SELECT *` and from explicit references.
   - Dropping a `PRIMARY KEY`, `UNIQUE` or indexed column is
     `ERR constraint`. Dropping a missing column is `ERR no_column`; with
     `DROP COLUMN IF EXISTS` it is a no-op `OK 0`.
3. **`ALTER TABLE t RENAME COLUMN old TO new`** — indexes and foreign keys
   follow the new name. A taken name is `ERR exists`, an unknown column
   `ERR no_column`, an unknown table `ERR no_table`.
4. **`ALTER TABLE t RENAME TO new`** — a taken table (or index) name is
   `ERR exists`, an unknown table `ERR no_table`.
5. **Every change survives a restart** on the same store, and an index
   stays valid: its answers are unchanged by any schema change.
6. `IF [NOT] EXISTS` follows the same rule for both forms: the condition is
   checked against the current schema, not the statement's own text.

## What the suite does

The cumulative suites still pass, and the differential generator
interleaves the schema changes above with DML: rows written before a change
must read correctly after it, and every answer must match the reference's.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
