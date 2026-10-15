# Ticket 5 — Compression negotiation

Bandwidth is the bottleneck for the big files: negotiate gzip.

## Contract additions

1. **`Accept-Encoding: gzip`** on a file that compresses -> the response
   carries **`Content-Encoding: gzip`** and a valid gzip body that decodes
   to exactly the stored bytes. Do not gzip responses that would grow
   (tiny or incompressible bodies may stay identity — but the suite tests
   a 4KB compressible file, which must be compressed).
2. **`Accept-Encoding: identity`** (or absent) -> the raw bytes with no
   `Content-Encoding`.
3. Everything earlier stays green — ranges and conditionals keep working
   for identity responses.
4. Compressing on the fly must stay memory-bounded (streaming is fine).

## What the acceptance suite does

Uploads a highly compressible ~5KB file; with `Accept-Encoding: gzip` it
requires `Content-Encoding: gzip`, a smaller body that decodes to the
exact file; with `identity` it requires the raw bytes and no
`Content-Encoding`.
