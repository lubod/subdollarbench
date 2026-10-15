# Ticket 4 — Know your upstreams: health + /status

On-call cannot see what the proxy thinks about its upstreams, and the
proxy only learns an upstream is bad when a client request burns on it.
Give the proxy **active health checking** and a small **status surface**.

## Contract additions

1. **Active health checks**: probe each upstream every ~1–2 seconds
   (a cheap `GET /` or `HEAD /` to `upstreamN:80`). An upstream that fails
   to answer (timeout/refused/bad status) is marked **down**; a healthy
   one is **up**. Detection must not depend on client traffic.
2. **`GET /status`** on the proxy returns **200** with one line per
   upstream, in this exact shape:

   ```
   upstream1: up
   upstream2: down
   upstream3: up
   ```

   The names are `upstream1`..`upstream3` (the fixed upstream order); the
   value is `up` or `down`. Additional lines are allowed.
3. Health flips within **~5 seconds** of the real state changing, in both
   directions. Downstream traffic must honor it: a down upstream receives
   no client requests until it recovers.
4. Everything earlier stays green.

## What the acceptance suite does

The cumulative suite plus a health stage:
`GET /status` must converge to all-up; the harness freezes one upstream
and (with **no** client traffic routed meanwhile) `/status` must report it
`down` within a few seconds; after the thaw `/status` must report it `up`
within 5s and the upstream must rejoin rotation within 10s.
