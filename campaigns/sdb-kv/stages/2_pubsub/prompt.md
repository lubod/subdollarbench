# Ticket 2 — Pub/Sub

Expiry shipped (ticket 1). Product now needs live updates: clients must
be able to subscribe to channels and receive messages published by other
connections, in order, without polling. Everything earlier must keep
working — the acceptance suite re-runs tickets 0-1.

## Deliverable

Pub/sub support in the existing server, same port, same RESP2 protocol.

## Required commands

| Command | Behavior |
|---|---|
| `SUBSCRIBE <channel> [<channel> ...]` | subscribe the connection to each channel; reply with one subscription confirmation per channel |
| `UNSUBSCRIBE [<channel> ...]` | leave channels (all when none given); reply with one confirmation per channel |
| `PUBLISH <channel> <message>` | deliver to every subscriber of that channel; reply `:N` = number of receivers |

## Semantics (the acceptance suite is strict here)

- Once a connection is subscribed, its stream carries subscription
  confirmations and delivered messages as RESP arrays; it may still issue
  `SUBSCRIBE`/`UNSUBSCRIBE`/`PING` on that connection.
- Delivery order per channel is the publish order; a subscriber must
  receive every message published while it was subscribed.
- After `UNSUBSCRIBE`, no further messages for that channel arrive on the
  connection.
- `PUBLISH` to a channel with no subscribers replies `:0`.
- A subscription confirmation names the channel and the connection's
  current subscription count. The subscription count rule (topics vs
  patterns) is yours to define consistently; a raw channel count is fine.
- Publishing from one connection to a subscriber on another connection
  must not deadlock either connection.

## Constraints (unchanged)

Standard library only; raw TCP sockets; neutrally named binary; existing
commands keep their exact RESP behavior, including expiry semantics.
Subscription state is per-connection; connections are independent.

## When done

Test with `bash` or a small script: subscribe on one connection, publish
on another, read the delivered frame, unsubscribe, publish again and
confirm silence. Keep tickets 0-1 green. Then create `/workspace/READY`.
