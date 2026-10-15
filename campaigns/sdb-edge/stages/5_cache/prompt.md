# Ticket 5 — Cache with a TTL

The upstreams are mostly static and the fleet is paying for the same bytes
over and over. Add a small **response cache** in front of the pool,
configurable, with honest cache headers.

## Contract additions

1. **Configuration**: the environment variable **`SDB_PROXY_CACHE_TTL`**
   (integer seconds) sets the cache window. `0` or unset means **caching
   off** (the default; every earlier ticket's behavior is unchanged).
2. **Behavior with a TTL set**: a successful `GET` response is cached for
   that many seconds. A request served fresh from an upstream is marked
   **`X-Cache: MISS`**; a request served from the cache is marked
   **`X-Cache: HIT`** and carries an **`Age: <seconds>`** header (integer,
   ≥ 0). Cached and fresh bodies must be **byte-identical** for the same
   entry.
3. **Expiry**: after the TTL elapses the next request for that entry goes
   upstream again (`MISS`). Never serve stale bytes indefinitely.
4. Never cache: non-`GET` methods, non-200 responses, or responses whose
   upstream headers say `Cache-Control: no-store`.
5. Everything earlier stays green — the acceptance suite runs tickets 0–4
   with caching **off** and only then enables it.

## What the acceptance suite does

The cumulative suite, then a caching stage with the proxy restarted under
`SDB_PROXY_CACHE_TTL=3`: repeated requests must show at least one
`X-Cache: HIT` with an `Age` header and a body byte-identical to its
`MISS` twin; after the 3s window a request must `MISS` again.

## Constraints

- Standard library only; the cache may be in memory.
- A cached hit must not touch an upstream.
