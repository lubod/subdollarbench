#!/usr/bin/env python3
"""R83 F4/R85 F1: a local logging proxy in front of OpenRouter's API.

Usage: scripts/opencode-proxy.py <port> <log.jsonl> [upstream] [options]

Options:
  --bind=ADDR   bind address (default 127.0.0.1; the cell path uses
                0.0.0.0 so the run's container reaches it)
  --billed      log only {id, model, cost, status} per chat call (the
                cell's billed.jsonl); the default logs the full record
                (the model check's proxy.jsonl)

The model check points opencode's `providers.openrouter.settings.baseURL`
at `http://127.0.0.1:<port>/api/v1`; the runner points it at the cell
proxy. This proxy forwards each request to the upstream (default
`https://openrouter.ai`) and appends one JSON line per chat call with
the generation id, the model, the response's usage and the provider's
cost. The client's Authorization header is forwarded, never logged, and
the billed log never holds the prompt. The body is buffered (the SSE
stream is relayed whole), so the usage is available.
"""
import http.client
import http.server
import json
import sys
import urllib.parse

PORT = int(sys.argv[1])
LOG = sys.argv[2]
UPSTREAM = "https://openrouter.ai"
BIND = "127.0.0.1"
BILLED = False
for arg in sys.argv[3:]:
    if arg == "--billed":
        BILLED = True
    elif arg.startswith("--bind="):
        BIND = arg.split("=", 1)[1]
    elif not arg.startswith("--"):
        UPSTREAM = arg
UP = urllib.parse.urlsplit(UPSTREAM)
if UP.scheme not in ("http", "https") or not UP.hostname:
    raise SystemExit("bad upstream: %s" % UPSTREAM)


def call_record(path, body, status, request_model=None):
    record = {"path": path, "id": None, "model": None, "usage": None,
              "cost": None, "status": status}
    text = body.decode("utf-8", "replace")
    values = []
    for line in text.splitlines():
        if not line.startswith("data: "):
            continue
        chunk = line[6:].strip()
        if chunk == "[DONE]":
            continue
        try:
            values.append(json.loads(chunk))
        except Exception:
            continue
    if not values:
        # A non-streaming answer (a title or summary call) is one JSON
        # object, not SSE chunks.
        try:
            one = json.loads(text)
        except Exception:
            one = None
        if isinstance(one, dict):
            values.append(one)
    for v in values:
        record["id"] = record["id"] or v.get("id")
        record["model"] = record["model"] or v.get("model")
        if isinstance(v.get("usage"), dict):
            record["usage"] = v["usage"]
            if isinstance(v["usage"].get("cost"), (int, float)):
                record["cost"] = v["usage"]["cost"]
    # The stub (and some non-streaming answers) carry no model; the
    # request body does.
    record["model"] = record["model"] or request_model
    return record


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _forward(self, method):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else None
        request_model = None
        if method == "POST" and body:
            try:
                request_model = json.loads(
                    body.decode("utf-8", "replace")).get("model")
            except Exception:
                request_model = None
        headers = {
            k: v for k, v in self.headers.items()
            if k.lower() in ("authorization", "content-type", "accept")
        }
        if UP.scheme == "https":
            conn = http.client.HTTPSConnection(
                UP.hostname, UP.port or 443, timeout=180)
        else:
            conn = http.client.HTTPConnection(
                UP.hostname, UP.port or 80, timeout=180)
        path = (UP.path.rstrip("/") + self.path) if UP.path else self.path
        try:
            conn.request(method, path, body, headers)
            resp = conn.getresponse()
            data = resp.read()
            status = resp.status
            ctype = resp.getheader("Content-Type") or "application/json"
        except Exception as exc:  # a proxy failure is the check's failure
            self.send_response(502)
            msg = ("proxy error: %s" % exc).encode()
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(msg)))
            self.end_headers()
            self.wfile.write(msg)
            return
        finally:
            conn.close()
        # Log before the response: a client reset must not lose the call.
        if method == "POST":
            record = call_record(self.path, data, status, request_model)
            if BILLED:
                record = {k: record[k]
                          for k in ("id", "model", "cost", "status")}
            with open(LOG, "a") as f:
                f.write(json.dumps(record) + "\n")
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        self._forward("POST")

    def do_GET(self):
        self._forward("GET")

    def log_message(self, *args):
        pass


class Server(http.server.ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        exc = sys.exc_info()[1]
        if isinstance(exc, (ConnectionResetError, BrokenPipeError)):
            return
        super().handle_error(request, client_address)


Server((BIND, PORT), Handler).serve_forever()
