# Ticket 1 — Expiry

Our key-value server works (ticket 0). Product now needs keys that
disappear on their own: sessions, caches, and rate-limit counters all
depend on expiry whiteout. Keep everything from ticket 0 green — the
acceptance suite re-runs it.

## Deliverable

Expiry support in the existing server, same port, same protocol.

## Required commands and options

| Command | Behavior |
|---|---|
| `EXPIRE k <sec>` | `:1` if the timeout was set, `:0` if the key does not exist |
| `PEXPIRE k <ms>` | same, millisecond precision |
| `EXPIREAT k <unix-sec>` | absolute time; a past time deletes the key immediately |
| `PEXPIREAT k <unix-ms>` | same, milliseconds |
| `TTL k` | seconds remaining (`:N`), `-1` no TTL, `-2` missing key |
| `PTTL k` | milliseconds remaining; same `-1` / `-2` convention |
| `PERSIST k` | remove the TTL: `:1` removed, `:0` nothing to remove |
| `SET k v EX <sec> [PX <ms>]` | set with a relative TTL |
| `SETEX k <sec> v` | set with TTL (atomic) |
| `GETEX k [EX <sec> \| PX <ms> \| PERSIST]` | read, optionally changing the TTL; no option must not touch the TTL |

## Semantics (the acceptance suite is strict here)

- Expired keys behave as missing: `GET` -> `$-1`, `EXISTS` -> `:0`.
- `SET k v` without options CLEARS any existing TTL.
- `INCR`/`DECR`/`APPEND`-style in-place updates PRESERVE the TTL.
- `EXPIRE k 0` (or a past `EXPIREAT`) deletes the key immediately and
  replies `:1` if the key existed.
- TTL/PTTL round to what a client would observe: never report a key as
  alive after its expiry moment when the next command touches it (lazy
  expiry is acceptable; active sweeping is optional).
- A short TTL (100-300ms) must be honored within a small tolerance:
  immediate checks see the key, checks after the TTL do not.

## Constraints (unchanged)

Standard library only; raw TCP sockets; neutrally named binary; existing
commands keep their exact RESP behavior. No background thread is required
— but if you use one, it must not race with client commands.

## When done

Test with `bash` (set a key with a 150ms TTL, check `TTL`, wait, check
`GET`), keep the ticket 0 checks green, then create `/workspace/READY`.
Regressions in earlier behavior fail this ticket.
