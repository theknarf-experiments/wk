"""A tiny CPython webserver for the browser demo: serves the files beside it
(index.html and friends) over wk's network fabric so the NetSurf node can
browse them. Nothing wasm-aware — stdlib http.server on a normal BSD socket.
Run:
  wk run example/browser.wk
"""
import os
import sys
from http.server import HTTPServer, SimpleHTTPRequestHandler

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8080


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=os.path.dirname(__file__) or ".", **kwargs)

    def log_message(self, fmt, *args):
        sys.stdout.write("%s - %s\n" % (self.address_string(), fmt % args))
        sys.stdout.flush()


def main():
    server = HTTPServer(("0.0.0.0", PORT), Handler)
    print(f"serving the browser demo on 0.0.0.0:{PORT}")
    sys.stdout.flush()
    server.serve_forever()


if __name__ == "__main__":
    main()
