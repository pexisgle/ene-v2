#!/usr/bin/env python3
"""Real Slint client + supervised renderer + real tray calls on a private test bus."""
import ctypes
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
from PIL import Image

assert os.environ.get("ENE_TRAY_TEST_PRIVATE_BUS")=="1", "use dbus-run-session"
assert os.environ.get("ENE_AVATAR_TEST_PRIVATE_COMPOSITOR")=="1", "use a private compositor"
root=Path(__file__).resolve().parents[1]
helper=root/"target/debug/examples/test_tray"
assert ctypes.CDLL(None).prctl(36,1,0,0,0)==0  # adopt/reap the renderer after its parent is killed
watcher=subprocess.Popen([str(helper)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
assert watcher.stdout.readline().strip()=="ready"
pointer=subprocess.Popen([str(root/"target/debug/examples/test_pointer")],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
assert pointer.stdout.readline().strip()=="ready"

def wait(predicate,timeout=15):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        result=predicate()
        if result:return result
        time.sleep(0.05)
    raise AssertionError("condition timed out")

def children(pid):
    # This executor does not expose /proc/<pid>/task/*/children. Use PPID from
    # stat, which also covers children spawned by the supervisor thread.
    result=[]
    for path in Path("/proc").glob("[0-9]*/stat"):
        try:
            fields=path.read_text().rsplit(")",1)[1].split()
            if int(fields[1])==pid:result.append(int(path.parent.name))
        except (FileNotFoundError,ProcessLookupError):pass
    return result

def action(label):
    subprocess.run([str(helper),label],check=True,capture_output=True,text=True)

log=root/"target/avatar-client-stderr.log"
with tempfile.TemporaryDirectory(prefix="ene-client-test-") as directory, log.open("w") as stderr:
    config=Path(directory)/"ene/client.json"
    test_motion=os.environ.get("ENE_CLIENT_TEST_MOTION")
    if test_motion:
        config.parent.mkdir(parents=True)
        model=Path(os.environ.get("XDG_CACHE_HOME",Path.home()/".cache"))/"ene/models/Seed-san.vrm"
        config.write_text(json.dumps(dict(avatar=dict(model=str(model),motion=test_motion,motion_playing=False,motion_loop=True))))
    env=dict(os.environ,XDG_CONFIG_HOME=directory,ENE_AVATAR_DEBUG="1")
    client=None
    renderer_pids=set()
    try:
        client=subprocess.Popen([str(root/"target/debug/ene-desktop"),"--demo"],env=env,stderr=stderr)
        child=wait(lambda:next(iter(children(client.pid)),None))
        renderer_pids.add(child)
        wait(config.exists,30)
        wait(lambda:"avatar event: Ready" in log.read_text(),90)
        if test_motion:
            wait(lambda:"MotionState { state: Paused }" in log.read_text(),15)
            assert json.loads(config.read_text())["avatar"]["motion"]==test_motion
        time.sleep(0.4)  # the compositor has received the rendered input region
        subprocess.run([os.environ.get("ENE_GRIM","grim"),str(root/"target/avatar-client.png")],check=True)
        # Close the normal window through its compositor close request, then
        # click the actual avatar to verify the cross-process ShowChat binding.
        swaymsg=os.environ.get("ENE_SWAYMSG","swaymsg")
        def chat_visible():
            tree=json.loads(subprocess.check_output([swaymsg,"-t","get_tree"],text=True))
            def find(node):
                return node.get("name")=="ene — 会話" or any(find(n) for n in node.get("nodes",[])+node.get("floating_nodes",[]))
            return find(tree)
        subprocess.run([swaymsg,'[title="ene — 会話"] kill'],check=True,capture_output=True)
        wait(lambda:not chat_visible())
        image=Image.open(root/"target/avatar-seed.png").convert("RGBA")
        opaque=[(x,y) for y in range(160,300) for x in range(100,260) if image.getpixel((x,y))[3]>128]
        x,y=opaque[len(opaque)//2]
        for event in [dict(type="set",x=896+x,y=396+y),dict(type="press"),dict(type="release")]:
            pointer.stdin.write(json.dumps(event)+"\n");pointer.stdin.flush();assert pointer.stdout.readline().strip()=="done";time.sleep(0.05)
        wait(chat_visible)
        action("キャラクターを隠す")
        wait(lambda: not json.loads(config.read_text())["avatar"]["visible"])
        action("キャラクターを表示")
        wait(lambda:json.loads(config.read_text())["avatar"]["visible"])
        if test_motion:
            stored=json.loads(config.read_text())["avatar"]
            assert stored["motion"]==test_motion and not stored["motion_playing"] and stored["motion_loop"]
        action("キャラクターを再起動")
        new_child=wait(lambda:next((p for p in children(client.pid) if p!=child),None))
        renderer_pids.add(new_child)
        assert not Path(f"/proc/{child}").exists(),"old renderer not reaped"
        time.sleep(2)
        action("クライアント終了")
        assert client.wait(timeout=5)==0
        assert not Path(f"/proc/{new_child}").exists(),"renderer left behind on explicit quit"
        # Restart and simulate a parent crash; IPC EOF terminates the renderer.
        ready_count=log.read_text().count("avatar event: Ready")
        client=subprocess.Popen([str(root/"target/debug/ene-desktop"),"--demo"],env=env,stderr=stderr)
        child=wait(lambda:next(iter(children(client.pid)),None))
        renderer_pids.add(child)
        wait(lambda:log.read_text().count("avatar event: Ready")>ready_count,90)
        start=time.monotonic()
        client.kill();client.wait(timeout=5)
        def reaped():
            pid,status=os.waitpid(child,os.WNOHANG)
            if pid:assert os.waitstatus_to_exitcode(status)==0
            return bool(pid)
        wait(reaped,5)
        print(f"PASS: real client close/avatar-click/tray hide/show/restart/quit, configuration persistence, child reaping, parent crash IPC exit ({time.monotonic()-start:.3f}s)")
        if test_motion:print("PASS: real client VRMA paused playback and motion configuration persistence")
    finally:
        if client is not None and client.poll() is None:client.kill();client.wait()
        # Reap adopted children even if an assertion fails midway through the
        # test. A failed test must not leave rendering processes behind.
        for pid in renderer_pids:
            deadline=time.monotonic()+5
            while True:
                try:
                    if os.waitpid(pid,os.WNOHANG)[0]:break
                except ChildProcessError:break
                if time.monotonic()>=deadline:
                    try:os.kill(pid,signal.SIGKILL)
                    except ProcessLookupError:pass
                    os.waitpid(pid,0)
                    break
                time.sleep(0.05)
        pointer.stdin.close();pointer.wait(timeout=5)
        watcher.stdin.close();watcher.wait(timeout=5)
