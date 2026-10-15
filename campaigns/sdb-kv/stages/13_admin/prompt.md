# Ticket 13 — Ops tooling needs a binary admin channel (SDB1 v2)

The store holds the company's data. Ops tooling cannot speak RESP and
wants a compact **binary admin protocol** on its own port: fixed framing,
checksummed payloads, the same keyspace, and live stats. One codebase, two
fronts.

## Deliverable

Serve the **SDB1** binary protocol on port **6395** (alongside RESP on
6379) from the same process and the same `./start.sh`.

## Wire protocol SDB1 (fixed, byte-exact)

Every frame is a 16-byte header followed by N payload bytes. All integers
big-endian.

```
Bytes 0..3:  Magic \x53\x44\x42\x31 ("SDB1")
Bytes 4..5:  uint16 Sequence / Request ID
Byte  6:     uint8 Opcode (request): 0x01 PING, 0x02 ECHO, 0x03 PUT, 0x04 GET, 0x06 STATS
Byte  7:     uint8 Status (response): 0x00 OK, 0x01 ERR_BAD_CHECKSUM, 0x02 ERR_UNKNOWN_OP
             Requests MUST send 0x00 here (reserved); only responses carry status.
Bytes 8..11: uint32 Payload Length N
Bytes 12..15: uint32 IEEE CRC32 of the payload bytes (empty payload CRCs as 0)
Body: N raw bytes (N may be 0).
```

Opcodes (request -> response):

1. **PING** (empty payload): reply Opcode PING, Status OK, empty payload,
   same Sequence.
2. **ECHO** (arbitrary payload): identical payload back, same Sequence.
3. **PUT**: payload = `key || 0x00 || value` (value may contain any
   bytes). Reply empty payload, Status OK.
4. **GET**: payload = `key`. Reply payload = the value, Status OK. A
   missing key is **not** an error reply with a message — it uses
   Status `ERR_UNKNOWN_OP` with an empty payload.
5. **STATS** (empty payload): reply Status OK with a UTF-8 payload of
   `field:value` lines (one per line, like INFO) mirroring your INFO
   counters: `total_connections_received`, `total_commands_processed`,
   `keyspace_hits`, `keyspace_misses`, `expired_keys`,
   `rejected_connections`, `connected_clients`, `uptime_in_seconds`.

Failure semantics: wrong magic closes the connection; a CRC mismatch
answers `ERR_BAD_CHECKSUM` and keeps the connection open; an unknown
opcode answers `ERR_UNKNOWN_OP` and stays open.

## Semantics

- **One store, two protocols**: `PUT` over SDB1 is immediately visible to
  RESP `GET` and vice versa. Same TTLs, same durability guarantees.
- The SDB1 server must handle split/coalesced reads (TCP is a stream) and
  answer 50-frame pipelines correctly, sequence ids preserved.
- Everything earlier stays green (RESP ladder, dashboard, security).

## What the acceptance suite does

Connects over SDB1: bad-magic close; PING/ECHO fidelity (small and 64KiB);
corrupt CRC -> `ERR_BAD_CHECKSUM` with the connection still usable;
unknown opcode -> `ERR_UNKNOWN_OP`; PUT/GET incl. a missing key; SDB1 PUT
readable over RESP and RESP SET readable over SDB1; STATS payload carries
the counter fields; a 50-frame pipeline answers with matched ids.

## Constraints

- Standard library only (raw sockets; no RPC frameworks).
- Ports: RESP stays 6379; the admin channel listens on 6395.
- Everything earlier must stay green.
