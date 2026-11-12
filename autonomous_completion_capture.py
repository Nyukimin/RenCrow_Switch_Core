"""Temporary loopback-only test transport; records actual provider inputs, not inferred history."""

from contextlib import contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import threading
import time
import socket
from urllib.parse import urlsplit


@contextmanager
def capture_requests(output: Path, upstream: str):
    target = urlsplit(upstream)
    if target.scheme != "http" or target.hostname not in {
        "127.0.0.1",
        "localhost",
        "::1",
    }:
        raise ValueError("test capture supports only an explicit loopback HTTP backend")
    if target.username or target.password or target.query or target.fragment:
        raise ValueError(
            "credential-bearing or ambiguous capture target is not allowed"
        )
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    lock = threading.Lock()
    counter = 0
    connections = set()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, *args):
            pass

        def do_GET(self):
            self.forward()

        def do_POST(self):
            self.forward()

        def forward(self):
            nonlocal counter
            if self.headers.get("Authorization") or self.headers.get(
                "Transfer-Encoding"
            ):
                self.send_error(
                    400, "test transport accepts no credentials or chunked requests"
                )
                return
            try:
                length = int(self.headers.get("Content-Length", "0"))
                if not 0 <= length <= 16 * 1024 * 1024:
                    raise ValueError("request size")
            except ValueError:
                self.send_error(413)
                return
            body = self.rfile.read(length)
            with lock:
                counter += 1
                ordinal = counter
            try:
                parsed = json.loads(body) if body else None
            except ValueError:
                self.send_error(400, "invalid JSON request")
                return
            record = {
                "ordinal": ordinal,
                "time_ns": time.time_ns(),
                "method": self.command,
                "path": self.path,
                "body": parsed,
            }
            (output / f"{ordinal:05d}.json").write_text(
                json.dumps(record, ensure_ascii=False) + "\n"
            )
            connection = HTTPConnection(target.hostname, target.port or 80, timeout=900)
            with lock:
                connections.add(connection)
            try:
                headers = {
                    key: value
                    for key, value in self.headers.items()
                    if key.lower()
                    not in {"host", "connection", "content-length", "transfer-encoding"}
                }
                connection.request(
                    self.command, self.path, body=body or None, headers=headers
                )
                response = connection.getresponse()
                self.send_response(response.status)
                for key, value in response.getheaders():
                    if key.lower() not in {"connection", "transfer-encoding"}:
                        self.send_header(key, value)
                self.send_header("Connection", "close")
                self.end_headers()
                while chunk := response.read1(65536):
                    self.wfile.write(chunk)
                    self.wfile.flush()
            finally:
                connection.close()
                with lock:
                    connections.discard(connection)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}{target.path.rstrip('/')}"
    finally:
        server.shutdown()
        with lock:
            active = list(connections)
        for connection in active:
            if connection.sock is not None:
                try:
                    connection.sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
            connection.close()
        server.server_close()
        thread.join(timeout=5)
