#!/usr/bin/env python3
"""Measure the real release renderer, keeping the chosen compositor and GPU visible in results."""
import json
import os
from pathlib import Path
import subprocess
import threading
import time

root = Path(__file__).resolve().parents[1]
exe = root / "target/release/ene-avatar"
settings = dict(model=str(Path(os.environ.get("XDG_CACHE_HOME", Path.home()/".cache"))/"ene/models/Seed-san.vrm"), visible=True, fps=30, right=24, bottom=24, output=None)
events=[]
with (root/"target/avatar-measure-stderr.log").open("w") as stderr:
    child=subprocess.Popen([str(exe)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=stderr,text=True)
    def read():
        for line in child.stdout: events.append(json.loads(line))
    threading.Thread(target=read,daemon=True).start()
    def send():
        positions=[e for e in events if e["type"]=="position_changed"]
        if positions:settings["output"]=positions[-1]["output"]
        child.stdin.write(json.dumps(dict(type="configure",settings=settings))+"\n");child.stdin.flush()
    def wait_ready(previous=0):
        start=time.monotonic()
        while sum(e["type"]=="ready" for e in events)<=previous:
            assert child.poll() is None,events
            assert not any(e["type"]=="error" for e in events),events
            assert time.monotonic()-start<90,events
            time.sleep(0.05)
    def usage():
        stats=Path(f"/proc/{child.pid}/stat").read_text().rsplit(")",1)[1].split()
        return (int(stats[11])+int(stats[12]))/os.sysconf("SC_CLK_TCK")
    def measure(name):
        time.sleep(1)
        cpu=usage();start=time.monotonic();pss=[]
        for _ in range(20):
            lines=Path(f"/proc/{child.pid}/smaps_rollup").read_text().splitlines()
            pss.append(next(int(line.split()[1]) for line in lines if line.startswith("Pss:")))
            time.sleep(0.25)
        return dict(state=name,seconds=round(time.monotonic()-start,2),cpu_percent=round((usage()-cpu)/(time.monotonic()-start)*100,2),mean_pss_mib=round(sum(pss)/len(pss)/1024,2))
    try:
        send();wait_ready()
        rows=[measure("visible_30fps")]
        settings["fps"]=60;send();rows.append(measure("visible_60fps"))
        motion=os.environ.get("ENE_AVATAR_MEASURE_MOTION")
        if motion:
            previous=sum(e["type"]=="ready" for e in events)
            event_start=len(events)
            settings.update(motion=motion,motion_playing=True,motion_loop=True,fps=30)
            send();wait_ready(previous)
            assert not any(e["type"]=="motion_error" for e in events[event_start:]),events
            assert any(e["type"]=="motion_state" and e["state"]=="playing" for e in events[event_start:]),events
            rows.append(measure("vrma_30fps"))
            settings["fps"]=60;send();rows.append(measure("vrma_60fps"))
        settings["visible"]=False;send()
        start=time.monotonic()
        while not any(e["type"]=="visibility_changed" and not e["visible"] for e in events):
            assert time.monotonic()-start<5;time.sleep(0.05)
        rows.append(measure("hidden"))
        child.stdin.write('{"type":"shutdown"}\n');child.stdin.flush();assert child.wait(timeout=5)==0
        result=dict(renderer="Bevy 0.19.1 / bevy_vrm1 0.9.5",model="Seed-san, pinned SHA-256 in licenses/Seed-san.md",environment="isolated Sway headless / Mesa Lavapipe CPU / Vulkan; hardware GPU performance is unmeasured",scope="ene-avatar process only; excludes UI/host/voice engine",presentation="offscreen GPU / maximum 4 readbacks / shm pixels and input in one commit",fps="requested update limits; actual presentation FPS unmeasured",release_binary_mib=round(exe.stat().st_size/1024/1024,2),gpu_memory="unmeasured (software GPU)",measurements=rows)
        print(json.dumps(result,ensure_ascii=False,indent=2))
    finally:
        if child.poll() is None:child.kill();child.wait()
