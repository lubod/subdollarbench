# Ticket 5 — Observability

Durability shipped (ticket 3). Ops now needs to see what the server is
doing: dashboards scrape metrics, and the on-call rotation needs error
signal. Everything earlier must stay green — the acceptance suite re-runs
every previous ticket, including the crash-recovery kill.

## Deliverable

An `INFO` command on the existing server, same port, same RESP2 protocol.

## Contract (the acceptance suite is strict about names)

`INFO` returns a **bulk string** whose body is one `field:value` line per
metric (line endings `\n` or `\r\n`; order irrelevant; extra fields are
fine; `# section` header lines are ignored).

These fields MUST exist and be **integers**:

| Field | Meaning |
|---|---|
| `total_connections_received` | connections accepted since start |
| `total_commands_processed` | commands executed since start |
| `keyspace_hits` | lookups that found a key (e.g. `GET` on an existing key) |
| `keyspace_misses` | lookups that did not find a key |
| `expired_keys` | keys removed because their TTL passed (lazy counting is fine) |
| `rejected_connections` | connections refused (`0` while no cap is hit) |
| `connected_clients` | currently open client connections |
| `uptime_in_seconds` | seconds since the server started |

Semantics: all counters are **cumulative since start** and **monotonic**
(never decrease). Deltas must track real traffic — a static or hardcoded
value will fail the acceptance check, which drives a randomized number of
operations and compares before/after readings.

`INFO <section>` (e.g. `INFO server`) may return a subset; bare `INFO`
must return everything above.

## Constraints (unchanged)

Standard library only; raw TCP sockets; neutrally named binary; tickets
0-3 semantics unchanged, including expiry and AOF crash recovery.

## When done

Test with `bash`: set and get a few keys, check `INFO` before/after and
confirm the counters moved; let a key expire and confirm `expired_keys`
grew. Keep earlier tickets green. Then create `/workspace/READY`.
