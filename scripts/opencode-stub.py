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

Usage: scripts/opencode-stub.py <port> <request-log> <case>
"""
import http.server
import json
import sys

PORT = int(sys.argv[1])
LOG = sys.argv[2]
CASE = sys.argv[3]
COUNT = {"n": 0}


def sse(wfile, chunks):
    for chunk in chunks:
        wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
    wfile.write(b"data: [DONE]\n\n")


def text_sse(wfile, text):
    sse(wfile, [
        {"id": "gen-stub", "choices": [{"index": 0,
         "delta": {"role": "assistant", "content": text},
         "finish_reason": None}]},
        {"id": "gen-stub", "choices": [{"index": 0, "delta": {},
         "finish_reason": "stop"}],
         "usage": {"prompt_tokens": 10, "completion_tokens": 1}},
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
         "usage": {"prompt_tokens": 10, "completion_tokens": 1}},
    ])


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        COUNT["n"] += 1
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n)
        with open(LOG, "a") as f:
            f.write(json.dumps({"case": CASE, "n": COUNT["n"], "path": self.path,
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


http.server.HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
