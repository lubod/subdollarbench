# Ticket 2 — Files: upload and retrieve

The store becomes real: clients upload and download files.

## Contract additions

1. **`POST /files/{filename}`** -> **`201 Created`** with the request body
   stored under `./data/files/{filename}` (create the directory on
   demand). Filenames are `[A-Za-z0-9._-]+`; anything else is a 400.
2. **`GET /files/{filename}`** -> `200 OK` with the stored bytes exactly,
   a correct `Content-Length`, and `Connection` semantics intact. An
   unknown file is `404`.
3. Re-uploading the same name replaces the content.
4. Everything earlier stays green.

## What the acceptance suite does

Uploads a seeded file, downloads it and compares byte-for-byte (and the
`Content-Length`), checks an unknown file 404s, and re-runs the earlier
checks.
