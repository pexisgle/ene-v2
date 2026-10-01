#!/usr/bin/env python3
"""Real animated VRM frames + atomic silhouette input on an isolated headless Sway."""
import json
import os
from pathlib import Path
import subprocess
import threading
import time
from PIL import Image

assert os.environ.get('ENE_AVATAR_TEST_PRIVATE_COMPOSITOR')=='1'
root=Path(__file__).resolve().parents[1]
swaymsg=os.environ.get('ENE_SWAYMSG','swaymsg')
outputs=json.loads(subprocess.check_output([swaymsg,'-t','get_outputs'],text=True))
assert all(o['name'].startswith('HEADLESS-') for o in outputs)
assert [(o['rect']['width'],o['rect']['height'],o['scale']) for o in outputs if o['active']]==[(1280,900,1)]
pointer=subprocess.Popen([str(root/'target/debug/examples/test_pointer')],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
assert pointer.stdout.readline().strip()=='ready'
settings=dict(model=str(Path(os.environ.get('XDG_CACHE_HOME',Path.home()/'.cache'))/'ene/models/Seed-san.vrm'),motion=str(root/'target/vrma-fixtures/anchor-and-silhouette.vrma'),motion_playing=True,motion_loop=True,visible=True,fps=30,right=24,bottom=24,output="HEADLESS-1")
events=[]
condition=threading.Condition()
def wait(predicate,timeout=90):
    deadline=time.monotonic()+timeout
    with condition:
        while not predicate(events):
            assert child.poll() is None,log.read_text()
            assert not any(e['type']=='error' for e in events),events
            assert time.monotonic()<deadline,events
            condition.wait(0.05)
def input_event(kind,x=None,y=None):
    pointer.stdin.write(json.dumps(dict(type=kind,x=x,y=y))+'\n');pointer.stdin.flush()
    assert pointer.stdout.readline().strip()=='done'
    time.sleep(0.05)
def count(kind):return sum(e['type']==kind for e in events)
def send():
    child.stdin.write(json.dumps(dict(type='configure',settings=settings))+'\n');child.stdin.flush()
def screenshot(label):
    path=root/f'target/vrma-{label}.png'
    subprocess.run([os.environ.get('ENE_GRIM','grim'),str(path)],check=True)
    return Image.open(path).convert('RGB').crop((896,396,1256,876))
def pause():
    previous_event=len(events)
    settings['motion_playing']=False;send()
    wait(lambda es: any(e['type']=='motion_state' and e['state']=='paused' for e in es[previous_event:]),10)
    time.sleep(0.6)  # settle readback / Wayland transaction after the pause event

log=root/'target/vrma-wayland-stderr.log'
with log.open('w') as stderr:
    child=subprocess.Popen([str(root/'target/debug/ene-avatar')],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=stderr,text=True)
    def read():
        for line in child.stdout:
            event=json.loads(line)
            with condition:
                events.append(event);print(json.dumps(event,ensure_ascii=False),flush=True);condition.notify_all()
    threading.Thread(target=read,daemon=True).start()
    try:
        send();wait(lambda es:any(e['type']=='ready' for e in es))
        wait(lambda es:any(e['type']=='motion_state' and e['state']=='playing' for e in es))
        samples=[]
        # Deliberately sample different live phases; no assumptions about the
        # software adapter's frame rate or GPU readback latency.
        for i in range(5):
            previous_event=len(events)
            pause()
            samples.append(screenshot(f'pose-{i}'))
            if i!=4:
                previous_event=len(events)
                settings['motion_playing']=True;send()
                wait(lambda es:any(e['type']=='motion_state' and e['state']=='playing' for e in es[previous_event:]),10)
                time.sleep(0.37+i*0.11)
        bg=samples[-1].getpixel((5,5))
        current=samples[-1]
        def strong(image,x,y):return max(abs(a-b) for a,b in zip(image.getpixel((x,y)),bg))>60
        old_only=[(x,y) for y in range(80,400) for x in range(20,330) if current.getpixel((x,y))==bg and any(strong(im,x,y) for im in samples[:-1])]
        now=[(x,y) for y in range(80,400) for x in range(20,330) if all(strong(current,x+dx,y+dy) for dx,dy in [(0,0),(-1,0),(1,0),(0,-1),(0,1)])]
        assert len(old_only)>30,'no moving silhouette / old-arm transparent point'
        assert now,'no current opaque point'
        before=count('show_chat')
        x,y=old_only[len(old_only)//2]
        input_event('set',896+x,396+y);input_event('press');input_event('release');time.sleep(0.3)
        assert count('show_chat')==before,'old moving silhouette intercepted click'
        x,y=now[len(now)//2]
        input_event('set',896+x,396+y);input_event('press');input_event('release')
        wait(lambda es:count('show_chat')==before+1,5)
        assert not any(e['type']=='position_changed' and (e['right'],e['bottom'])!=(24,24) for e in events),'motion moved the placement anchor'
        input_event('press');input_event('move',-70,-50);input_event('release')
        wait(lambda es:any(e['type']=='position_changed' and (e['right'],e['bottom'])==(94,74) for e in es),5)
        settings.update(right=94,bottom=74,output='HEADLESS-1')
        # Switch to one-shot: it must finish, rather than silently loop.
        previous_event=len(events);previous_ready=count('ready')
        settings.update(motion_playing=True,motion_loop=False);send()
        wait(lambda es:count('ready')>previous_ready)
        wait(lambda es:any(e['type']=='motion_state' and e['state']=='finished' for e in es[previous_event:]),15)
        # A bad VRMA falls back to the avatar's rest pose; model/UI remain usable.
        previous_event=len(events);previous_ready=count('ready')
        settings['motion']=str(root/'target/vrma-fixtures/external.vrma');send()
        wait(lambda es:any(e['type']=='motion_error' for e in es[previous_event:]))
        wait(lambda es:count('ready')>previous_ready)
        previous_ready=count('ready')
        settings.update(motion=str(root/'target/vrma-fixtures/anchor-and-silhouette.vrma'),motion_loop=True);send()
        wait(lambda es:count('ready')>previous_ready)
        # Animated high DPI and hide/show also recreate valid input regions.
        subprocess.run([swaymsg,'output HEADLESS-1 mode 2560x1800; output HEADLESS-1 scale 2'],check=True,capture_output=True)
        time.sleep(2)
        subprocess.run([os.environ.get('ENE_GRIM','grim'),str(root/'target/vrma-dpi2.png')],check=True)
        settings['visible']=False;send();wait(lambda es:any(e['type']=='visibility_changed' and not e['visible'] for e in es),5)
        previous_ready=count('ready');settings['visible']=True;send();wait(lambda es:count('ready')>previous_ready)
        child.stdin.close();assert child.wait(timeout=5)==0
        print(f'PASS: live VRMA, changing silhouette ({len(old_only)} vacated pixels), old-arm click-through, current click, fixed placement, drag, one-shot, invalid motion/recovery, animated DPI, hide/show, IPC exit')
    finally:
        if child.poll() is None:child.kill();child.wait()
        pointer.stdin.close();pointer.wait(timeout=5)
        subprocess.run([swaymsg,'output HEADLESS-1 scale 1; output HEADLESS-1 mode 1280x900'],check=True,capture_output=True)
