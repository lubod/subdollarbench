# Ticket 1 — Echo and header handling

The server routes (ticket 0). Add the small dynamic endpoints every
service needs.

## Contract additions

1. **`GET /echo/{str}`** -> `200 OK` with exactly `{str}` as the body
   (URL-decoded where applicable, `Content-Length` correct).
2. **`GET /user-agent`** -> `200 OK` with the value of the request's
   `User-Agent` header as the body (empty when absent is acceptable, but
   the status must be 200).
3. Everything earlier stays green (root 200, 404, keep-alive, 400 on
   malformed input).

## What the acceptance suite does

Echo round-trips a seeded token byte-exactly, the user-agent endpoint
returns the sent header, and the ticket-0 checks still pass.
