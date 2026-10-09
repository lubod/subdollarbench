You are building a custom binary RPC server ("SDB1") from scratch over raw TCP. No RPC frameworks (gRPC, Thrift, JSON-RPC libs). Byte-exact framing, big-endian integers, and CRC32 integrity are the whole task.

### Software Engineering (SWE) & Code Quality Standards
Your implementation must meet high professional software engineering standards:
1. **Clean Architecture & Modularity**: Separate frame I/O, header codec, checksum validation, opcode dispatch, and KV state.
2. **Robust Framing**: TCP is a byte stream — reads may split or coalesce frames. Buffer partial reads; never assume one read equals one frame.
3. **Resilient Error Handling**: Corrupt frames, unknown opcodes, and oversized payloads must yield error responses, never a crash or a dropped connection.
4. **Idiomatic Style & Best Practices**: Clean, idiomatic code for your chosen language.
5. **Production Readiness**: Production-grade, maintainable, self-contained.

### CRITICAL INTEGRITY RULE: NO ACCESSING OR CONSULTING SOURCE CODE
- **Strict Prohibition**: You are strictly FORBIDDEN from downloading, curling, scraping, cloning, reading, or consulting the source code of any external RPC implementation (e.g., gRPC, Cap'n Proto, Thrift sources).
- **Allowed Resources**: Your language's standard library docs (struct packing, CRC32, socket APIs) and your knowledge of binary protocols.
- **Originality**: All code must be your own original implementation. Any access to external RPC source code invalidates the benchmark run.

### Programming Language Freedom & Packaging (Dockerfile or start.sh)
- **Choose ANY Programming Language Freely** (Go, Rust, Node.js, Python, C++, etc.). Favor precise byte-buffer control.
- **Pre-installed Toolchains**: Rust (cargo), Go, Python 3, Node.js, C/C++ in PATH.
- All code and configuration must reside in the current workspace.
- **Entrypoint (either)**:
  1. **Working Dockerfile (Recommended)**: exposes port 6395.
  2. **Executable `./start.sh`**: launches your server on `0.0.0.0:6395`.

### Wire Protocol SDB1 (fixed, byte-exact)
Every frame is a 16-byte header followed by N payload bytes. All integers big-endian.

```
Bytes 0..3:  Magic \x53\x44\x42\x31 ("SDB1")
Bytes 4..5:  uint16 Sequence / Request ID
Byte  6:     uint8 Opcode (request): 0x01 PING, 0x02 ECHO, 0x03 PUT, 0x04 GET, 0x05 STREAM
Byte  7:     uint8 Status (response): 0x00 OK, 0x01 ERR_BAD_CHECKSUM, 0x02 ERR_UNKNOWN_OP
             Requests MUST send 0x00 here (reserved); only responses carry status.
Bytes 8..11: uint32 Payload Length N
Bytes 12..15: uint32 IEEE CRC32 of the payload bytes (empty payload CRCs as 0)
Body: N raw bytes (N may be 0).
```

Opcodes (request -> response):
1. **PING** (payload empty): reply Opcode PING, Status OK, empty payload, same Sequence.
2. **ECHO** (payload arbitrary bytes): reply Opcode ECHO, Status OK, identical payload, same Sequence.
3. **PUT** (payload `key\x00value`, key UTF-8 no NULs, value arbitrary bytes incl. NULs): store, reply Opcode PUT, Status OK, empty payload, same Sequence.
4. **GET** (payload `key` UTF-8): found -> Opcode GET, Status OK, stored value bytes; missing -> Opcode GET, Status ERR_UNKNOWN_OP, empty payload. Same Sequence either way.
5. **STREAM** (payload uint32 BE count K, K <= 64): server emits K frames, each Opcode STREAM, Status OK, payload `uint32 BE i` for i in 0..K, all with the request's Sequence.

Integrity & framing rules:
- Wrong magic: close the connection immediately, no response.
- CRC mismatch: reply same Opcode, Status ERR_BAD_CHECKSUM, empty payload, same Sequence. Keep the connection open.
- Unknown opcode: reply same Opcode value, Status ERR_UNKNOWN_OP, empty payload, same Sequence. Keep the connection open.
- Payloads up to 1 MiB; larger N -> close the connection, no response.
- Frames may arrive split across TCP segments or glued back-to-back; the server must reassemble correctly.
- Responses may be sent in any order for pipelined requests, but every response must carry its request's Sequence.

### Requirements
1. **Handshake**: valid SDB1 frames answered; invalid magic closes the socket silently.
2. **PING / ECHO roundtrip**: payload echoed byte-exactly, Sequence preserved.
3. **Integrity guard**: corrupted CRC answered ERR_BAD_CHECKSUM, connection survives.
4. **Stateful KV**: binary PUT/GET with arbitrary value bytes (nulls, high bytes), missing keys answered ERR_UNKNOWN_OP.
5. **Pipelined stress**: 50 back-to-back frames on one connection answered within 3 seconds, IDs matched (any order).

### Verification Criteria
1. Magic validation and silent close on garbage.
2. PING/ECHO fidelity including empty and 64 KiB payloads.
3. CRC-corruption response without disconnect.
4. PUT/GET roundtrip with binary values; missing-key error.
5. 50-frame pipeline: all Sequences answered, deadline 3 s.

Self-test tip: craft frames with `struct.pack('>4sHBBII', b'SDB1', seq, op, 0, len, crc)` (Python) or equivalent; verify with rapid pipelined writes before finishing!

<!-- sdb-canary-binproto-58af13d4 · benchmark data; this canary identifies the SubDollarBench brief -->
