# Ticket 20 — PostgreSQL wire protocol

The database speaks its own text protocol; this ticket lets ordinary
PostgreSQL clients use it. Serve **protocol 3.0** on `--pg-port <port>`
in addition to the text port.

## This ticket's contract

1. **SSLRequest** is refused with the single byte `N`.
2. **Startup**: AuthenticationOk; when `SDB_SQL_PASSWORD` is set the server
   first sends AuthenticationCleartextPassword and rejects a wrong password
   with SQLSTATE `28P01`. Then ParameterStatus `server_version` and
   `client_encoding=UTF8`, BackendKeyData, and ReadyForQuery `I`.
3. **Simple Query** (message `Q`): `SELECT` answers RowDescription,
   DataRow and CommandComplete; DDL/DML answer CommandComplete (`CREATE TABLE`,
   `INSERT 0 n`, `UPDATE n`, `DELETE n`, ...); ReadyForQuery carries
   `I`/`T`/`E` across `BEGIN`/`COMMIT` and errors.
4. **Extended Query**: Parse/Bind/Describe/Execute/Sync. A prepared
   `SELECT $1` with one text parameter must return that parameter, and
   `Describe` may answer NoData (the RowDescription may come with Execute).
5. **SQLSTATE**: missing table `42P01`, syntax `42601`, duplicate key
   `23505` (errors still carry ReadyForQuery and leave the connection
   usable).
6. **A stock `psql`** (`psql -h 127.0.0.1 -p <pg-port> -U sdb -d sdb -c
   'SELECT 1'`) must work.

The harness runs the cumulative suites first, then restarts you with
`--pg-port` and a password on a scratch store, acts as a frontend for all
of the above, and runs `psql` from the stock PostgreSQL image.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- The PG port is a second listener; the text protocol keeps working.
- When done: create `/workspace/READY`.
