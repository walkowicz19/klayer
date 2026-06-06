"""Serve test fixtures on http://localhost:8765/ for klayer ingest testing."""
import http.server, os

PORT = 8765
FIXTURE_DIR = os.path.join(os.path.dirname(__file__), "fixtures")

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=FIXTURE_DIR, **kw)
    def log_message(self, fmt, *args):
        print(f"  [{self.address_string()}] {fmt % args}")

print(f"Serving fixtures at http://localhost:{PORT}/")
print("  /sample.html  -> text/html")
print("  /sample.md    -> text/markdown")
print("  /sample.json  -> application/json")
print("  /sample.txt   -> text/plain")
print("  /sample.pdf   -> application/pdf")
print("Ctrl-C to stop.\n")
with http.server.HTTPServer(("", PORT), Handler) as srv:
    srv.serve_forever()
