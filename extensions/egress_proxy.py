"""Allowlist egress proxy (stdlib only): HTTP GET + CONNECT tunneling.
Usage: python3 egress_proxy.py <port>
Env ALLOWLIST (comma hosts), default covers openrouter + registries + github.
NO_PROXY semantics: client-side (agent env); this proxy 403s anything off-list.
"""
import os
import socket
import threading

PORT = int(__import__("sys").argv[1]) if len(__import__("sys").argv) > 1 else 3128
ALLOW = set(
    os.environ
    .get(
        "ALLOWLIST",
        "api.openrouter.ai,crates.io,static.crates.io,proxy.golang.org,"
        "registry.npmjs.org,pypi.org,files.pythonhosted.org,github.com,"
        "objects.githubusercontent.com,codeload.github.com",
    )
    .split(",")
)


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
                    # Read chunk stream until terminal 0-chunk.
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
                        body = raw[:need] if len(raw) >= need else raw
                        if size == 0:
                            break
                        raw = raw[need:]
                        if size == 0:
                            break
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
    c, _ = srv.accept()
    threading.Thread(target=handle, args=(c,), daemon=True).start()
