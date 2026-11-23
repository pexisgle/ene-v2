# Coverage: avatar shutdown

Property: **the avatar renderer terminates cleanly from every shutdown path the
desktop client uses, including before the Wayland handshake finishes.**

The harm if it breaks: quitting the app, or losing the desktop client, leaves a
renderer that panics or stops responding. A panic exits 101, which the client
reports as a renderer crash rather than a clean shutdown, and it can leave a
stale window on screen.

## What `run.sh` proves

It starts the real `ene-avatar` binary with no mocked compositor and drives the
protocol's two shutdown paths: a `quit` command and stdin closing. Each runs
twice, once during the Wayland init handshake and once with the render loop
running.

| Check | Code it describes |
| --- | --- |
| exit status is 0 | `control::run`, `platform::wayland::run` |
| no panic on stderr | the `CompositorProvide` arm returning before the frame body |
| exit within two seconds | `control::run`'s teardown budget and channel closure |

It found a real defect when it was written. `exwlshellev`'s init loop accepts
only `None`, `RequestBind` or `RequestCompositor` and panics on anything else,
while `platform::wayland::run` answered `CompositorProvide` by running a frame
and could return `RequestExit` before the first dispatch. Every quit that
arrived during the handshake therefore panicked. `no panic on stderr` for the
`-before-first-frame` cases is the check that catches it.

The same script refutes a suspected two-second hang. `App::run` moves the app
into its runner, so the `Control` resource holding the output sender is dropped
with it, and a clean exit is immediate. `exits promptly` pins that, so the
hypothesis cannot silently return.

## What it does not cover

- It needs a display and a working Vulkan device. On a machine without one the
  renderer cannot start at all and this script is not the right check.
- It does not verify what is drawn, or that a loaded model renders. The
  `avatar-integration` checks cover the protocol and the scene.
- It does not cover the Windows or macOS runners.

## Running it

```sh
verification/avatar-shutdown/run.sh
```
