"""A real CPython webserver, running as a wasm container on wk's network fabric.

Nothing wasm-aware here — it's the stdlib http.server binding a normal BSD
socket. wk compiles CPython to wasm32-wasip2, so socket() / bind() / listen() /
accept() land on the userspace fabric, and the wired HostPort publishes it to
localhost. Single-threaded (wasip2 has no threads yet), which is all a demo
needs. Run:
  wk run example/python-web.wk    # then: curl localhost:8088
"""
import html
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
hits = 0


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        global hits
        hits += 1
        body = f"""<!doctype html>
<title>CPython on wk</title>
<h1>Hello from CPython {sys.version.split()[0]} 🐍</h1>
<p>Served by the stdlib <code>http.server</code>, compiled to
<code>wasm32-wasip2</code>, talking real TCP over wk's userspace fabric.</p>
<ul>
  <li>path: <code>{html.escape(self.path)}</code></li>
  <li>request #: {hits}</li>
  <li>platform: <code>{sys.platform}</code></li>
</ul>
"""
        payload = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, fmt, *args):
        # Route request logs to stdout so `wk logs` / the terminal node show them.
        sys.stdout.write("%s - %s\n" % (self.address_string(), fmt % args))
        sys.stdout.flush()


def main():
    server = HTTPServer(("0.0.0.0", PORT), Handler)
    print(f"CPython {sys.version.split()[0]} serving on 0.0.0.0:{PORT} — curl localhost:{PORT}")
    sys.stdout.flush()
    server.serve_forever()


if __name__ == "__main__":
    main()
