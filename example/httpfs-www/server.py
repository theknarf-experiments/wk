# A file server speaking httpfs's conventions — text/plain autoindex (one
# entry per line, directories with a trailing slash), HEAD, plain GETs —
# on top of Python's stdlib server, which handles the rest (301 on a dir
# without its slash, 404s, Content-Length). Runs as a wasm container on
# wk's fabric: the httpfs node dials http://python:8000 by node name.
import io
import os
import sys
from http.server import HTTPServer, SimpleHTTPRequestHandler


class Autoindex(SimpleHTTPRequestHandler):
    def list_directory(self, path):
        entries = sorted(os.listdir(path))
        body = "".join(
            e + ("/" if os.path.isdir(os.path.join(path, e)) else "") + "\n"
            for e in entries
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        return io.BytesIO(body)


port = int(sys.argv[1]) if len(sys.argv) > 1 else 8000
os.chdir(os.path.join(os.path.dirname(os.path.abspath(__file__)), "public"))
print(f"httpfs-www: serving /app/public on 0.0.0.0:{port}", flush=True)
HTTPServer(("0.0.0.0", port), Autoindex).serve_forever()
