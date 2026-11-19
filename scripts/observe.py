#!/usr/bin/env python3
"""Open the live ene activity viewer in a local browser."""

import argparse
import base64
from http.client import HTTPConnection, HTTPException
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import ipaddress
import json
import os
from pathlib import Path
import secrets
import struct
import sys
import time
import uuid
import webbrowser

ROOT = Path(__file__).resolve().parent.parent
sys.dont_write_bytecode = True
sys.path.insert(0, str(ROOT / ".agents/skills/verify-ene-host/scripts"))
from connect import EnvelopeDecoder, ProtocolError, connection


def default_connection():
    if path := os.environ.get("ENE_HOST_CONNECTION"):
        return Path(path)
    if path := os.environ.get("ENE_HOST_RUNTIME_DIR"):
        return Path(path) / "connection.json"
    development = ROOT / "target/run/host/runtime/connection.json"
    if development.is_file():
        return development
    if runtime := os.environ.get("XDG_RUNTIME_DIR"):
        return Path(runtime) / "ene/connection.json"
    if local := os.environ.get("LOCALAPPDATA"):
        cache = Path(local) / "ene/cache"
    elif sys.platform == "darwin":
        cache = Path.home() / "Library/Caches/ene"
    else:
        cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "ene"
    return cache / "runtime/connection.json"


def local_target(path):
    host, port, prefix, token = connection(path)
    if host != "localhost" and not ipaddress.ip_address(host).is_loopback:
        raise ValueError("接続先はローカルホストに限定されています")
    return host, port, prefix, token


def call(target, method, value, stream=False):
    host, port, prefix, token = target
    body = json.dumps(value).encode()
    headers = {"Authorization": f"Bearer {token}", "Content-Type": "application/json"}
    if stream:
        body = struct.pack(">BI", 0, len(body)) + body
        headers.update({
            "Content-Type": "application/connect+json", "Connect-Protocol-Version": "1",
        })
    http = HTTPConnection(host, port, timeout=20)
    try:
        http.request("POST", f"{prefix}/ene.host.v1.HostService/{method}", body, headers)
        response = http.getresponse()
        if response.status != 200:
            raise OSError(f"ホストから HTTP {response.status}")
        return http, response
    except Exception:
        http.close()
        raise


def snapshot(target):
    http, response = call(target, "GetSnapshot", {})
    try:
        return json.load(response)
    finally:
        http.close()


def display_event(event, names):
    companion = event.get("companionId", "")
    cursor = event.get("cursor", {})
    base = {
        "at": event.get("emittedAt"),
        "companion": names.get(companion, companion),
        "sequence": cursor.get("seq", ""),
        "epoch": cursor.get("epoch", ""),
    }
    if turn := event.get("turnEvent"):
        report = turn.get("eyeReport") or {}
        return {**base, "role": "head", "action": "turn", "detail": {
            "status": turn.get("status"), "turn_id": turn.get("turnId"),
            "failure": (turn.get("failure") or {}).get("code"),
            "report_id": report.get("reportId"), "disposition": report.get("disposition"),
            "reply": report.get("reply"),
        }}
    if work := event.get("workEvent"):
        detail = work.get("statusChanged") or work.get("runFinished") or {}
        return {**base, "role": "hands", "action": "work", "detail": {
            "kind": work.get("kind"), "status": detail.get("status") or detail.get("state"),
            "work_id": work.get("workId"), "run_id": work.get("runId"),
            "failure": (detail.get("failure") or {}).get("code"),
        }}
    if approval := (event.get("approvalRequested") or {}).get("request"):
        return {**base, "role": "hands" if approval.get("runId") else "head",
                "action": "approval", "detail": {"tool": approval.get("toolName")}}
    if event.get("kind") == "diagnostic":
        data = json.loads(event["generic"]["payload"]["json"])
        return {**base, **data}
    if (generic := event.get("generic") or {}).get("kind") == "perception_status":
        data = json.loads(generic["payload"]["json"])
        return {**base, "role": "eyes", "action": "model_status", "detail": {
            "status": data.get("status"),
        }}
    return None


def display_stream(path, cursor=None):
    names = {}
    while True:
        try:
            target = local_target(path)
            current = snapshot(target)
            names = {item["id"]: item["name"] for item in current.get("companions", [])}
            if not cursor:
                cursor = current.get("cursor")
                if cursor:
                    cursor = {**cursor, "seq": str(max(0, int(cursor.get("seq", 0)) - 250))}
            diagnostics = "diagnostics" in current.get("capabilities", {}).get("features", [])
            request = {"includeDiagnostics": True} if diagnostics else {}
            connected = {"state": "live", "limited": not diagnostics}
            if cursor:
                request["cursor"] = cursor
            http, response = call(target, "Subscribe", request, stream=True)
            try:
                yield "status", connected
                decoder = EnvelopeDecoder()
                while data := response.read1(65536):
                    for flags, payload in decoder.feed(data):
                        if flags & 2:
                            raise OSError("ホストの購読が終了しました")
                        event = json.loads(payload)
                        if event.get("cursor"):
                            cursor = event["cursor"]
                        if companion := event.get("companion"):
                            names[companion["id"]] = companion["name"]
                        if event.get("resyncRequired") or event.get("initialState"):
                            yield "status", {**connected, "gap": True}
                        if row := display_event(event, names):
                            yield "row", row
                yield "status", {"state": "waiting"}
            finally:
                http.close()
        except TimeoutError:
            continue
        except (OSError, ValueError, KeyError, ProtocolError, HTTPException):
            yield "status", {"state": "waiting"}
        time.sleep(1)


def serve(path, open_browser):
    prefix = "/" + secrets.token_urlsafe(24) + "/"
    page_file = Path(__file__).with_name("observer.html")

    class Viewer(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            authority = f"127.0.0.1:{self.server.server_port}"
            if (self.headers.get("Host") != authority
                    or self.headers.get("Origin") not in (None, f"http://{authority}")):
                self.send_error(403)
                return
            page = self.path.split("?", 1)[0]
            if page.startswith(prefix + "frame/"):
                self.serve_frame(page.removeprefix(prefix + "frame/"))
                return
            if page not in (prefix, prefix + "events"):
                self.send_error(404)
                return
            self.send_response(200)
            is_page = page == prefix
            self.send_header("Content-Type", "text/html; charset=utf-8" if is_page else "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("Content-Security-Policy",
                             "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; "
                             "connect-src 'self'; img-src 'self'; frame-ancestors 'none'; base-uri 'none'")
            self.end_headers()
            if is_page:
                self.wfile.write(page_file.read_bytes())
                return
            last_epoch, _, last_seq = self.headers.get("Last-Event-ID", "").partition(":")
            cursor = {"epoch": last_epoch, "seq": last_seq} if last_epoch and last_seq.isdecimal() else None
            try:
                for kind, data in display_stream(path, cursor):
                    sse_id = f'{data["epoch"]}:{data["sequence"]}' if kind == "row" else ""
                    output = f"id: {sse_id}\n" if sse_id else ""
                    output += f"event: {kind}\ndata: {json.dumps(data, ensure_ascii=False)}\n\n"
                    self.wfile.write(output.encode("utf-8"))
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass

        def serve_frame(self, frame_id):
            try:
                if str(uuid.UUID(frame_id)) != frame_id:
                    raise ValueError("invalid frame id")
                http, response = call(local_target(path), "GetPerceptionFrame", {"id": frame_id})
                try:
                    jpeg = base64.b64decode(json.load(response)["jpeg"], validate=True)
                finally:
                    http.close()
            except (OSError, ValueError, KeyError, HTTPException):
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Type", "image/jpeg")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(jpeg)))
            self.end_headers()
            try:
                self.wfile.write(jpeg)
            except (BrokenPipeError, ConnectionResetError):
                pass

    with ThreadingHTTPServer(("127.0.0.1", 0), Viewer) as server:
        url = f"http://127.0.0.1:{server.server_port}{prefix}"
        print(url, flush=True)
        if open_browser:
            webbrowser.open(url)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass


def self_test():
    row = display_event({"kind": "diagnostic", "generic": {"payload": {"json": json.dumps({
        "role": "eyes", "action": "screen_received", "detail": {"bytes": 1024},
    })}}}, {})
    assert row["detail"]["bytes"] == 1024
    assert display_event({"messageCreated": {"message": {"text": "会話本文"}}}, {}) is None
    assert display_event({"turnEvent": {"status": "TURN_STATUS_ANSWERED"}}, {})["role"] == "head"
    watched = display_event({"turnEvent": {
        "status": "TURN_STATUS_STOPPED", "eyeReport": {
            "reportId": "report-1", "disposition": "DISPOSITION_WATCHED",
        },
    }}, {})
    assert watched["detail"]["report_id"] == "report-1"
    assert watched["detail"]["disposition"] == "DISPOSITION_WATCHED"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--connection", type=Path, default=default_connection())
    parser.add_argument("--no-open", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
    else:
        serve(args.connection, not args.no_open)


if __name__ == "__main__":
    main()
