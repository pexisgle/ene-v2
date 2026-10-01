#!/usr/bin/env python3
"""Real loopback HTTP/WebSocket lifecycle, isolated runtime/database, no external API."""
import asyncio
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import websockets

ROOT = Path(__file__).resolve().parents[1]


def http(connection, path, token=True):
    headers = {"Authorization": "Bearer " + connection["token"]} if token else {}
    request = urllib.request.Request(connection["endpoint"] + path, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, json.load(error)


def wait_ready(process, descriptor):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError("host exited before ready")
        if descriptor.exists():
            return json.loads(descriptor.read_text())
        time.sleep(0.02)
    raise AssertionError("host startup timed out")


async def main():
    with tempfile.TemporaryDirectory(prefix="ene-host-test-") as folder:
        folder = Path(folder)
        runtime = folder / "runtime"
        state = folder / "state"
        runtime.mkdir(mode=0o700)
        env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime), ENE_HOST_STATE_DIR=str(state))
        # The host foundation and this test never need a provider credential.
        env.pop("OPENROUTER_API_KEY", None)
        descriptor = runtime / "ene-host/connection.json"
        processes = []
        with (folder / "host.log").open("w+") as log:
            def start():
                p = subprocess.Popen([str(ROOT / "target/debug/ene-host")], cwd=ROOT,
                                     env=env, stdout=log, stderr=log)
                processes.append(p)
                return p
            try:
                p = start()
                first = wait_ready(p, descriptor)
                assert urllib.parse.urlparse(first["endpoint"]).hostname == "127.0.0.1"
                assert stat.S_IMODE(descriptor.stat().st_mode) == 0o600
                assert stat.S_IMODE(state.stat().st_mode) == 0o700
                assert stat.S_IMODE((state / "host.sqlite3").stat().st_mode) == 0o600
                status, _ = http(first, "/snapshot", token=False)
                assert status == 401
                status, initial = http(first, "/snapshot")
                assert status == 200
                duplicate = start()
                assert await asyncio.to_thread(duplicate.wait, timeout=5) != 0
                assert json.loads(descriptor.read_text()) == first
                url = first["endpoint"].replace("http:", "ws:") + "/events"
                try:
                    async with websockets.connect(url):
                        raise AssertionError("unauthorized WebSocket accepted")
                except websockets.exceptions.InvalidStatus as error:
                    assert error.response.status_code == 401
                async with websockets.connect(url, additional_headers={"Authorization": "Bearer " + first["token"]}) as ws:
                    ready = json.loads(await asyncio.wait_for(ws.recv(), 5))
                    assert ready["type"] == "ready"
                    assert ready["cursor"] == initial["cursor"]
                    connection_id = ready["connection_id"]
                    status, subscribed = http(first, "/snapshot?connection_id=" + connection_id)
                    assert status == 200 and subscribed["cursor"] == ready["cursor"]
                    await ws.ping()
                deadline = time.monotonic() + 3
                while True:
                    status, _ = http(first, "/snapshot?connection_id=" + connection_id)
                    if status == 409:
                        break
                    assert time.monotonic() < deadline
                    await asyncio.sleep(0.02)
                # A live socket must not hold graceful termination open.
                async with websockets.connect(url, additional_headers={"Authorization": "Bearer " + first["token"]}) as ws:
                    await ws.recv()
                    p.terminate()
                    assert await asyncio.to_thread(p.wait, timeout=5) == 0
                assert not descriptor.exists()
                second_process = start()
                second = wait_ready(second_process, descriptor)
                assert first["token"] != second["token"]
                status, restarted = http(second, "/snapshot")
                assert status == 200
                for field in ["host_id", "environment_id", "companions", "settings"]:
                    assert restarted[field] == initial[field]
                assert restarted["cursor"]["epoch"] != initial["cursor"]["epoch"]
                old = dict(second, token=first["token"])
                assert http(old, "/snapshot")[0] == 401
                second_process.terminate()
                assert await asyncio.to_thread(second_process.wait, timeout=5) == 0
                log.flush()
                log.seek(0)
                assert first["token"] not in log.read()
                print("PASS: private descriptor, loopback auth, singleton, real WebSocket subscription/snapshot/ping, disconnect invalidation, live-socket shutdown, stable IDs, rotating token/epoch")
            finally:
                for process in processes:
                    if process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(timeout=5)


if __name__ == "__main__":
    asyncio.run(main())
