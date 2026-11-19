"""Allowlist egress proxy (stdlib only): HTTP GET + CONNECT tunneling.
Usage: python3 egress_proxy.py <port>
Env ALLOWLIST (comma hosts); the default is the .47 model API only
(R81 F2: no registries, no GitHub).
NO_PROXY semantics: client-side (agent env); this proxy 403s anything off-list.
"""
import http.client
import json
import os
import socket
import threading
import urllib.parse
import uuid

PORT = int(__import__("sys").argv[1]) if len(__import__("sys").argv) > 1 else 3128
ALLOW = set(
    os.environ
    .get("ALLOWLIST", "openrouter.ai,api.openrouter.ai")
    .split(",")
)
# R85 F1: the run's billing proxy lives in this sidecar. The agent's
# baseURL is http://egress-proxy:<port>/api/v1; a request on that path
# (origin-form or absolute-form) is forwarded to the upstream over TLS
# and one {id, model, cost, status} line is written to BILL_LOG. Only
# the runner's sidecar sets BILL_LOG; the egress-only sidecar keeps the
# plain allowlist proxy behaviour.
BILL_LOG = os.environ.get("BILL_LOG", "")
# R90 M4: one marker file per in-flight call beside the log. The runner
# waits for this directory to empty before it reads the bill; a marker
# that survives a sidecar crash names the call whose record was lost.
BILL_OPEN = (BILL_LOG + ".open") if BILL_LOG else ""
if BILL_OPEN:
    os.makedirs(BILL_OPEN, exist_ok=True)
BILL_UPSTREAM = os.environ.get("BILL_UPSTREAM", "https://openrouter.ai")
UP = urllib.parse.urlsplit(BILL_UPSTREAM)


def billing_record(body, status, request_model):
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
        try:
            one = json.loads(text)
        except Exception:
            one = None
        if isinstance(one, dict):
            values.append(one)
    record = {"id": None, "model": None, "cost": None, "status": status}
    for v in values:
        record["id"] = record["id"] or v.get("id")
        record["model"] = record["model"] or v.get("model")
        usage = v.get("usage")
        if isinstance(usage, dict) and isinstance(usage.get("cost"), (int, float)):
            record["cost"] = usage["cost"]
    record["model"] = record["model"] or request_model
    return record


def billing_path(target):
    """The /api/v1 path of an origin- or absolute-form request."""
    if target.startswith("http://") or target.startswith("https://"):
        return urllib.parse.urlsplit(target).path
    return target


def handle_billing(client, head, method, path):
    # `head` may already hold the first body bytes (the recv read past
    # the blank line); split them out before reading the rest.
    parts = head.split(b"\r\n\r\n", 1)
    head = parts[0] + b"\r\n\r\n"
    body = parts[1] if len(parts) > 1 else b""
    lines = parts[0].decode("latin1").split("\r\n")
    headers = {}
    for ln in lines[1:]:
        if not ln:
            break
        k, _, v = ln.partition(":")
        headers[k.strip().lower()] = v.strip()
    try:
        clen = int(headers.get("content-length") or 0)
    except ValueError:
        clen = 0
    while len(body) < clen:
        chunk = client.recv(min(65536, clen - len(body)))
        if not chunk:
            break
        body += chunk
    request_model = None
    if method == "POST" and body:
        try:
            request_model = json.loads(
                body.decode("utf-8", "replace")).get("model")
        except Exception:
            request_model = None
    marker = ""
    if BILL_OPEN:
        marker = os.path.join(BILL_OPEN, "%d-%s" % (os.getpid(), uuid.uuid4().hex))
        try:
            with open(marker, "w") as mf:
                mf.write("%s %s\n" % (method, path))
        except OSError:
            marker = ""
    fwd = {k: headers[k] for k in ("authorization", "content-type", "accept")
           if k in headers}
    fwd_path = (UP.path.rstrip("/") + path) if UP.path else path
    if UP.scheme == "https":
        conn = http.client.HTTPSConnection(
            UP.hostname, UP.port or 443, timeout=180)
    else:
        conn = http.client.HTTPConnection(
            UP.hostname, UP.port or 80, timeout=180)
    resp_body = bytearray()
    status = 502
    reason = "Bad Gateway"
    ctype = "text/plain"
    headers_sent = False
    client_ok = True

    def send_client(data):
        nonlocal client_ok
        if not client_ok:
            return
        try:
            client.sendall(data)
        except OSError:
            # R87 F1: a client that hangs up mid-stream must not stop the
            # upstream read; the bill is still owed.
            client_ok = False

    try:
        conn.request(method, fwd_path, body, fwd)
        resp = conn.getresponse()
        status = resp.status
        reason = resp.reason or "OK"
        ctype = resp.getheader("Content-Type") or "application/json"
        cenc = resp.getheader("Content-Encoding")
        # R87 F1: forward the status line and headers at once, then each
        # body chunk as it arrives (SSE or chunked or close-delimited),
        # never buffering the whole answer.
        head = "HTTP/1.1 %d %s\r\nContent-Type: %s\r\n" % (
            status, reason, ctype)
        if cenc:
            head += "Content-Encoding: %s\r\n" % cenc
        head += "Connection: close\r\n\r\n"
        send_client(head.encode("latin1"))
        headers_sent = True
        while True:
            try:
                # `read1` returns as soon as some bytes are available; a
                # plain `read(n)` would wait for n bytes (or EOF) and
                # buffer the stream again.
                chunk = resp.read1(65536)
            except Exception:
                # The 180 s socket timeout is a per-read guard; a timeout
                # ends the stream (the log keeps what arrived).
                break
            if not chunk:
                break
            resp_body += chunk
            send_client(chunk)
    except Exception as exc:
        if not headers_sent:
            status = 502
            reason = "Bad Gateway"
            ctype = "text/plain"
            resp_body += ("billing proxy error: %s" % exc).encode()
            send_client(
                ("HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\n"
                 "Connection: close\r\n\r\n").encode("latin1")
                + bytes(resp_body))
    finally:
        conn.close()
    if method == "POST":
        try:
            with open(BILL_LOG, "a") as f:
                f.write(json.dumps(
                    billing_record(bytes(resp_body), status, request_model))
                    + "\n")
        except OSError:
            pass
    if marker:
        try:
            os.remove(marker)
        except OSError:
            pass


def allowed(host):
    h = host.lower().split(":")[0]
    return any(h == a or h.endswith("." + a) for a in ALLOW)


def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d:
                break
            b.sendall(d)
    except OSError:
        pass
    finally:
        for s in (a, b):
            try:
                s.shutdown(socket.SHUT_WR)
            except OSError:
                pass


def handle(client):
    try:
        # R93 G4: a client that connects and sends nothing must not pin a
        # handler thread forever.
        client.settimeout(60)
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = client.recv(4096)
            if not chunk:
                client.close()
                return
            head += chunk
        lines = head.decode("latin1").split("\r\n")
        method, target = lines[0].split()[:2]
        host = ""
        for ln in lines[1:]:
            if ln.lower().startswith("host:"):
                host = ln[5:].strip()
                break
        if method.upper() == "CONNECT":
            host = target
        # R85 F1: the billing path, before the allowlist (the sidecar's
        # own host is not on it).
        if BILL_LOG and host.split(":")[0] in (
                "egress-proxy", "localhost", "127.0.0.1"):
            path = billing_path(target)
            if path.startswith("/api/v1/"):
                handle_billing(client, head, method, path)
                return
        if not host or not allowed(host):
            client.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\n\r\nFORBIDDEN")
            client.close()
            return
        if method.upper() == "CONNECT":
            upstream = socket.create_connection(host.rsplit(":", 1) if ":" in host else (host, 443), timeout=15)
            client.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            t1 = threading.Thread(target=pipe, args=(client, upstream), daemon=True)
            t2 = threading.Thread(target=pipe, args=(upstream, client), daemon=True)
            t1.start()
            t2.start()
            t1.join()
            t2.join()
        else:
            # Plain HTTP: only origin-form targets on allowlisted hosts.
            if target.startswith("http://"):
                h = target[7:].split("/")[0]
                if not allowed(h):
                    client.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\n\r\nFORBIDDEN")
                    client.close()
                    return
                upstream = socket.create_connection(h.rsplit(":", 1) if ":" in h else (h, 80), timeout=15)
                upstream.sendall(head)
            else:
                upstream = socket.create_connection(host.rsplit(":", 1) if ":" in host else (host, 80), timeout=15)
                upstream.sendall(head)
            resp_head = b""
            upstream.settimeout(30)
            while b"\r\n\r\n" not in resp_head:
                try:
                    chunk = upstream.recv(65536)
                except OSError:
                    break
                if not chunk:
                    break
                resp_head += chunk
            # Split headers/body-so-far; honor framing so keep-alive
            # upstreams don't hang the relay.
            parts = resp_head.split(b"\r\n\r\n", 1)
            headers = parts[0].decode("latin1") if parts else ""
            buffered = parts[1] if len(parts) > 1 else b""
            body = buffered
            clen = None
            chunked = False
            for ln in headers.split("\r\n")[1:]:
                k, _, v = ln.partition(":")
                if k.strip().lower() == "content-length":
                    try:
                        clen = int(v.strip())
                    except ValueError:
                        clen = None
                if k.strip().lower() == "transfer-encoding" and "chunked" in v.lower():
                    chunked = True
            try:
                if chunked:
                    # Read the whole chunk stream, then de-chunk it. `body`
                    # was seeded with `buffered` above for the content-length
                    # and unframed paths; start clean here, or the buffered
                    # prefix is counted twice and the de-chunk pass below
                    # re-parses it as a size line.
                    body = b""
                    raw = buffered
                    while True:
                        while b"\r\n" not in raw:
                            chunk = upstream.recv(65536)
                            if not chunk:
                                break
                            raw += chunk
                        if not raw:
                            break
                        ln, _, rest = raw.partition(b"\r\n")
                        try:
                            size = int(ln.split(b";")[0].strip(), 16)
                        except ValueError:
                            raw = rest
                            continue
                        need = len(ln) + 2 + size + 2
                        while len(raw) < need:
                            chunk = upstream.recv(65536)
                            if not chunk:
                                break
                            raw += chunk
                        # Accumulate every frame. Assigning here overwrote the
                        # frames already collected, so only the last survived --
                        # and when the last frame is the terminal 0-chunk, the
                        # client got a 200 with an empty body.
                        body += raw[:need]
                        if size == 0:
                            break
                        raw = raw[need:]
                    # Re-emit de-chunked as content-length for simplicity.
                    payload = b""
                    raw = body
                    while raw:
                        ln, _, rest = raw.partition(b"\r\n")
                        try:
                            size = int(ln.split(b";")[0].strip(), 16)
                        except ValueError:
                            break
                        if size == 0:
                            break
                        payload += rest[:size]
                        raw = rest[size + 2:]
                    hlines = [l for l in headers.split("\r\n") if not l.lower().startswith("transfer-encoding")]
                    hlines.append("Content-Length: %d" % len(payload))
                    resp = ("\r\n".join(hlines) + "\r\n\r\n").encode("latin1") + payload
                elif clen is not None:
                    need = clen - len(buffered)
                    while need > 0:
                        try:
                            chunk = upstream.recv(min(65536, need))
                        except OSError:
                            break
                        if not chunk:
                            break
                        body += chunk
                        need -= len(chunk)
                    resp = (headers + "\r\n\r\n").encode("latin1") + body
                else:
                    # No framing: read until close with a hard cap (agents
                    # fetch manifests, not streams).
                    cap = 8 * 1024 * 1024
                    while len(body) < cap:
                        try:
                            chunk = upstream.recv(65536)
                        except OSError:
                            break
                        if not chunk:
                            break
                        body += chunk
                    resp = (headers + "\r\n\r\n").encode("latin1") + body
            except OSError:
                resp = (headers + "\r\n\r\n").encode("latin1") + body
            try:
                client.sendall(resp)
            except OSError:
                pass
            upstream.close()
    except Exception:
        pass
    finally:
        client.close()


srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", PORT))
srv.listen(64)
while True:
    # R93 G4: one bad connection (or a failed thread start) must not kill
    # the sidecar, or the run's bill and the whole session are lost.
    try:
        c, _ = srv.accept()
    except OSError:
        continue
    try:
        threading.Thread(target=handle, args=(c,), daemon=True).start()
    except Exception:
        try:
            c.close()
        except OSError:
            pass
