"""A deterministic OpenAI-compatible chat-completions server for pi (kiln plan
01a1ce705e62732b83f16295d74ee263, step 5): the first completion of a conversation asks for pi's grep
and find tools (ripgrep's and fd's), the next (once tool results are in the conversation) answers
"done". No credential: pi takes any key for a models.json provider (research f29).

    python3 -I mock_provider.py PORT_FILE LOG_FILE

Binds 127.0.0.1 on a free port, writes the port to PORT_FILE, and logs one JSON line per request.
"""

import http.server
import json
import sys

port_file, log_file = sys.argv[1], sys.argv[2]
BASE = {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": 0, "model": "mock-1"}
CALLS = [("call_grep_1", "grep", {"pattern": "needle", "path": "."}), ("call_find_1", "find", {"pattern": "*.txt", "path": "."})]


def log(entry):
    with open(log_file, "a", encoding="utf-8") as f:
        f.write(json.dumps(entry) + "\n")


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, code, body, kind="application/json"):
        data = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        log({"method": "GET", "path": self.path})
        if self.path.rstrip("/").endswith("/models"):
            self.reply(200, json.dumps({"object": "list", "data": [{"id": "mock-1", "object": "model"}]}))
        else:
            self.reply(404, "{}")

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        try:
            req = json.loads(self.rfile.read(length) or b"{}")
        except ValueError:
            self.reply(400, "{}")
            return
        messages = req.get("messages") or []
        answered = any(m.get("role") == "tool" for m in messages if isinstance(m, dict))
        tools = [t.get("function", {}).get("name") for t in req.get("tools") or [] if isinstance(t, dict)]
        log({"method": "POST", "path": self.path, "stream": req.get("stream"), "tool_result_in": answered, "tools": tools})
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self.reply(404, "{}")
            return
        if answered:
            chunks = [
                {**BASE, "choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}, "finish_reason": None}]},
                {**BASE, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        else:
            calls = [{"index": i, "id": cid, "type": "function", "function": {"name": name, "arguments": json.dumps(args)}} for i, (cid, name, args) in enumerate(CALLS)]
            chunks = [
                {**BASE, "choices": [{"index": 0, "delta": {"role": "assistant", "content": None, "tool_calls": calls}, "finish_reason": None}]},
                {**BASE, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
        chunks.append({**BASE, "choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
        if req.get("stream"):
            body = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
            self.reply(200, body, "text/event-stream")
        else:
            message = {"role": "assistant", "content": "done"} if answered else {"role": "assistant", "content": None, "tool_calls": chunks[0]["choices"][0]["delta"]["tool_calls"]}
            finish = "stop" if answered else "tool_calls"
            self.reply(200, json.dumps({"id": "chatcmpl-mock", "object": "chat.completion", "created": 0, "model": "mock-1", "choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}))


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w", encoding="utf-8") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
