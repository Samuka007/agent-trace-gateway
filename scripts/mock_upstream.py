import http.server, json

class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('content-length', 0))
        body = self.rfile.read(n)
        with open('/tmp/mock_seen.json', 'w') as f:
            json.dump({k: v for k, v in self.headers.items()}, f, indent=1)
        with open('/tmp/mock_body.txt', 'wb') as f:
            f.write(body)
        self.send_response(200)
        self.send_header('content-type', 'text/event-stream')
        self.send_header('x-codex-primary-used-percent', '42')
        self.send_header('x-codex-secondary-used-percent', '7')
        self.end_headers()
        self.wfile.write(b'data: {"ok":true}\n\n')

    def log_message(self, *a):
        pass

http.server.HTTPServer(('127.0.0.1', 8499), H).serve_forever()
