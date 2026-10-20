# Ticket 20 (lite) — PostgreSQL wire protocol, simple query

The shorter PostgreSQL ticket. Serve **protocol 3.0** on `--pg-port` with
the startup and **simple Query** protocol; extended query is not required.

## This ticket's contract

1. **SSLRequest** refused with `N`.
2. **Startup**: AuthenticationOk (AuthenticationCleartextPassword first
   when `SDB_SQL_PASSWORD` is set, wrong password `28P01`), ParameterStatus
   `server_version` and `client_encoding=UTF8`, BackendKeyData,
   ReadyForQuery `I`.
3. **Simple Query**: `SELECT` answers RowDescription/DataRow/
   CommandComplete; DDL/DML answer CommandComplete; ReadyForQuery tracks
   `I`/`T`/`E`.
4. **SQLSTATE**: `42P01` missing table, `42601` syntax, `23505` duplicate
   key; the connection stays usable.
5. A stock `psql` must run a `SELECT`.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
