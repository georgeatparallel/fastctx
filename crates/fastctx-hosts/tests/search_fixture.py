"""Local scripted model and HTTPS MCP fixture for codex_search.rs. No vendor calls.

The CONNECT proxy accepts only search.parallel.ai:443. A temporary certificate,
trusted only by the test's Codex process, lets the test observe the real default
URL and headers without adding a production endpoint override.
"""
import json
import ssl
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "scripts"))
from model_mock import Mock, last_user_text, message, stream

root = Path(sys.argv[1])
log_lock = threading.Lock()


def record(kind, value):
    with log_lock, (root / "requests.jsonl").open("a") as out:
        out.write(json.dumps({"kind": kind, **value}) + "\n")


class Model(Mock):
    def respond(self, handler, request):
        record("model", {"body": request})
        items = request.get("input") or []
        if items and items[-1].get("type") == "function_call_output":
            return stream(handler, [message("Consumed: " + str(items[-1].get("output")))])
        prompt = last_user_text(items)
        wanted = "web_fetch" if "FETCH" in prompt else "web_search"
        for tool in request.get("tools") or []:
            namespace = tool.get("name") if tool.get("type") == "namespace" else None
            for inner in tool.get("tools", []) if namespace else [tool]:
                name = inner.get("name", "")
                if name == wanted or name.endswith("__" + wanted):
                    live = "LIVE" in prompt
                    args = {"urls": ["https://docs.parallel.ai/integrations/mcp/search-mcp" if live else
                                     "https://example.org/source"]} if wanted == "web_fetch" else {
                        "objective": prompt, "search_queries": ["Parallel Search MCP documentation" if live else "fixture source"]}
                    call = {"type": "function_call", "name": name,
                            "call_id": "call_" + uuid.uuid4().hex,
                            "arguments": json.dumps(args)}
                    if namespace:
                        call["namespace"] = namespace
                    return stream(handler, [call])
        return stream(handler, [message("NO_SEARCH_TOOL")])


context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(root / "cert.pem", root / "key.pem")


class MCP(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_CONNECT(self):
        if self.path != "search.parallel.ai:443":
            record("blocked", {"destination": self.path})
            self.send_error(403)
            return
        record("connect", {"destination": self.path})
        self.send_response(200)
        self.end_headers()
        self.connection = context.wrap_socket(self.connection, server_side=True)
        self.rfile = self.connection.makefile("rb", -1)
        self.wfile = self.connection.makefile("wb", 0)
        self.close_connection = False

    def do_GET(self):
        self.send_error(405)

    def do_DELETE(self):
        self.send_response(204)
        self.end_headers()

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        record("mcp", {"path": self.path, "host": self.headers.get("Host"),
                       "user_agent": self.headers.get("User-Agent"),
                       "authorization": self.headers.get("Authorization"), "body": body})
        method = body.get("method")
        if "id" not in body:
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if method == "initialize":
            result = {"protocolVersion": body["params"]["protocolVersion"],
                      "capabilities": {"tools": {}}, "serverInfo": {"name": "fixture", "version": "1"}}
        elif method == "tools/list":
            result = {"tools": [{"name": name, "description": name,
                        "inputSchema": {"type": "object", "properties": props, "required": list(props)}}
                       for name, props in [
                           ("web_search", {"objective": {"type": "string"},
                                           "search_queries": {"type": "array", "items": {"type": "string"}}}),
                           ("web_fetch", {"urls": {"type": "array", "items": {"type": "string"}}})]]}
        elif method == "tools/call":
            args = body["params"].get("arguments", {})
            objective = args.get("objective", "FETCH")
            if "TIMEOUT" in objective or "CANCEL" in objective:
                time.sleep(35)
            text = "fixture-error" if "ERROR" in objective else "fixture-source https://example.org/source"
            result = {"content": [{"type": "text", "text": text}], "isError": "ERROR" in objective}
        else:
            result = {}
        payload = json.dumps({"jsonrpc": "2.0", "id": body["id"], "result": result}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        try:
            self.wfile.write(payload)
        except OSError:
            pass


proxy = ThreadingHTTPServer(("127.0.0.1", 0), MCP)
proxy.daemon_threads = True
threading.Thread(target=proxy.serve_forever, daemon=True).start()
model = Model()
print(json.dumps({"model": model.base_url, "proxy": f"http://127.0.0.1:{proxy.server_port}"}), flush=True)
sys.stdin.read()
model.close()
proxy.shutdown()
