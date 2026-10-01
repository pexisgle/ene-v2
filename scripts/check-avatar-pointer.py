#!/usr/bin/env python3
"""Exercise production avatar clicks/drag through a temporary Linux uinput pointer.

Requires writable /dev/uinput and an unlocked Wayland desktop. Clicks are sent
only after this avatar's own wl_pointer.enter confirms focus. Transparent
coverage is probed by movement/leave, without clicking another application.
No saved settings are changed. Not a performance benchmark.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import threading
import time


class Pointer:
    def __init__(self, width, height):
        self.fd = os.open('/dev/uinput', os.O_WRONLY | os.O_NONBLOCK)
        self.width, self.height = width, height
        for event in (1, 3):
            fcntl.ioctl(self.fd, 0x40045564, event)
        fcntl.ioctl(self.fd, 0x40045565, 0x110)
        fcntl.ioctl(self.fd, 0x4004556e, 0)  # INPUT_PROP_POINTER
        for axis, maximum in [(0, width-1), (1, height-1)]:
            fcntl.ioctl(self.fd, 0x40045567, axis)
            fcntl.ioctl(self.fd, 0x401c5504, struct.pack('HH6i', axis, 0, 0, 0, maximum, 0, 0, 1))
        fcntl.ioctl(self.fd, 0x405c5503, struct.pack('HHHH80sI', 3, 1, 2, 1, b'ene avatar pointer verification', 0))
        fcntl.ioctl(self.fd, 0x5501)
        time.sleep(.35)
    def events(self, events):
        data = b''.join(struct.pack('llHHi', 0, 0, *event) for event in [*events, (0, 0, 0)])
        os.write(self.fd, data)
        time.sleep(.07)
    def move(self, x, y):
        self.events([(3, 0, max(0, min(self.width-1, round(x)))), (3, 1, max(0, min(self.height-1, round(y))))])
    def button(self, pressed):
        self.events([(1, 0x110, int(pressed))])
    def close(self):
        fcntl.ioctl(self.fd, 0x5502)
        os.close(self.fd)


class Capture:
    def __init__(self, process, directory):
        self.events, self.protocol, self.focus = [], [], False
        self.lock = threading.Lock()
        self.threads = []
        def stdout():
            with (directory / 'stdout.log').open('w') as log:
                for line in process.stdout:
                    log.write(line)
                    log.flush()
                    try:
                        event = json.loads(line)
                    except ValueError:
                        continue
                    with self.lock:
                        self.events.append(event)
        def stderr():
            with (directory / 'wayland.log').open('w') as log:
                for line in process.stderr:
                    log.write(line)
                    log.flush()
                    if re.search(r'wl_pointer#\d+\.(enter|leave|button|motion)\(', line):
                        with self.lock:
                            self.protocol.append(line.strip())
                            if '.enter(' in line:
                                self.focus = True
                            if '.leave(' in line:
                                self.focus = False
        for action in [stdout, stderr]:
            thread = threading.Thread(target=action, daemon=True)
            thread.start()
            self.threads.append(thread)
    def has(self, kind, after=0):
        with self.lock:
            return any(e.get('type') == kind for e in self.events[after:])
    def wait(self, kind, after=0, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.has(kind, after):
                return
            time.sleep(.02)
        raise RuntimeError('missing avatar event: ' + kind)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/ene-avatar'))
    parser.add_argument('--model', type=Path, default=Path.home()/'.cache/ene/models/Seed-san.vrm')
    parser.add_argument('--output', default='HDMI-A-1')
    parser.add_argument('--virtual-width', type=int, default=1920)
    parser.add_argument('--virtual-height', type=int, default=2160)
    parser.add_argument('--output-dir', type=Path, required=True)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=False)
    pointer, process, capture = None, None, None
    report = {'status': 'failed', 'normal_backend': 'gpu', 'profile': False, 'verification_readback': False}
    try:
        pointer = Pointer(args.virtual_width, args.virtual_height)
        # Movement wakes an idle output; never sends a button during startup.
        pointer.move(args.virtual_width*.5, args.virtual_height*.25)
        env = {**os.environ, 'WAYLAND_DEBUG': '1', 'ENE_AVATAR_BACKEND': 'gpu', 'ENE_AVATAR_PROFILE': '0', 'ENE_AVATAR_VERIFY': '0'}
        process = subprocess.Popen([str(args.binary.resolve())], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
        capture = Capture(process, args.output_dir)
        def send(message):
            process.stdin.write(json.dumps(message)+'\n')
            process.stdin.flush()
        settings = {'model': str(args.model.resolve()), 'motion': None, 'motion_playing': False, 'visible': True, 'fps': 60, 'size_percent': 100, 'right': 24, 'bottom': 24, 'output': args.output}
        send({'type': 'configure', 'settings': settings})
        capture.wait('ready', timeout=30)
        offset = len(capture.events)
        send({'type': 'recenter'})
        capture.wait('position_changed', offset)
        time.sleep(.2)
        points = [(0.5, 0.25), (0.5, 0.5)] + [(x, y) for x in [.5,.375,.625,.25,.75] for y in [.2,.3,.4,.5,.6,.7,.8]]
        for nx, ny in points:
            x, y = args.virtual_width*nx, args.virtual_height*ny
            pointer.move(x, y)
            if capture.focus:
                break
        else:
            raise RuntimeError('virtual pointer did not enter avatar; output/device mapping must be checked')
        report['focused_point'] = [x, y]
        offset = len(capture.events)
        pointer.button(True)
        pointer.button(False)
        capture.wait('show_chat', offset)
        report['click'] = 'show_chat received after focused press/release'
        offset = len(capture.events)
        pointer.button(True)
        pointer.move(x+50, y+20)
        pointer.button(False)
        capture.wait('position_changed', offset)
        report['drag'] = [e for e in capture.events[offset:] if e.get('type') == 'position_changed']
        # Return to the centred avatar, then confirm focus leaves at a transparent corner.
        offset = len(capture.events)
        send({'type': 'recenter'})
        capture.wait('position_changed', offset)
        pointer.move(x, y)
        if not capture.focus:
            raise RuntimeError('avatar focus missing after recenter')
        pointer.move(x+300, y)
        if capture.focus:
            pointer.move(x-300, y)
        if capture.focus:
            raise RuntimeError('transparent canvas margin retained avatar pointer focus')
        report['transparent_pointer'] = 'wl_pointer.leave received at a canvas margin; no background click sent'
        report['status'] = 'ok'
    except Exception as error:
        report['error'] = str(error)
    finally:
        if process:
            if process.poll() is None:
                try:
                    process.stdin.write('{"type":"shutdown"}\n')
                    process.stdin.flush()
                    process.wait(timeout=3)
                except Exception:
                    process.terminate()
                    process.wait(timeout=3)
            report['returncode'] = process.returncode
            process.stdin.close()
            if capture:
                for thread in capture.threads:
                    thread.join(timeout=2)
                report['events'] = capture.events
                report['pointer_protocol'] = capture.protocol
        if pointer:
            pointer.button(False)
            pointer.close()
        (args.output_dir/'summary.json').write_text(json.dumps(report, indent=2, ensure_ascii=False)+'\n')
    print(json.dumps({k: v for k,v in report.items() if k not in ('events', 'pointer_protocol')}, ensure_ascii=False))
    return 0 if report['status'] == 'ok' else 1


if __name__ == '__main__':
    raise SystemExit(main())
