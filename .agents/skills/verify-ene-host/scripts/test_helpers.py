#!/usr/bin/env python3
"""Run with python3 -m unittest discover -s .agents/skills/verify-ene-host/scripts."""
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest

SCRIPTS = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


connect = load("connect")


class FragmentedSocket:
    def __init__(self, fragments):
        self.fragments = iter(fragments)

    def settimeout(self, timeout):
        pass

    def recv(self, count):
        return next(self.fragments, b"")


class ReaderTests(unittest.TestCase):
    def setUp(self):
        def expired(signum, frame):
            raise AssertionError("reader exceeded the test watchdog")
        previous = signal.signal(signal.SIGALRM, expired)
        self.addCleanup(signal.signal, signal.SIGALRM, previous)
        self.addCleanup(signal.alarm, 0)
        signal.alarm(2)

    def test_fragmented_lines_and_exact_body(self):
        reader = connect.Reader(FragmentedSocket([
            b"HTTP/1.", b"1 200 OK\r", b"\nContent-Len", b"gth: 7\r\n\r", b"\n{\"n\"", b":1}",
        ]), 0.2)
        self.assertEqual(reader.read_until(b"\r\n"), b"HTTP/1.1 200 OK\r\n")
        self.assertEqual(reader.read_until(b"\r\n"), b"Content-Length: 7\r\n")
        self.assertEqual(reader.read_until(b"\r\n"), b"\r\n")
        self.assertEqual(reader.read_exact(7), b'{"n":1}')

    def test_partial_eof(self):
        for operation in (lambda r: r.read_until(b"\r\n"), lambda r: r.read_exact(5)):
            with self.subTest(operation=operation), self.assertRaises(EOFError):
                operation(connect.Reader(FragmentedSocket([b"abc"]), 0.1))

    def test_real_deadline_with_partial_input(self):
        for operation in (lambda r: r.read_until(b"\r\n"), lambda r: r.read_exact(5)):
            with self.subTest(operation=operation):
                left, right = socket.socketpair()
                with left, right:
                    right.sendall(b"abc")
                    start = time.monotonic()
                    with self.assertRaises(TimeoutError):
                        operation(connect.Reader(left, 0.06))
                    self.assertLess(time.monotonic() - start, 0.5)


class ClientTests(unittest.TestCase):
    def call(self, fragments, command="rpc", hold=False, count=1):
        with tempfile.TemporaryDirectory() as temp, socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            listener.settimeout(2)
            connection = Path(temp) / "connection.json"
            connection.write_text(json.dumps({"endpoint": f"http://127.0.0.1:{listener.getsockname()[1]}", "token": "fixture"}))
            request = []
            release = threading.Event()
            errors = []

            def serve():
                try:
                    with listener.accept()[0] as sock:
                        sock.settimeout(2)
                        raw = b""
                        while b"\r\n\r\n" not in raw:
                            raw += sock.recv(4096)
                        head, body = raw.split(b"\r\n\r\n", 1)
                        length = int(next(line.split(b":", 1)[1] for line in head.split(b"\r\n") if line.lower().startswith(b"content-length:")))
                        while len(body) < length:
                            body += sock.recv(4096)
                        request.append((head, body))
                        for fragment in fragments:
                            sock.sendall(fragment)
                            time.sleep(0.002)
                        if hold:
                            release.wait(2)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception as error:
                    errors.append(error)

            thread = threading.Thread(target=serve)
            thread.start()
            try:
                args = [sys.executable, str(SCRIPTS / "connect.py"), command, "fixture.Service/Call", '{"input":1}', "--connection", str(connection), "--timeout", "0.15"]
                if command == "stream":
                    args += ["--count", str(count)]
                result = subprocess.run(args, capture_output=True, text=True, timeout=2)
            finally:
                release.set()
                thread.join(3)
            self.assertFalse(thread.is_alive())
            self.assertEqual(errors, [])
            self.assertIn(b"Authorization: Bearer fixture", request[0][0])
            return result, request[0]

    def test_fragmented_status_headers_and_body(self):
        result, request = self.call([b"HTTP/1.", b"1 200 OK\r", b"\nContent-Len", b"gth: 7\r\n\r", b"\n{\"n\"", b":1}"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, '{"n":1}\n')
        self.assertEqual(result.stderr, "HTTP 200\n")
        self.assertEqual(request[1], b'{"input":1}')

    def test_chunked_unary_and_stream_framing(self):
        payload = b'{"n":1}'
        envelope = b"\x00" + struct.pack(">I", len(payload)) + payload
        for command, body in (("rpc", payload), ("stream", envelope)):
            with self.subTest(command=command):
                wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n" + f"{len(body):x}\r\n".encode() + body + b"\r\n0\r\nX-Test: yes\r\n\r\n"
                result, request = self.call([wire[i:i + 3] for i in range(0, len(wire), 3)], command)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, '{"n":1}\n')
                if command == "stream":
                    self.assertIn(b"Content-Type: application/connect+json", request[0])
                    self.assertEqual(request[1], b"\x00\x00\x00\x00\x0b" + b'{"input":1}')

    def test_eof_rejects_truncated_response(self):
        for wire in (b"HTTP/1.", b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{"):
            with self.subTest(wire=wire):
                result, _ = self.call([wire])
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertNotIn("Traceback", result.stderr)

    def test_deadline_in_status_headers_body_and_stream(self):
        for command, wire, expected in (
            ("rpc", b"HTTP/1.", 2),
            ("rpc", b"HTTP/1.1 200 OK\r\nContent-Len", 2),
            ("rpc", b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{", 2),
            ("rpc", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{", 2),
            ("stream", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n", 3),
        ):
            with self.subTest(command=command, wire=wire):
                result, _ = self.call([wire], command, hold=True)
                self.assertEqual(result.returncode, expected, result.stderr)


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="verify-helper-test-")
        self.addCleanup(self.temp.cleanup)
        self.run = Path(self.temp.name)
        for name in ("state", "runtime", "evidence"):
            (self.run / name).mkdir()
        (self.run / "runtime" / "connection.json").write_text("{}")
        (self.run / "evidence" / "keep").write_text("evidence")

    @contextlib.contextmanager
    def owned(self, ignore_term=False):
        code = "import signal,time; " + ("signal.signal(signal.SIGTERM, signal.SIG_IGN); " if ignore_term else "") + "print('ready', flush=True); time.sleep(30)"
        with subprocess.Popen([sys.executable, "-c", code], stdout=subprocess.PIPE, text=True) as process:
            self.assertEqual(process.stdout.readline().strip(), "ready")
            fields = Path(f"/proc/{process.pid}/stat").read_text().rsplit(")", 1)[1].split()
            identity = {"pid": process.pid, "starttime": int(fields[19])}
            (self.run / "host.pid").write_text(str(process.pid))
            (self.run / "host.identity").write_text(json.dumps(identity))
            try:
                yield process, identity
            finally:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=2)
                process.stdout.close()

    def host(self, command="stop", script=None, extra_env=None):
        args = ["bash", str(SCRIPTS / "host.sh"), command]
        if script:
            args = ["bash", "-c", 'source "$1" path >/dev/null; ' + script, "fixture", str(SCRIPTS / "host.sh")]
        return subprocess.run(args, env={**os.environ, "VERIFY_RUN_DIR": str(self.run), **(extra_env or {})}, capture_output=True, text=True, timeout=3)

    def test_wrong_starttime_refuses_stop_and_restart(self):
        with self.owned() as (process, identity):
            identity["starttime"] += 1
            (self.run / "host.identity").write_text(json.dumps(identity))
            for command in ("stop", "restart"):
                with self.subTest(command=command):
                    script = "build() { :; }; restart" if command == "restart" else None
                    result = self.host(command, script=script)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIsNone(process.poll())
                    self.assertTrue((self.run / "runtime" / "connection.json").exists())
                    self.assertTrue((self.run / "host.pid").exists())

    def test_changed_pid_and_missing_identity_refuse_signal(self):
        with self.owned() as (process, identity):
            (self.run / "host.pid").write_text(str(os.getpid()))
            self.assertNotEqual(self.host().returncode, 0)
            self.assertIsNone(process.poll())
            (self.run / "host.pid").write_text(str(process.pid))
            (self.run / "host.identity").unlink()
            self.assertNotEqual(self.host().returncode, 0)
            self.assertIsNone(process.poll())

    def test_stop_verifies_exit_and_removes_discovery_dirs(self):
        with self.owned() as (process, _):
            result = self.host()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(process.wait(timeout=1), -signal.SIGTERM)
            for name in ("state", "runtime", "host.pid", "host.identity"):
                self.assertFalse((self.run / name).exists(), name)
            self.assertTrue((self.run / "evidence" / "keep").exists())
        self.assertEqual(self.host().returncode, 0)

    def test_kill_escalation_uses_same_owned_process(self):
        process_helper = load("process")
        with self.owned(ignore_term=True) as (process, _):
            process_helper.terminate(self.run, timeout=0.03)
            self.assertEqual(process.wait(timeout=1), -signal.SIGKILL)

    def test_restart_keeps_state_and_removes_stale_discovery(self):
        with self.owned() as (process, _):
            result = self.host(script='build() { :; }; launch() { test -d "$run/state" && test ! -e "$connection"; }; restart')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(process.wait(timeout=1), -signal.SIGTERM)
            self.assertTrue((self.run / "state").exists())

    def test_failed_launch_reaps_host_and_removes_discovery(self):
        fixture = self.run / "host-fixture"
        fixture.write_text(f"#!{sys.executable}\nimport time\ntime.sleep(30)\n")
        fixture.chmod(0o700)
        result = self.host(script='binary="$run/host-fixture"; wait_ready() { cp "$identity_file" "$run/launched.identity"; return 1; }; launch')
        self.assertNotEqual(result.returncode, 0)
        identity = json.loads((self.run / "launched.identity").read_text())
        self.assertFalse(Path(f'/proc/{identity["pid"]}').exists())
        self.assertFalse((self.run / "host.pid").exists())
        self.assertFalse((self.run / "host.identity").exists())
        self.assertFalse((self.run / "runtime" / "connection.json").exists())

    def test_capture_failure_terminates_only_owned_child(self):
        with self.owned() as (process, _):
            (self.run / "host.identity").unlink()
            (self.run / "host.identity").mkdir()
            result = subprocess.run([sys.executable, str(SCRIPTS / "process.py"), "capture", str(self.run), str(process.pid)], capture_output=True, text=True, timeout=2)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(process.wait(timeout=1), -signal.SIGTERM)

    def test_capture_records_owned_child_and_rejects_parent(self):
        with self.owned() as (process, identity):
            args = [sys.executable, str(SCRIPTS / "process.py"), "capture", str(self.run)]
            result = subprocess.run(args + [str(process.pid)], capture_output=True, text=True, timeout=2)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads((self.run / "host.identity").read_text()), identity)
            result = subprocess.run(args + [str(os.getpid())], capture_output=True, text=True, timeout=2)
            self.assertNotEqual(result.returncode, 0)
            self.assertIsNone(process.poll())

    def test_already_exited_host_can_be_cleaned(self):
        with self.owned() as (process, _):
            process.terminate()
            process.wait(timeout=1)
            result = self.host()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse((self.run / "runtime").exists())

    def test_cleanup_failure_is_not_success(self):
        for script in ('rm() { return 1; }; stop', 'rm() { :; }; stop'):
            with self.subTest(script=script):
                result = self.host(script=script)
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue((self.run / "state").exists())


if __name__ == "__main__":
    unittest.main()
