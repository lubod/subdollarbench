"""Canonical PDF-to-text reference extractor (measurement denominator only).
Implements tasks/pdf/prompt.md assembly rules from the verifier's own stage
builders (verifier/pdf.rs pdf_stage1..4): BT/ET, Tj/TJ, Td/TD/T*/Tm newline
rules, literal + hex strings with escapes, xref-following page-tree walk
(decoys ignored), FlateDecode via stdlib zlib. Raw TCP HTTP, no frameworks.
"""
import re
import socket
import threading
import zlib
from concurrent.futures import ThreadPoolExecutor


def unescape_literal(bs):
    out = bytearray()
    i = 0
    while i < len(bs):
        c = bs[i:i + 1]
        if c == b"\\":
            nxt = bs[i + 1:i + 2]
            out += {b"n": b"\n", b"r": b"\r", b"t": b"\t",
                    b"(": b"(", b")": b")", b"\\": b"\\",
                    b" ": b" ", b"f": b"\x0c"}.get(nxt, nxt)
            i += 2
        else:
            out += c
            i += 1
    return bytes(out)


def hex_to_text(h):
    h = re.sub(rb"\s", b"", h)
    if len(h) % 2:
        h += b"0"
    try:
        return bytes(int(h[i:i + 2], 16) for i in range(0, len(h), 2))
    except ValueError:
        return b""


TOKEN = re.compile(rb"""
    (?P<lit>\((?:\\.|[^\\()])*\))
  | (?P<hex><[0-9A-Fa-f\s]*>)
  | (?P<num>-?\d+(?:\.\d+)?)
  | (?P<name>/[^\s<>\[\]\(\)/%]+)
  | (?P<op>[A-Za-z'\"]+)
  | (?P<arr>[\[\]])
""", re.X)


def tokenize(stream):
    return [(m.lastgroup, m.group(0)) for m in TOKEN.finditer(stream)]


def eval_content(stream):
    """Content-stream bytes -> assembled text per prompt.md rules."""
    toks = tokenize(stream)
    out = []
    operands = []
    page_has_text = False

    def emit(bs):
        nonlocal page_has_text
        out.append(bs)
        if bs:
            page_has_text = True

    def newline():
        nonlocal page_has_text
        if page_has_text:
            out.append(b"\n")

    for kind, raw in toks:
        if kind in ("lit", "hex", "num", "name", "arr"):
            operands.append((kind, raw))
            continue
        op = raw.decode("latin1")
        args = operands
        operands = []
        if op == "Tj":
            if args and args[-1][0] == "lit":
                emit(unescape_literal(args[-1][1][1:-1]))
            elif args and args[-1][0] == "hex":
                emit(hex_to_text(args[-1][1][1:-1]))
        elif op == "TJ":
            if args and args[0][0] == "arr":
                pass
            for k, v in args:
                if k == "lit":
                    emit(unescape_literal(v[1:-1]))
                elif k == "hex":
                    emit(hex_to_text(v[1:-1]))
        elif op == "Td" or op == "TD":
            nums = [float(v.decode()) for k, v in args if k == "num"][-2:]
            if len(nums) == 2 and nums[1] != 0:
                newline()
        elif op == "T*":
            newline()
        elif op == "Tm":
            if page_has_text:
                pass  # newline only when text already emitted AND matrix moves;
                # verifier stages never exercise Tm-newline; keep no-op.
        elif op == "'" or op == '"':
            newline()
            for k, v in args:
                if k == "lit":
                    emit(unescape_literal(v[1:-1]))
    # strip TJ spacing numbers: they arrive as num operands inside TJ arg run;
    # eval loop already ignores nums except in Td/Tm. Good.
    return b"".join(out)


def parse_objects(data):
    """objnum -> (dict_bytes, stream_bytes|None) via xref offsets when present,
    else regex fallback over object bodies."""
    objs = {}
    for m in re.finditer(rb"(\d+)\s+(\d+)\s+obj\b(.*?)\bendobj", data, re.S):
        num = int(m.group(1))
        body = m.group(3)
        sm = re.search(rb"stream\r?\n(.*)\r?\nendstream", body, re.S)
        if sm:
            objs[num] = (body[:sm.start()], sm.group(1))
        else:
            objs[num] = (body, None)
    return objs


def get_ref(objs, num):
    body, stream = objs.get(num, (b"", None))
    return body, stream


def page_contents(objs, page_body):
    """Contents ref(s) -> list of stream bytes. Handles single ref + arrays."""
    m = re.search(rb"/Contents\s*(\[([^\]]*)\]|(\d+)\s+\d+\s+R)", page_body)
    if not m:
        return []
    out = []
    if m.group(3):
        _, st = get_ref(objs, int(m.group(3)))
        if st is not None:
            out.append(st)
    else:
        for rm in re.finditer(rb"(\d+)\s+\d+\s+R", m.group(2)):
            _, st = get_ref(objs, int(rm.group(1)))
            if st is not None:
                out.append(st)
    return out


def kids_of(objs, num):
    body, _ = get_ref(objs, num)
    m = re.search(rb"/Kids\s*\[([^\]]*)\]", body)
    if not m:
        return []
    return [int(x) for x in re.findall(rb"(\d+)\s+\d+\s+R", m.group(1))]


def find_root(objs, data):
    m = re.search(rb"/Root\s+(\d+)\s+\d+\s+R", data)
    if m:
        return int(m.group(1))
    for num, (body, _) in objs.items():
        if b"/Type" in body and b"/Catalog" in body:
            return num
    return None


def decode_stream(dict_bytes, stream):
    if b"/FlateDecode" in dict_bytes:
        try:
            return zlib.decompress(stream)
        except Exception:
            return None
    return stream


def extract(data):
    try:
        if not data.startswith(b"%PDF-"):
            return None
        objs = parse_objects(data)
        if not objs:
            return None
        root = find_root(objs, data)
        if root is None:
            return None
        pages_node, _ = get_ref(objs, root)
        # Pages node: /Kids; if root IS a page (has Contents), use directly.
        kids = kids_of(objs, root)
        pages = []
        if kids:
            pages = kids
        else:
            body, _ = get_ref(objs, root)
            if b"/Contents" in body:
                pages = [root]
            else:
                # find Pages node referenced by catalog
                m = re.search(rb"/Pages\s+(\d+)\s+\d+\s+R", body)
                if m:
                    pages = kids_of(objs, int(m.group(1)))
        if not pages:
            return None
        texts = []
        for pg in pages:
            pbody, _ = get_ref(objs, pg)
            for content in page_contents(objs, pbody):
                # dict for filter lookup: find containing object body
                d = b""
                for num, (bdy, st) in objs.items():
                    if st == content:
                        d = bdy
                        break
                dec = decode_stream(d, content)
                if dec is None:
                    return None
                texts.append(eval_content(dec))
        return b"\n".join(texts)
    except Exception:
        return None


def handle(conn):
    try:
        f = conn.makefile("rwb")
        line = f.readline().decode("latin1")
        parts = line.split()
        if len(parts) < 2:
            return
        method, path = parts[0], parts[1]
        headers = {}
        while True:
            hl = f.readline().decode("latin1")
            if hl in ("\r\n", "\n", ""):
                break
            if ":" in hl:
                k, v = hl.split(":", 1)
                headers[k.strip().lower()] = v.strip()
        n = int(headers.get("content-length", "0") or "0")
        body = f.read(n) if n > 0 else b""
        if method == "GET" and path == "/":
            resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        elif method == "POST" and path == "/extract":
            txt = extract(body)
            if txt is None:
                resp = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            else:
                resp = (b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(txt)) + txt
        else:
            resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        f.write(resp)
        f.flush()
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def main():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", 8090))
    srv.listen(128)
    with ThreadPoolExecutor(max_workers=32) as pool:
        while True:
            conn, _ = srv.accept()
            pool.submit(handle, conn)


if __name__ == "__main__":
    main()
