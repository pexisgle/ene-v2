#!/usr/bin/env python3
"""Minimal Connect-over-HTTP client for an ene-host verification instance.

Standard library only. Reads the endpoint and Owner token from connection.json.

Usage:
  connect.py rpc SERVICE/Method '<json>' [--connection FILE] [--timeout SECS]
  connect.py stream SERVICE/Method '<json>' [--connection FILE]
                    [--count N] [--timeout SECS]

  SERVICE   full proto service, e.g. ene.host.v1.HostService
  Method    e.g. GetSnapshot, Subscribe
  '<json>'  request body; '@path' reads it from a file

Output: each response message as one compact JSON line on stdout. The HTTP
status goes to stderr (nothing else). Exit 0 means HTTP 200 and, for stream,
that --count frames arrived. Exit 3 means the stream timed out first.

The Connect streaming envelope is a 1-byte flag, a 4-byte big-endian length,
then the JSON payload. Unary calls use plain JSON (application/json), streams
use application/connect+json with a single data frame as the request.
"""
import argparse
import json
import socket
import struct
import sys
import time
from urllib.parse import urlsplit


class ProtocolError(Exception):
    pass


class Reader:
    """Buffered socket reader with deadline-aware reads."""

    def __init__(self, sock, timeout):
        self.sock = sock
        self.buf = b""
        self.timeout = timeout
        self.sock.settimeout(min(timeout, 0.5))

    def fill(self):
        while True:
            if self.buf:
                return
            try:
                data = self.sock.recv(65536)
            except socket.timeout:
                self.sock.settimeout(min(self.timeout, 0.5))
                continue
            if not data:
                raise EOFError("connection closed")
            self.buf += data
            return

    def read_until(self, marker):
        while marker not in self.buf:
            self.fill()
        end = self.buf.index(marker) + len(marker)
        out, self.buf = self.buf[:end], self.buf[end:]
        return out

    def read_exact(self, n):
        while len(self.buf) < n:
            self.fill()
        out, self.buf = self.buf[:n], self.buf[n:]
        return out


class ChunkDecoder:
    """Incremental HTTP/1.1 chunked-transfer decoder."""

    def __init__(self):
        self.raw = b""
        self.state = "size"
        self.remaining = 0
        self.done = False

    def feed(self, data):
        self.raw += data
        out = []
        while True:
            if self.state == "size":
                line, sep, rest = self.raw.partition(b"\r\n")
                if not sep:
                    break
                self.raw = rest
                size = int(line.split(b";")[0].strip() or b"0", 16)
                if size == 0:
                    self.state = "trailer"
                else:
                    self.state = "data"
                    self.remaining = size
            elif self.state == "data":
                if len(self.raw) < self.remaining + 2:
                    break
                out.append(self.raw[: self.remaining])
                self.raw = self.raw[self.remaining + 2:]
                self.state = "size"
            elif self.state == "trailer":
                if self.raw.startswith(b"\r\n"):
                    self.raw = self.raw[2:]
                    self.done = True
                    break
                line, sep, rest = self.raw.partition(b"\r\n")
                if not sep:
                    break
                self.raw = rest
            if self.done:
                break
        return out


class EnvelopeDecoder:
    """Incremental Connect streaming envelope decoder."""

    def __init__(self):
        self.raw = b""

    def feed(self, data):
        self.raw += data
        out = []
        while len(self.raw) >= 5:
            flags = self.raw[0]
            size = int.from_bytes(self.raw[1:5], "big")
            if len(self.raw) < 5 + size:
                break
            payload = self.raw[5:5 + size]
            self.raw = self.raw[5 + size:]
            out.append((flags, payload))
        return out


def connection(path):
    with open(path, "rb") as handle:
        value = json.load(handle)
    endpoint, token = value["endpoint"], value["token"]
    parts = urlsplit(endpoint)
    if parts.scheme != "http" or not parts.hostname:
        raise ProtocolError(f"unsupported endpoint: {endpoint}")
    return parts.hostname, parts.port, parts.path.rstrip("/"), token


def open_call(target, path, body, headers, timeout):
    host, port, prefix, token = target
    sock = socket.create_connection((host, port), timeout=timeout)
    base = {
        "Host": f"{host}:{port}",
        "Authorization": f"Bearer {token}",
        "Connection": "close",
    }
    base.update(headers)
    request = [f"POST {prefix}/{path} HTTP/1.1"]
    request += [f"{key}: {value}" for key, value in base.items()]
    request.append(f"Content-Length: {len(body)}")
    request.append("")
    request.append("")
    sock.sendall("\r\n".join(request).encode() + body)

    reader = Reader(sock, timeout)
    status_line = reader.read_until(b"\r\n").decode("latin-1")
    parts = status_line.split(" ", 2)
    try:
        status = int(parts[1])
    except (IndexError, ValueError) as error:
        raise ProtocolError(f"bad status line: {status_line!r}") from error
    response_headers = {}
    while True:
        line = reader.read_until(b"\r\n")
        if line == b"\r\n":
            break
        key, _, value = line.partition(b":")
        response_headers[key.strip().lower().decode("latin-1")] = value.strip().decode("latin-1")
    return sock, reader, status, response_headers


def read_unary(reader, headers, timeout):
    body = b""
    if headers.get("transfer-encoding", "").lower() == "chunked":
        decoder = ChunkDecoder()
        deadline = time.monotonic() + timeout
        while not decoder.done:
            if time.monotonic() > deadline:
                raise ProtocolError("timed out reading the response body")
            try:
                data = _drain(reader)
            except EOFError:
                break
            if not data:
                continue
            body += b"".join(decoder.feed(data))
    else:
        length = int(headers.get("content-length", "0"))
        if length:
            body = reader.read_exact(length)
    return body


def _drain(reader):
    """Return whatever the socket has right now; empty on timeout."""
    if reader.buf:
        out, reader.buf = reader.buf, b""
        return out
    try:
        data = reader.sock.recv(65536)
    except socket.timeout:
        return b""
    if not data:
        raise EOFError("connection closed")
    return data


def unary(args, target):
    body = args.body.encode() if not args.body.startswith("@") else open(args.body[1:], "rb").read()
    sock, reader, status, headers = open_call(
        target, args.method, body,
        {"Content-Type": "application/json"},
        args.timeout,
    )
    try:
        payload = read_unary(reader, headers, args.timeout)
    finally:
        sock.close()
    if payload:
        sys.stdout.write(payload.decode("utf-8", "replace") + "\n")
    sys.stderr.write(f"HTTP {status}\n")
    return 0 if status == 200 else 1


def stream(args, target):
    body = args.body.encode() if not args.body.startswith("@") else open(args.body[1:], "rb").read()
    envelope = b"\x00" + struct.pack(">I", len(body)) + body
    sock, reader, status, headers = open_call(
        target, args.method, envelope,
        {
            "Content-Type": "application/connect+json",
            "Connect-Protocol-Version": "1",
        },
        args.timeout,
    )
    sys.stderr.write(f"HTTP {status}\n")
    if status != 200:
        payload = read_unary(reader, headers, args.timeout)
        sys.stdout.write(payload.decode("utf-8", "replace") + "\n")
        sock.close()
        return 1

    decoder = ChunkDecoder()
    envelopes = EnvelopeDecoder()
    received = 0
    frames = []
    error_seen = False
    deadline = time.monotonic() + args.timeout
    try:
        while received < args.count and not error_seen:
            if time.monotonic() > deadline:
                break
            data = _drain(reader)
            if not data:
                if decoder.done:
                    break
                continue
            for chunk in decoder.feed(data):
                for flags, payload in envelopes.feed(chunk):
                    frames.append((flags, payload))
                    received += 1
    except EOFError:
        pass
    finally:
        sock.close()

    for flags, payload in frames:
        sys.stdout.write(payload.decode("utf-8", "replace") + "\n")
        if flags & 0x02:
            error_seen = b'"error"' in payload
    if received < args.count and not error_seen:
        return 3
    return 1 if error_seen else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("rpc", "stream"):
        item = sub.add_parser(name)
        item.add_argument("method", help="SERVICE/Method, e.g. ene.host.v1.HostService/GetSnapshot")
        item.add_argument("body", nargs="?", default="{}", help="request JSON, or @path")
        item.add_argument("--connection", default=None, help="connection.json path")
        item.add_argument("--timeout", type=float, default=10.0)
        if name == "stream":
            item.add_argument("--count", type=int, default=1, help="frames to wait for")
    args = parser.parse_args()
    if "/" not in args.method:
        parser.error("method must be SERVICE/Method")
    path = args.connection or __import__("os").environ.get("ENE_HOST_CONNECTION")
    if not path:
        parser.error("pass --connection or set ENE_HOST_CONNECTION")
    try:
        target = connection(path)
        return unary(args, target) if args.command == "rpc" else stream(args, target)
    except (OSError, KeyError, ValueError, ProtocolError) as error:
        sys.stderr.write(f"error: {error}\n")
        return 2


if __name__ == "__main__":
    sys.exit(main())
