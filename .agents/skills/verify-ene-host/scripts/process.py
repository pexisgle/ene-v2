#!/usr/bin/env python3
"""Linux process identity checks for host.sh. Never signal a bare PID."""
import json
import os
from pathlib import Path
import select
import signal
import sys


def process_stat(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(fields[1]), int(fields[19])


def capture(run, pid):
    fd = os.pidfd_open(pid)
    try:
        parent, starttime = process_stat(pid)
        if parent != os.getppid():
            raise ValueError("host is not a child of the launching shell")
        try:
            (run / "host.identity").write_text(json.dumps({"pid": pid, "starttime": starttime}))
            (run / "host.pid").write_text(f"{pid}\n")
        except OSError:
            terminate_fd(fd, 15)
            raise
    finally:
        os.close(fd)


def open_owned(run):
    pid = int((run / "host.pid").read_text())
    identity = json.loads((run / "host.identity").read_text())
    if pid <= 1 or pid != identity["pid"]:
        raise ValueError("host PID does not match its recorded identity")
    try:
        fd = os.pidfd_open(pid)
    except ProcessLookupError:
        return None
    try:
        try:
            _, starttime = process_stat(pid)
        except FileNotFoundError:
            os.close(fd)
            return None
        if starttime != identity["starttime"]:
            raise ValueError("host starttime does not match its recorded identity")
        return fd
    except Exception:
        os.close(fd)
        raise


def terminate(run, timeout=15):
    fd = open_owned(run)
    if fd is None:
        return
    try:
        terminate_fd(fd, timeout)
    finally:
        os.close(fd)


def terminate_fd(fd, timeout):
    poller = select.poll()
    poller.register(fd, select.POLLIN)
    if poller.poll(0):
        return
    # The retained pidfd pins the checked identity across TERM and KILL.
    try:
        signal.pidfd_send_signal(fd, signal.SIGTERM)
        if not poller.poll(int(timeout * 1000)):
            signal.pidfd_send_signal(fd, signal.SIGKILL)
            if not poller.poll(5000):
                raise TimeoutError("host did not exit after SIGKILL")
    except ProcessLookupError:
        if not poller.poll(5000):
            raise TimeoutError("host exit could not be verified")


def main():
    command, directory, *args = sys.argv[1:]
    run = Path(directory)
    if command == "capture":
        capture(run, int(args[0]))
    elif command == "terminate":
        terminate(run)
    elif command == "alive":
        fd = open_owned(run)
        if fd is None:
            return 1
        try:
            poller = select.poll()
            poller.register(fd, select.POLLIN)
            return 1 if poller.poll(0) else 0
        finally:
            os.close(fd)
    else:
        raise ValueError(f"unknown process command: {command}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError) as error:
        sys.stderr.write(f"host process: {error}\n")
        sys.exit(1)
