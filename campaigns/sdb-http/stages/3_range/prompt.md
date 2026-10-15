# Ticket 3 — Byte ranges

Clients resume downloads and seek media: support HTTP byte ranges on the
stored files.

## Contract additions

1. **`Range: bytes=a-b`** -> `206 Partial Content` with the exact slice, a
   `Content-Range: bytes a-b/<len>` header, and the slice's
   `Content-Length`.
2. **Suffix ranges** (`bytes=-N`, the last N bytes) and **open ranges**
   (`bytes=a-`) work the same way.
3. An **unsatisfiable** range (start beyond the file) -> `416 Range Not
   Satisfiable` (a `Content-Range: bytes */<len>` header is good practice).
4. Without a `Range` header nothing changes: `200 OK` and the full file.
5. Everything earlier stays green.

## What the acceptance suite does

Uploads a known 64-byte file and checks `bytes=2-5`, `bytes=-4`,
`bytes=60-` (all 206 with correct `Content-Range` and bytes), an
unsatisfiable range (416), and a plain GET (200, full body).
