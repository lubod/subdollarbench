# Ticket 4 — Conditional GET (validators)

Caches and clients must be able to skip downloads they already have.

## Contract additions

1. **`GET /files/{name}`** carries an **`ETag`** and a **`Last-Modified`**
   validator. The ETag must change when the content changes (stale
   validators must never 304).
2. **`If-None-Match: <etag>`** with the current validator -> `304 Not
   Modified`, no body.
3. **`If-Modified-Since: <date>`** at or after the file's Last-Modified ->
   `304 Not Modified`.
4. Non-matching validators -> the normal `200 OK` with the full body.
5. Everything earlier stays green.

## What the acceptance suite does

Uploads a file, captures its ETag/Last-Modified, verifies 304 on matching
validators (and no body), 200 on a stale ETag, re-uploads changed content
and requires a *different* ETag with the new bytes served, and that the
old validator no longer 304s.
