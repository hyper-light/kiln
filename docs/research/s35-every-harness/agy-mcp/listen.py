import http.server, sys, json
log = open(sys.argv[2], "a")
class H(http.server.BaseHTTPRequestHandler):
    def _seen(self):
        n = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(n).decode(errors="replace") if n else ""
        log.write(json.dumps({"method": self.command, "path": self.path, "auth": self.headers.get("authorization"), "body": body[:300]}) + "\n"); log.flush()
        self.send_response(401); self.end_headers()
    do_GET = do_POST = do_DELETE = _seen
    def log_message(self, *a): pass
srv = http.server.HTTPServer(("127.0.0.1", 0), H)
open(sys.argv[1], "w").write(str(srv.server_address[1]))
srv.serve_forever()
