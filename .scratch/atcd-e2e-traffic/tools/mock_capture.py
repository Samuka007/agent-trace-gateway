#!/usr/bin/env python3
"""Mock codex 上游：落盘每个请求的 headers/body 到 OUTDIR（按请求编号），回哑 SSE。

与 scripts/mock_upstream.py 同响应形状（data: {"ok":true}\n\n + x-codex-*-used-percent），
但：(1) 落盘到 R11 evidence 目录而非 /tmp 固定名；(2) 记录请求行（method/path）与
client 地址；(3) 文件名带请求序号，支持同一实例被多个 agent 依次打。
"""
import http.server
import json
import os
import threading

OUTDIR = os.environ["MOCK_OUTDIR"]
PORT = int(os.environ.get("MOCK_PORT", "8499"))
os.makedirs(OUTDIR, exist_ok=True)
_lock = threading.Lock()
_counter = 0


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        global _counter
        n = int(self.headers.get("content-length", 0))
        body = self.rfile.read(n)
        with _lock:
            idx = _counter
            _counter += 1
        tag = f"req{idx:03d}"
        rec = {
            "request_line": f"{self.command} {self.path}",
            "client": f"{self.client_address[0]}:{self.client_address[1]}",
            "headers": {k: v for k, v in self.headers.items()},
        }
        with open(os.path.join(OUTDIR, tag + ".upstream.headers.json"), "w") as f:
            json.dump(rec, f, indent=1, ensure_ascii=False)
        with open(os.path.join(OUTDIR, tag + ".upstream.body.bin"), "wb") as f:
            f.write(body)
        with open(os.path.join(OUTDIR, "_seen_count"), "w") as f:
            f.write(str(_counter))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("x-codex-primary-used-percent", "42")
        self.send_header("x-codex-secondary-used-percent", "7")
        self.end_headers()
        self.wfile.write(b'data: {"ok":true}\n\n')

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
