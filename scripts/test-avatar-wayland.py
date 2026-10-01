#!/usr/bin/env python3
"""Real renderer/Wayland integration, on an explicitly isolated headless Sway only."""
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
from PIL import Image

if os.environ.get("ENE_AVATAR_TEST_PRIVATE_COMPOSITOR") != "1":
    raise SystemExit("Use an isolated headless Sway and ENE_AVATAR_TEST_PRIVATE_COMPOSITOR=1; this test moves its pointer.")
SWAYMSG = os.environ.get("ENE_SWAYMSG", "swaymsg")
GRIM = os.environ.get("ENE_GRIM", "grim")

def sway(command):
    result = subprocess.run([SWAYMSG, command], check=True, capture_output=True, text=True)
    assert all(r.get("success") for r in json.loads(result.stdout)), result.stdout

outputs = json.loads(subprocess.check_output([SWAYMSG, "-t", "get_outputs"], text=True))
assert all(o["name"].startswith("HEADLESS-") for o in outputs), "Refusing a physical compositor"
root = Path(__file__).resolve().parents[1]
model = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "ene/models/Seed-san.vrm"
executable = Path(os.environ.get("ENE_AVATAR_BIN", root / "target/debug/ene-avatar"))
settings = dict(model=str(model), visible=True, fps=30, right=24, bottom=24, output=None)
pointer = subprocess.Popen([str(root / "target/debug/examples/test_pointer")], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
assert pointer.stdout.readline().strip() == "ready"
def input_event(kind, x=None, y=None):
    pointer.stdin.write(json.dumps(dict(type=kind, x=x, y=y)) + "\n")
    pointer.stdin.flush()
    assert pointer.stdout.readline().strip() == "done"
    time.sleep(0.05)
events = []
condition = threading.Condition()
errors = root / "target/avatar-wayland-stderr.log"

with errors.open("w") as stderr:
    child = subprocess.Popen([str(executable)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True)
    def read_events():
        for line in child.stdout:
            event = json.loads(line)
            with condition:
                events.append(event)
                print(json.dumps(event, ensure_ascii=False), flush=True)
                condition.notify_all()
    reader = threading.Thread(target=read_events, daemon=True)
    reader.start()
    def send():
        child.stdin.write(json.dumps(dict(type="configure", settings=settings)) + "\n")
        child.stdin.flush()
    def wait(predicate, timeout=90):
        deadline = time.monotonic() + timeout
        with condition:
            while not predicate(events):
                assert not any(e["type"] == "error" for e in events), events
                assert child.poll() is None, errors.read_text()
                remaining = deadline - time.monotonic()
                assert remaining > 0, events
                condition.wait(min(remaining, 0.5))
    def clicks():
        return sum(e["type"] == "show_chat" for e in events)
    try:
        send()
        wait(lambda es: any(e["type"] == "ready" for e in es))
        time.sleep(0.4)  # allow the rendered alpha region to reach the compositor
        # The standalone screenshot was rendered at 360x480; use its alpha to
        # choose an actual opaque interior pixel, rather than a guessed bounding box.
        image = Image.open(root / "target/avatar-seed.png").convert("RGBA")
        assert image.size == (360, 480)
        opaque = [(x,y) for y in range(160,300) for x in range(100,260) if image.getpixel((x,y))[3] > 128]
        assert opaque
        x,y = opaque[len(opaque)//2]
        origin = (1280-360-24, 900-480-24)
        # A transparent pixel inside the layer rectangle must pass through.
        assert image.getpixel((5,5))[3] == 0
        before = clicks()
        input_event("set", origin[0]+5, origin[1]+5)
        input_event("press")
        input_event("release")
        time.sleep(0.3)
        assert clicks() == before
        input_event("set", origin[0]+x, origin[1]+y)
        input_event("press")
        input_event("release")
        wait(lambda es: clicks() == before+1, 5)
        # Drag changes margins, and emits placement rather than an accidental click.
        input_event("press")
        input_event("move", -80, -60)
        input_event("release")
        wait(lambda es:any(e["type"] == "position_changed" and e["right"]==104 and e["bottom"]==84 for e in es),5)
        assert clicks() == before+1
        settings.update(right=104,bottom=84,output="HEADLESS-1")
        subprocess.run([GRIM,str(root / "target/avatar-desktop.png")],check=True)
        settings["visible"] = False
        send()
        wait(lambda es:any(e["type"]=="visibility_changed" and not e["visible"] for e in es),5)
        # Hiding stops GPU updates and removes the entire surface, not just its input.
        time.sleep(0.3)
        subprocess.run([GRIM,str(root / "target/avatar-hidden.png")],check=True)
        ready_before = sum(e["type"]=="ready" for e in events)
        settings["visible"] = True
        settings["fps"] = 60
        send()
        wait(lambda es:sum(e["type"]=="ready" for e in es)>ready_before)
        sway("output HEADLESS-1 mode 2560x1800; output HEADLESS-1 scale 2")
        time.sleep(2)
        subprocess.run([GRIM,str(root / "target/avatar-dpi2.png")],check=True)
        sway("output HEADLESS-1 mode 1600x1125; output HEADLESS-1 scale 1.25")
        time.sleep(1)
        subprocess.run([GRIM,str(root / "target/avatar-dpi125.png")],check=True)
        sway("output HEADLESS-1 scale 1; output HEADLESS-1 mode 1280x900")
        # Replace the active output, then verify that the model is rebuilt on
        # the remaining output rather than stranded on the removed wl_output.
        known={o["name"] for o in json.loads(subprocess.check_output([SWAYMSG,"-t","get_outputs"],text=True))}
        sway("create_output")
        new_outputs=json.loads(subprocess.check_output([SWAYMSG,"-t","get_outputs"],text=True))
        replacement=next(o["name"] for o in new_outputs if o["name"] not in known)
        ready_before=sum(e["type"]=="ready" for e in events)
        sway("output HEADLESS-1 disable")
        wait(lambda es:any(e["type"]=="position_changed" and e["output"]==replacement for e in es) and sum(e["type"]=="ready" for e in es)>ready_before)
        sway("output HEADLESS-1 enable")
        sway(f"output {replacement} disable")
        ready_before=sum(e["type"]=="ready" for e in events)
        wait(lambda es:sum(e["type"]=="ready" for e in es)>ready_before)
        # Parent abnormal termination closes stdin. Child must exit without a shutdown request.
        start = time.monotonic()
        child.stdin.close()
        assert child.wait(timeout=5) == 0
        print(f"PASS: transparent click-through, click, drag, hide/show, DPI request, output replacement, IPC EOF exit ({time.monotonic()-start:.3f}s)")
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        sway("output HEADLESS-1 enable; output HEADLESS-1 scale 1; output HEADLESS-1 mode 1280x900")

pointer.stdin.close()
pointer.wait(timeout=5)
