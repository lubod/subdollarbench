# Ticket 6 — Chunked uploads and size limits

Upload clients vary; the server must handle their shapes without
unbounded memory.

## Contract additions

1. **`Transfer-Encoding: chunked`** request bodies are decoded and stored
   byte-exact (no `Content-Length` present).
2. **`Expect: 100-continue`**: a client that sends the header must not
   deadlock — answer with an interim `100 Continue` (or the final status)
   promptly, then complete the upload.
3. **Size limits**: declare a request-body cap of **16 MiB** and reject a
   declared body above it with **`413 Payload Too Large`** *before*
   reading the bytes. The harness runs your server under a **128MB memory
   limit** during this ticket — a server that buffers the declared body
   dies; a server that refuses it at the head survives.
4. Everything earlier stays green.

## What the acceptance suite does

Restarts the server under the 128MB cap and, against the cumulative
suite plus this stage: uploads via chunked encoding and compares the
stored bytes; runs a `100-continue` upload; and declares a 128MiB body
expecting a `413` (or 4xx/close) without the bytes being sent.
