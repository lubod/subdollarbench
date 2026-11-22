#!/usr/bin/env python3
"""R79-2: a local OpenRouter chat-completions stub for the opencode text.

It speaks the streaming SSE shape the OpenRouter provider expects. The
case is fixed at start:

  retry        one 429 with Retry-After, then a 200 text answer
  exhausted    429 on every request
  402          a 402 billing refusal
  stream_error a 200 SSE whose first event is an error
  tool         a tool call, then (after the tool result) a text answer
  ok           a plain text answer
  chaos        R96 Z2: a fixed schedule of hostile calls (slow stream,
               429, empty answer, timeout, trigger words in model prose,
               and a stream left open at the session's end)

Usage: scripts/opencode-stub.py <port> <request-log> <case>
"""
import http.server
import json
import os
import sys
import time

PORT = int(sys.argv[1])
LOG = sys.argv[2]
CASE = sys.argv[3]
COUNT = {"n": 0}

# R96 Z2: the delays are real in a rehearsal (the slow call must outlast
# the agent's five-minute header wait) and short in tests.
SLOW = float(os.environ.get("SDB_CHAOS_SLOW_SECS", "330"))
HANG = float(os.environ.get("SDB_CHAOS_HANG_SECS", "330"))
OPEN = float(os.environ.get("SDB_CHAOS_OPEN_SECS", "60"))


def sse(wfile, chunks):
    for chunk in chunks:
        wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
    wfile.write(b"data: [DONE]\n\n")


def text_sse(wfile, text, charge=0.001):
    sse(wfile, [
        {"id": "gen-stub", "choices": [{"index": 0,
         "delta": {"role": "assistant", "content": text},
         "finish_reason": None}]},
        {"id": "gen-stub", "choices": [{"index": 0, "delta": {},
         "finish_reason": "stop"}],
         "usage": {"prompt_tokens": 10, "completion_tokens": 1,
                   "cost": charge}},
    ])


def tool_sse(wfile):
    sse(wfile, [
        {"id": "gen-stub", "choices": [{"index": 0, "delta": {
            "role": "assistant",
            "tool_calls": [{"index": 0, "id": "call_stub_1", "type": "function",
                            "function": {"name": "bash",
                                         "arguments": "{\"command\":\"echo probe-forty-two\"}"}}],
        }, "finish_reason": None}]},
        {"id": "gen-stub", "choices": [{"index": 0, "delta": {},
         "finish_reason": "tool_calls"}],
         "usage": {"prompt_tokens": 10, "completion_tokens": 1, "cost": 0.001}},
    ])


def rate_limited(wfile):
    wfile.write(b'{"error":{"message":"rate limited","code":429}}')


def chaos_step(n, wfile):
    """One call of the fixed schedule. Returns the charge the stub made."""
    step = (n - 1) % 8
    if step == 0:
        # A slow stream: the headers arrive before the first token, then
        # the answer takes longer than the agent's five-minute wait.
        wfile.write(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
        wfile.write(b'data: {"id":"gen-stub","choices":[{"delta":{"content":"slow"}}]}\n\n')
        wfile.flush()
        time.sleep(SLOW)
        sse(wfile, [
            {"id": "gen-stub", "choices": [{"index": 0,
             "delta": {"content": " but answered"}, "finish_reason": None}]},
            {"id": "gen-stub", "choices": [{"index": 0, "delta": {},
             "finish_reason": "stop"}],
             "usage": {"prompt_tokens": 10, "completion_tokens": 1,
                       "cost": 0.001}},
        ])
        return 0.001
    if step == 1:
        rate_limited(wfile)
        return 0.0
    if step == 2:
        # An answer with no response text: health counts it as empty.
        text_sse(wfile, "")
        return 0.001
    if step == 3:
        # A timeout: the connection is held, then dropped with no answer.
        time.sleep(HANG)
        return 0.0
    if step == 4:
        text_sse(wfile, "the provider said rate limit, try again later")
        return 0.001
    if step == 5:
        text_sse(wfile, "the provider returned too many requests")
        return 0.001
    if step == 6:
        text_sse(wfile, "unauthorized token: check your auth")
        return 0.001
    # Step 7: still answering when the session ends. The first chunk
    # lands, then the stream stays open past the session's teardown.
    wfile.write(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
    wfile.write(b'data: {"id":"gen-stub","choices":[{"delta":{"content":"open"}}]}\n\n')
    wfile.flush()
    time.sleep(OPEN)
    # The session may be gone, but the proxy still bills the call: finish
    # the stream with its usage so the charge is visible in the bill.
    sse(wfile, [{"id": "gen-stub", "choices": [{"delta": {},
        "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 1,
                  "cost": 0.001}}])
    return 0.001


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        COUNT["n"] += 1
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n)
        if CASE == "slow":
            # R85 F1: the proxy-gone test kills the proxy while this
            # request is in flight. R90 M4: the in-flight billing test
            # keeps the call open past the session's teardown.
            time.sleep(float(os.environ.get("SDB_STUB_SLOW_SECS", "3")))
        if CASE == "chaos":
            step = (COUNT["n"] - 1) % 8
            if step == 1:
                self.send_response(429)
                self.send_header("Retry-After", "1")
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                rate_limited(self.wfile)
                charge = 0.0
            elif step == 3:
                charge = 0.0
                time.sleep(HANG)
            else:
                if step not in (0, 7):
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                try:
                    charge = chaos_step(COUNT["n"], self.wfile)
                except (BrokenPipeError, ConnectionResetError):
                    charge = 0.001
            with open(LOG, "a") as f:
                f.write(json.dumps({"case": CASE, "n": COUNT["n"], "path": self.path,
                                    "step": step, "charge": charge,
                                    "body": body.decode("utf-8", "replace")}) + "\n")
            return
        charge = 0.0
        with open(LOG, "a") as f:
            f.write(json.dumps({"case": CASE, "n": COUNT["n"], "path": self.path,
                                "charge": charge,
                                "body": body.decode("utf-8", "replace")}) + "\n")
        if CASE == "retry" and COUNT["n"] == 1:
            self.send_response(429)
            self.send_header("Retry-After", "1")
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error":{"message":"rate limited","code":429}}')
            return
        if CASE == "exhausted":
            self.send_response(429)
            self.send_header("Retry-After", "1")
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error":{"message":"rate limited","code":429}}')
            return
        if CASE == "402":
            self.send_response(402)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error":{"message":"insufficient credits","code":402}}')
            return
        if CASE == "stream_error":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            sse(self.wfile, [{"error": {"message": "upstream stream broke",
                                        "type": "provider.stream"}}])
            return
        if CASE == "tool" and COUNT["n"] == 1:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            tool_sse(self.wfile)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        text_sse(self.wfile, "probe-forty-two" if CASE == "tool" else "ok")

    def log_message(self, *args):
        pass


http.server.HTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
