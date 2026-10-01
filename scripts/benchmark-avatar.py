#!/usr/bin/env python3
"""Benchmark ene-avatar via its stdin Configure IPC, without writing client settings.

Default: gpu/geometry/empty/shm, 3 repeats, 10s warmup + 60s measurement
after Ready (14 minutes plus startup and separate verification runs).
Example: scripts/benchmark-avatar.py --smoke --backends gpu --skip-verify
Matrix: --sizes 50 100 200 --scales 1 2 --fps 30 60
Scale overrides require native support for ENE_AVATAR_TEST_SCALE. Reported
telemetry dimensions/scale are retained so an ignored override is detectable.

CPU uses /proc/PID/stat: 100% is one fully occupied core (can exceed 100%).
DRM fdinfo busy-time counters are deduplicated by device/client and reported
per engine, capacity-normalized. No sums across unlike engines. Whole-device
gpu_busy_percent is recorded separately and includes other processes.
See https://www.kernel.org/doc/html/v6.9/gpu/drm-usage-stats.html .
No dependencies beyond Python's standard library. No image capture/readback.
"""

import argparse
import copy
import csv
import itertools
import json
import math
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import sys
import threading
import time


ROOT = Path(__file__).resolve().parents[1]
PREFIX = "ENE_AVATAR_METRIC "
BACKENDS = ("gpu", "geometry", "empty", "shm")
SCHEMA_VERSION = 1


def finite_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def nonnegative_int(value):
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def distribution(values):
    values = sorted(v for v in values if finite_number(v))
    if not values:
        return {"available": False, "samples": 0, "mean": None, "p50": None,
                "p95": None, "p99": None, "min": None, "max": None}
    return {"available": True, "samples": len(values), "mean": statistics.fmean(values),
            "p50": values[math.ceil(len(values) * .50) - 1],
            "p95": values[math.ceil(len(values) * .95) - 1],
            "p99": values[math.ceil(len(values) * .99) - 1],
            "min": values[0], "max": values[-1]}


class Artifacts:
    """Serialize concurrent pipe/resource records to a complete streaming artifact."""
    def __init__(self, directory):
        self.directory = directory
        self.lock = threading.Lock()
        self.raw_path = directory / "raw.jsonl"
        self.file = self.raw_path.open("w", encoding="utf-8")

    def write(self, kind, **fields):
        with self.lock:
            record = {"kind": kind, "received_monotonic_ns": time.monotonic_ns(),
                      "received_unix_ns": time.time_ns(), **fields}
            self.file.write(json.dumps(record, ensure_ascii=False, allow_nan=False) + "\n")
            self.file.flush()
        return record["received_monotonic_ns"]

    def close(self):
        with self.lock:
            self.file.close()


def parse_drm_fdinfo(text, fallback_device=None):
    fields = dict(line.split(":", 1) for line in text.splitlines() if ":" in line)
    fields = {k.strip(): v.strip() for k, v in fields.items()}
    client = fields.get("drm-client-id")
    device = fields.get("drm-pdev") or fallback_device
    if not client or not device or not fields.get("drm-driver"):
        return None
    counters, capacities, memory = {}, {}, {}
    for key, value in fields.items():
        parts = value.split()
        if key.startswith("drm-engine-capacity-"):
            if len(parts) == 1 and parts[0].isdigit() and int(parts[0]) > 0:
                capacities[key.removeprefix("drm-engine-capacity-")] = int(parts[0])
        elif key.startswith("drm-engine-"):
            if len(parts) == 2 and parts[0].isdigit() and parts[1] == "ns":
                counters[key.removeprefix("drm-engine-")] = int(parts[0])
        elif key.startswith(("drm-total-", "drm-memory-", "drm-resident-", "drm-shared-", "drm-active-", "drm-purgeable-")):
            multipliers = {"B": 1, "bytes": 1, "KiB": 1024, "MiB": 1024 ** 2, "GiB": 1024 ** 3}
            if len(parts) in (1, 2) and parts[0].isdigit():
                unit = parts[1] if len(parts) == 2 else "B"
                if unit in multipliers:
                    memory[key.removeprefix("drm-")] = int(parts[0]) * multipliers[unit]
    return {"driver": fields["drm-driver"], "device": device, "client_id": client,
            "engine_ns": counters, "capacity": capacities, "memory_bytes": memory}


def deduplicate_clients(clients):
    unique = {}
    for client in clients:
        key = (client["device"], client["client_id"])
        if key not in unique:
            unique[key] = copy.deepcopy(client)
        else:
            # Duplicate FDs can be read at slightly different instants.
            for engine, value in client["engine_ns"].items():
                unique[key]["engine_ns"][engine] = max(
                    value, unique[key]["engine_ns"].get(engine, 0))
            unique[key]["capacity"].update(client["capacity"])
            for region, value in client["memory_bytes"].items():
                unique[key]["memory_bytes"][region] = max(
                    value, unique[key]["memory_bytes"].get(region, 0))
    return list(unique.values())


def read_drm_clients(pid, proc_root=Path("/proc"), sys_root=Path("/sys")):
    directory = proc_root / str(pid) / "fdinfo"
    clients, errors, raw_fdinfo = [], [], []
    try:
        paths = list(directory.iterdir())
    except OSError as error:
        return {"available": False, "reason": str(error), "clients": []}
    drm_seen = False
    for path in paths:
        try:
            text = path.read_text()
            if "drm-driver:" not in text:
                continue
            drm_seen = True
            raw_fdinfo.append({"fd": path.name, "text": text})
            # Render/card nodes on the same device must share a dedup identity.
            fallback = None
            try:
                node = (proc_root / str(pid) / "fd" / path.name).stat()
                device = sys_root / "dev/char" / f"{os.major(node.st_rdev)}:{os.minor(node.st_rdev)}" / "device"
                if node.st_rdev and device.exists():
                    fallback = str(device.resolve())
            except OSError:
                pass
            client = parse_drm_fdinfo(text, fallback)
            if client:
                clients.append(client)
        except OSError as error:
            errors.append(str(error))
    clients = deduplicate_clients(clients)
    available = any(c["engine_ns"] for c in clients)
    return {"available": available, "clients": clients, "read_errors": errors, "raw_fdinfo": raw_fdinfo,
            "reason": None if available else (
                "DRM FDs lack usable ns counters or device/client identity" if drm_seen
                else "no DRM client ns counters exposed")}


class ResourceSampler:
    def __init__(self, pid, proc_root=Path("/proc"), sys_root=Path("/sys")):
        self.pid, self.proc_root, self.sys_root = pid, proc_root, sys_root
        self.hz = os.sysconf("SC_CLK_TCK")
        self.page_size = os.sysconf("SC_PAGE_SIZE")
        self.previous = None
        self.high_water = {}

    def sample(self):
        now = time.monotonic_ns()
        result = {"monotonic_ns": now, "cpu_one_core_percent": None, "rss_bytes": None,
                  "cpu_ticks": None, "clock_ticks_per_second": self.hz,
                  "process_start_ticks": None, "proc_error": None,
                  "gpu_process_engine_percent": {}, "gpu_process_interval_ns": None,
                  "gpu_process_memory_bytes": {}}
        try:
            text = (self.proc_root / str(self.pid) / "stat").read_text()
            result["raw_proc_stat"] = text.strip()
            # comm may contain spaces and ')'; all remaining fields are numeric
            # except the first state field. Index 0 here is proc stat field 3.
            fields = text[text.rindex(")") + 2:].split()
            result["cpu_ticks"] = int(fields[11]) + int(fields[12])
            result["rss_bytes"] = int(fields[21]) * self.page_size
            result["process_start_ticks"] = int(fields[19])
        except (OSError, ValueError, IndexError) as error:
            result["proc_error"] = str(error)
        drm = read_drm_clients(self.pid, self.proc_root, self.sys_root)
        result["gpu_process"] = drm
        current = {}
        for client in drm["clients"]:
            for region, value in client["memory_bytes"].items():
                label = f"{client['device']}/{region}"
                result["gpu_process_memory_bytes"][label] = (
                    result["gpu_process_memory_bytes"].get(label, 0) + value)
            for engine, ns in client["engine_ns"].items():
                key = (client["device"], client["client_id"], engine)
                # Kernel spec: keep the larger previous counter on regression.
                high = max(ns, self.high_water.get(key, ns))
                self.high_water[key] = high
                current[key] = (high, client["capacity"].get(engine, 1))
        if self.previous:
            prev, previous_counters = self.previous
            dt = now - prev["monotonic_ns"]
            result["sample_interval_ns"] = dt
            if (dt > 0 and result["cpu_ticks"] is not None and prev["cpu_ticks"] is not None
                    and result["process_start_ticks"] == prev["process_start_ticks"]
                    and result["cpu_ticks"] >= prev["cpu_ticks"]):
                result["cpu_one_core_percent"] = (
                    (result["cpu_ticks"] - prev["cpu_ticks"]) / self.hz / (dt / 1e9) * 100)
            capacities = {}
            for (device, client_id, engine), (ns, capacity) in current.items():
                key = (device, client_id, engine)
                if key not in previous_counters or dt <= 0:
                    continue
                old_ns, old_capacity = previous_counters[key]
                if old_capacity != capacity:
                    continue
                label = f"{device}/{engine}"
                capacities.setdefault(label, set()).add(capacity)
                value = (ns - old_ns) / dt * 100 / capacity
                result["gpu_process_engine_percent"][label] = (
                    result["gpu_process_engine_percent"].get(label, 0) + value)
            # Different reported capacities cannot be meaningfully combined.
            for label, caps in capacities.items():
                if len(caps) != 1:
                    result["gpu_process_engine_percent"].pop(label, None)
            if result["gpu_process_engine_percent"]:
                result["gpu_process_interval_ns"] = dt
        self.previous = (result, current)
        busy = {}
        for path in self.sys_root.glob("class/drm/card[0-9]*/device/gpu_busy_percent"):
            try:
                value = int(path.read_text().strip())
                if 0 <= value <= 100:
                    # Deduplicate nodes resolving to the same physical file.
                    busy[str(path.resolve())] = value
            except (OSError, ValueError):
                continue
        result["gpu_whole_device_busy_percent"] = busy
        result["gpu_whole_device_available"] = bool(busy)
        return result


class PipeCapture:
    def __init__(self, process, artifacts, backend, verify):
        self.process, self.artifacts = process, artifacts
        self.backend, self.verify = backend, verify
        self.ready = threading.Event()
        self.failure = threading.Event()
        self.lock = threading.Lock()
        self.metrics, self.errors = [], []
        self.ready_ns = None
        self.invalid_metrics = 0
        self.expect_model_error = False
        self.expected_errors = []
        self.threads = []
        for stream, pipe in (("stdout", process.stdout), ("stderr", process.stderr)):
            thread = threading.Thread(target=self.drain, args=(stream, pipe), daemon=True)
            thread.start()
            self.threads.append(thread)

    def drain(self, stream, pipe):
        try:
            # Raw byte logs preserve stderr noise and every stdout IPC line.
            with (self.artifacts.directory / f"{stream}.log").open("wb") as log:
                for line in iter(pipe.readline, b""):
                    log.write(line)
                    log.flush()
                    text = line.decode("utf-8", errors="replace").rstrip("\r\n")
                    if stream == "stderr" and text.startswith(PREFIX):
                        try:
                            metric = json.loads(text[len(PREFIX):])
                            if (not isinstance(metric, dict)
                                    or metric.get("schema_version") != SCHEMA_VERSION
                                    or metric.get("backend") != self.backend
                                    or metric.get("verify") is not self.verify
                                    or metric.get("pid") != self.process.pid
                                    or not isinstance(metric.get("kind"), str)
                                    or "sample" not in metric):
                                raise ValueError("metric envelope mismatch")
                            stamp = self.artifacts.write("metric", record=metric)
                            with self.lock:
                                self.metrics.append((stamp, metric))
                        except (ValueError, TypeError) as error:
                            self.artifacts.write("invalid_metric", line=text, error=str(error))
                            with self.lock:
                                self.invalid_metrics += 1
                    elif stream == "stdout":
                        try:
                            event = json.loads(text)
                        except ValueError:
                            event = None
                        stamp = self.artifacts.write("ipc", event=event, line=text)
                        if isinstance(event, dict) and event.get("type") == "ready":
                            if self.ready_ns is None:
                                self.ready_ns = stamp
                            self.ready.set()
                        if isinstance(event, dict) and event.get("type") in ("error", "motion_error"):
                            with self.lock:
                                expected = self.expect_model_error and event.get("type") == "error"
                                if expected:
                                    self.expected_errors.append(event)
                                else:
                                    self.errors.append(event)
                            self.artifacts.write("ipc_error_classification", expected=expected, event=event)
                            if not expected:
                                self.failure.set()
        except Exception as error:
            with self.lock:
                self.errors.append({"type": "capture_error", "message": str(error)})
            self.failure.set()
        finally:
            pipe.close()

    def finish(self):
        for thread in self.threads:
            thread.join(timeout=3)
        if any(t.is_alive() for t in self.threads):
            raise RuntimeError("pipe drain failed to finish; no subsequent process will be started")


def send_ipc(process, value):
    message = (json.dumps(value, ensure_ascii=False, allow_nan=False) + "\n").encode()
    if len(message) > 64 * 1024:
        raise ValueError("Configure exceeds the 64 KiB IPC limit")
    process.stdin.write(message)
    process.stdin.flush()


def stop_process(process):
    if process.poll() is None:
        try:
            send_ipc(process, {"type": "shutdown"})
        except (OSError, ValueError):
            pass
    if process.stdin:
        try:
            process.stdin.close()
        except OSError:
            pass
    method = "ipc"
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        method = "sigterm"
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            method = "sigkill"
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=3)
    return method


def display_latency_summary(presentations):
    """Keep clock-aligned display estimates independent of callback upper bounds."""
    names = ("enqueue_to_display_ns_estimate", "enqueue_to_display_ns_lower_bound",
             "enqueue_to_display_ns_upper_bound", "enqueue_to_display_ns_uncertainty")
    accepted, reasons, invalid = [], set(), 0
    for sample in presentations:
        values = [sample.get(name) for name in names]
        if all(value is None for value in values):
            reasons.add(str(sample.get("display_latency_unavailable_reason")
                            or "clock-aligned display latency absent from telemetry"))
            continue
        mapping = sample.get("enqueue_clock_mapping")
        valid = all(nonnegative_int(value) for value in values) and isinstance(mapping, dict)
        if valid:
            estimate, lower, upper, uncertainty = values
            keys = ("clock_id", "enqueue_clock_lower_ns", "enqueue_clock_upper_ns",
                    "calibration_window_ns", "clock_resolution_ns", "registration_age_ns")
            valid = all(nonnegative_int(mapping.get(key)) for key in keys)
            valid = valid and mapping["clock_id"] == sample.get("clock_id") == getattr(time, "CLOCK_MONOTONIC", None)
            valid = valid and isinstance(mapping.get("assumptions"), str) and bool(mapping["assumptions"])
            valid = valid and mapping["clock_resolution_ns"] > 0
            valid = valid and mapping["enqueue_clock_lower_ns"] <= mapping["enqueue_clock_upper_ns"]
            valid = valid and lower <= estimate <= upper and uncertainty == max(estimate - lower, upper - estimate)
            stamp = sample.get("presentation_ns")
            valid = valid and nonnegative_int(stamp) and stamp >= mapping["enqueue_clock_lower_ns"]
            valid = valid and lower == max(0, stamp - mapping["enqueue_clock_upper_ns"])
            valid = valid and upper == stamp - mapping["enqueue_clock_lower_ns"]
        if valid:
            accepted.append(sample)
        else:
            invalid += 1
            reasons.add("invalid display latency bounds or CLOCK_MONOTONIC mapping evidence")
    if not presentations:
        reasons.add("no presented telemetry in measurement window")
    return {"display_latency_available": bool(accepted),
            "display_latency_samples": len(accepted),
            "display_latency_missing_samples": len(presentations) - len(accepted) - invalid,
            "display_latency_invalid_samples": invalid,
            **{name: distribution([s[name] for s in accepted]) for name in names},
            "display_latency_unavailable_reasons": sorted(reasons),
            "display_latency_mapping_assumptions": sorted({s["enqueue_clock_mapping"]["assumptions"] for s in accepted}),
            "display_latency_calibration_window_ns": distribution([
                s["enqueue_clock_mapping"]["calibration_window_ns"] for s in accepted]),
            "display_latency_clock_resolution_ns": distribution([
                s["enqueue_clock_mapping"]["clock_resolution_ns"] for s in accepted]),
            "display_latency_registration_age_ns": distribution([
                s["enqueue_clock_mapping"]["registration_age_ns"] for s in accepted]),
            "display_latency_scope": "original frame enqueue to wp_presentation display timestamp, mapped to Linux CLOCK_MONOTONIC; midpoint estimate and deterministic sampling/resolution bounds conditional on recorded assumptions; does not bound compositor timestamp error, clock drift, or physical photon timing"}


def metric_summary(metrics, start_ns, end_ns, verify):
    eligible = [m for stamp, m in metrics if start_ns is not None and start_ns <= stamp < end_ns]
    cpu, gpu = {}, {}
    presentations, discarded, verification = [], 0, []
    dimensions, gpu_reasons, presentation_reasons = set(), set(), set()
    readback = {"readback_bytes": [], "timestamp_readback_bytes": [], "overflow_retries": []}
    transfers_by_frame = {}
    invalid_verification = 0
    for record in eligible:
        kind, sample = record["kind"], record["sample"]
        if kind == "presentation_unavailable":
            presentation_reasons.add(str(sample))
        if not isinstance(sample, dict):
            continue
        if kind == "cpu_frame":
            width, height, scale = sample.get("logical_width"), sample.get("logical_height"), sample.get("scale")
            if nonnegative_int(width) and nonnegative_int(height) and finite_number(scale):
                dimensions.add((width, height, scale))
            for key, value in sample.items():
                if key.endswith("_ns") and finite_number(value) and value >= 0:
                    cpu.setdefault(key, []).append(value)
        elif kind == "gpu_timestamps":
            period = sample.get("timestamp_period_ns")
            if sample.get("unavailable_reason"):
                gpu_reasons.add(str(sample["unavailable_reason"]))
            if finite_number(period) and period > 0:
                for stage in ("scene_queue_elapsed", "scene_render_batch", "mask", "present_queue_elapsed"):
                    # Legacy schema-v1 logs before the explicit queue label remain readable.
                    span = sample.get(stage, sample.get(stage.removesuffix("_queue_elapsed")))
                    if (isinstance(span, dict) and nonnegative_int(span.get("start_ticks"))
                            and nonnegative_int(span.get("end_ticks"))
                            and span["end_ticks"] >= span["start_ticks"]):
                        duration = (span["end_ticks"] - span["start_ticks"]) * period
                        if finite_number(duration):
                            label = "scene_ns" if stage == "scene_render_batch" else stage + "_ns"
                            gpu.setdefault(label, []).append(duration)
        elif kind == "gpu_durations":
            if sample.get("unavailable_reason"):
                gpu_reasons.add(str(sample["unavailable_reason"]))
            for stage in ("scene", "scene_queue_elapsed", "mask", "copy_mask", "present"):
                value = sample.get(stage + "_ns")
                if finite_number(value) and value >= 0:
                    gpu.setdefault(stage + "_ns", []).append(value)
            for key in readback:
                value = sample.get(key)
                if nonnegative_int(value):
                    counters = transfers_by_frame.setdefault(sample.get("frame_id"), {k: 0 for k in readback})
                    counters[key] += value
            value = sample.get("transport_latency_ns")
            if finite_number(value) and value >= 0:
                gpu.setdefault("transport_latency_ns", []).append(value)
        elif kind == "presented":
            presentations.append(sample)
        elif kind == "discarded":
            discarded += 1
        elif kind == "verification" and verify:
            keys = ("true_positives", "false_positive", "false_negative", "true_negatives",
                    "frame_id", "width", "height")
            valid = all(nonnegative_int(sample.get(k)) for k in keys)
            valid = valid and sample["width"] > 0 and sample["height"] > 0
            valid = valid and sum(sample[k] for k in keys[:4]) == sample["width"] * sample["height"]
            valid = valid and isinstance(sample.get("reference_rule"), str) and bool(sample["reference_rule"])
            valid = valid and finite_number(sample.get("scale")) and sample["scale"] > 0
            if valid:
                verification.append(sample)
            else:
                invalid_verification += 1
    for counters in transfers_by_frame.values():
        for key, value in counters.items():
            readback[key].append(value)
    intervals, previous, resets = [], None, 0
    for sample in presentations:
        stamp, clock = sample.get("presentation_ns"), sample.get("clock_id")
        if not nonnegative_int(stamp) or not nonnegative_int(clock):
            previous = None
            resets += 1
            continue
        if previous:
            old_clock, old_stamp = previous
            if clock == old_clock and stamp > old_stamp:
                intervals.append(stamp - old_stamp)
            else:
                resets += 1
        previous = (clock, stamp)
    seconds = (end_ns - start_ns) / 1e9 if start_ns is not None and end_ns > start_ns else 0
    interval_stats = distribution(intervals)
    presentation = {"available": bool(presentations), "presented": len(presentations),
                    "discarded": discarded, "clock_resets": resets,
                    "fps": len(presentations) / seconds if presentations and seconds else None,
                    "interval_fps": 1e9 / interval_stats["mean"] if interval_stats["available"] else None,
                    "interval_ns": interval_stats,
                    "callback_latency_ns": distribution([p.get("callback_latency_ns") for p in presentations]),
                    "enqueue_to_callback_ns": distribution([p.get("enqueue_to_callback_ns") for p in presentations]),
                    "enqueue_latency_scope": "enqueue-to-callback receipt upper bound; kept separate from clock-aligned display latency",
                    **display_latency_summary(presentations),
                    "unavailable_reasons": sorted(presentation_reasons) if not presentations else [],
                    "fps_source": "wp_presentation callbacks / actual measurement seconds",
                    "interval_source": "consecutive presented timestamps in the same compositor clock"}
    if not presentations and not presentation["unavailable_reasons"]:
        presentation["unavailable_reasons"] = ["no presented telemetry in measurement window"]
    accuracy = {"available": bool(verification), "samples": len(verification),
                "invalid_samples": invalid_verification, "false_positive": None, "false_negative": None,
                "true_positives": None, "true_negatives": None,
                "false_positive_rate": None, "false_negative_rate": None,
                "reference_rules": sorted({v["reference_rule"] for v in verification}),
                "reason": None if verification else "no valid samples from separate verification process"}
    if verification:
        for key in ("true_positives", "false_positive", "false_negative", "true_negatives"):
            accuracy[key] = sum(v[key] for v in verification)
        negative = accuracy["false_positive"] + accuracy["true_negatives"]
        positive = accuracy["false_negative"] + accuracy["true_positives"]
        accuracy["false_positive_rate"] = accuracy["false_positive"] / negative if negative else None
        accuracy["false_negative_rate"] = accuracy["false_negative"] / positive if positive else None
        accuracy["pixels_compared"] = positive + negative
        accuracy["mismatch_frames"] = [{k: v[k] for k in ("frame_id", "width", "height", "scale", "false_positive", "false_negative")}
                                       for v in verification if v["false_positive"] or v["false_negative"]]
    stages = ("scene_ns", "scene_queue_elapsed_ns", "mask_ns", "copy_mask_ns", "present_ns",
              "present_queue_elapsed_ns", "transport_latency_ns")
    gpu_stats = {k: distribution(gpu.get(k, [])) for k in stages}
    if not gpu and not gpu_reasons:
        gpu_reasons.add("no valid GPU timestamp/duration samples in measurement window")
    return {"metric_records": len(eligible), "cpu_frame_available": bool(cpu),
            "cpu_frame_ns": {k: distribution(v) for k, v in cpu.items()},
            "gpu_stage_available": bool(gpu), "gpu_stage_ns": gpu_stats,
            "gpu_timing_scopes": {"scene_queue_elapsed_ns": "queue span around app.update, including idle time/uploads/intervening work",
                                  "scene_ns": "GPU timestamp span within one RenderGraph Submit batch; scene/render work between begin/end buffers",
                                  "present_queue_elapsed_ns": "queue span around presenter draw, including idle/intervening work",
                                  "mask_ns": "GPU mask timestamp duration",
                                  "copy_mask_ns": "GPU copy plus mask timestamp duration",
                                  "present_ns": "GPU present-pass timestamp duration if supplied",
                                  "transport_latency_ns": "CPU monotonic enqueue-to-present-call elapsed; not GPU or display latency"},
            "gpu_timestamp_unavailable_reasons": sorted(gpu_reasons), "presentation": presentation,
            "gpu_transfer": {key: {"total": sum(values) if values else None,
                                   "per_frame": distribution(values)} for key, values in readback.items()},
            "accuracy": accuracy, "observed_dimensions": [list(d) for d in sorted(dimensions, key=str)]}


def exercise_plan(settings, duration, missing_model, output, alternate_model=None):
    """Configure only in-memory state; reserve the final interval for restoration."""
    active = copy.deepcopy(settings)
    actions = []
    def configure(name, **changes):
        active.update(changes)
        actions.append((name, {"type": "configure", "settings": copy.deepcopy(active)}))
    configure("pause", motion_playing=False)
    configure("resume", motion_playing=True)
    changed_size = settings["size_percent"] + (25 if settings["size_percent"] <= 175 else -25)
    configure("size", size_percent=changed_size)
    configure("restore_size", size_percent=settings["size_percent"])
    configure("hide", visible=False)
    configure("show", visible=True)
    if alternate_model:
        configure("alternate_model", model=str(alternate_model), motion=None)
        configure("restore_valid_model", model=settings["model"], motion=settings["motion"])
    configure("bad_model", model=str(missing_model))
    configure("restore_model", model=settings["model"])
    configure("clear_motion", motion=None)
    configure("restore_motion", motion=settings["motion"], motion_playing=settings["motion_playing"])
    configure("output_change", output=output)
    actions.append(("recenter", {"type": "recenter"}))
    actions.append(("restore_settings", {"type": "configure", "settings": copy.deepcopy(settings)}))
    return [(duration * (i + 1) / (len(actions) + 1), name, request)
            for i, (name, request) in enumerate(actions)]


def resource_summary(samples):
    cpu = [(s["cpu_one_core_percent"], s.get("sample_interval_ns", 0)) for s in samples
           if finite_number(s["cpu_one_core_percent"]) and s.get("sample_interval_ns", 0) > 0]
    weight = sum(dt for _, dt in cpu)
    engines, devices, memory = {}, {}, {}
    engine_weights = {}
    for sample in samples:
        for engine, value in sample["gpu_process_engine_percent"].items():
            engines.setdefault(engine, []).append(value)
            engine_weights.setdefault(engine, []).append((value, sample["gpu_process_interval_ns"]))
        for device, value in sample["gpu_whole_device_busy_percent"].items():
            devices.setdefault(device, []).append(value)
        for region, value in sample["gpu_process_memory_bytes"].items():
            memory.setdefault(region, []).append(value)
    engine_stats = {k: distribution(v) for k, v in engines.items()}
    for key, pairs in engine_weights.items():
        engine_stats[key]["time_weighted_mean"] = sum(v * dt for v, dt in pairs) / sum(dt for _, dt in pairs)
    gpu_reasons = sorted({s["gpu_process"]["reason"] for s in samples if s["gpu_process"].get("reason")})
    if not engines and not gpu_reasons:
        gpu_reasons = ["insufficient consecutive DRM ns-counter samples"]
    return {"resource_samples": len(samples), "cpu_one_core_percent": {
                **distribution([v for v, _ in cpu]),
                "time_weighted_mean": sum(v * dt for v, dt in cpu) / weight if weight else None},
            "rss_bytes": distribution([s["rss_bytes"] for s in samples]),
            "gpu_process_available": bool(engines), "gpu_process_engine_percent": engine_stats,
            "gpu_process_unavailable_reasons": gpu_reasons if not engines else [],
            "gpu_process_memory_available": bool(memory),
            "gpu_process_memory_bytes": {k: distribution(v) for k, v in memory.items()},
            "gpu_process_memory_reason": None if memory else "no standardized DRM memory counters with deduplicated client identity",
            "gpu_process_memory_note": "sum of driver-reported per-client region/stat counters after fd dedup; shared allocations across distinct clients may remain counted twice; total/memory/resident are separate views, never added together",
            "gpu_whole_device_available": bool(devices),
            "gpu_whole_device_busy_percent": {k: distribution(v) for k, v in devices.items()},
            "gpu_whole_device_note": "all clients including compositor and unrelated processes; not avatar attribution"}


def run_one(args, settings, backend, repeat, scale, scenario, verify, directory):
    directory.mkdir()
    artifacts = Artifacts(directory)
    env = dict(os.environ)
    env["ENE_AVATAR_BACKEND"] = backend
    env["ENE_AVATAR_PROFILE"] = "1"
    env.pop("ENE_AVATAR_VERIFY", None)
    if verify:
        env["ENE_AVATAR_VERIFY"] = "1"
    if scale is not None:
        env["ENE_AVATAR_TEST_SCALE"] = str(scale)
    warmup = args.verify_warmup if verify else args.warmup
    duration = args.verify_duration if verify else args.duration
    metadata = {"schema_version": SCHEMA_VERSION, "backend": backend, "verify": verify,
                "repeat": repeat, "scenario": scenario, "settings": settings,
                "requested_test_scale": scale, "effective_test_scale_env": env.get("ENE_AVATAR_TEST_SCALE"),
                "warmup_seconds": warmup, "requested_measure_seconds": duration,
                "exercise": args.exercise,
                "command": [str(args.binary)], "started_unix_ns": time.time_ns(),
                "raw_json_path": str(artifacts.raw_path.resolve())}
    write_json(directory / "run.json", metadata)
    artifacts.write("run", metadata=metadata)
    process, capture = None, None
    status, failure, shutdown_method = "failed", None, None
    measure_start, measure_end = None, None
    resources = []
    exercises = exercise_plan(settings, duration, directory / "missing-model.vrm", args.exercise_output, args.exercise_model) if args.exercise else []
    exercise_index = 0
    exercise_actions = []
    try:
        process = subprocess.Popen([str(args.binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, env=env, start_new_session=True)
        metadata["pid"] = process.pid
        capture = PipeCapture(process, artifacts, backend, verify)
        send_ipc(process, {"type": "configure", "settings": settings})
        sampler = ResourceSampler(process.pid)
        startup_deadline = time.monotonic() + args.ready_timeout
        next_sample = 0
        warmup_deadline = None
        while True:
            now = time.monotonic()
            if process.poll() is not None:
                raise RuntimeError(f"avatar exited before measurement completed ({process.returncode})")
            if capture.failure.is_set():
                raise RuntimeError("avatar IPC or pipe capture error; inspect stdout.log/stderr.log")
            if warmup_deadline is None and capture.ready.is_set():
                warmup_deadline = capture.ready_ns / 1e9 + warmup
                artifacts.write("phase", phase="warmup", ready_monotonic_ns=capture.ready_ns)
            if warmup_deadline is None and now >= startup_deadline:
                raise RuntimeError(f"Ready not received within {args.ready_timeout}s")
            if warmup_deadline is not None and now >= warmup_deadline and measure_start is None:
                # Reset interval baselines exactly at the measurement boundary.
                sampler = ResourceSampler(process.pid)
                measure_start = time.monotonic_ns()
                artifacts.write("phase", phase="measure", boundary_monotonic_ns=measure_start)
                next_sample = 0
            if measure_start is not None:
                elapsed = (time.monotonic_ns() - measure_start) / 1e9
                while exercise_index < len(exercises) and elapsed >= exercises[exercise_index][0]:
                    offset, name, request = exercises[exercise_index]
                    if name == "bad_model":
                        with capture.lock:
                            capture.expect_model_error = True
                    send_ipc(process, request)
                    stamp = artifacts.write("exercise", action=name, scheduled_seconds=offset, request=request)
                    exercise_actions.append({"action": name, "scheduled_seconds": offset,
                                             "sent_seconds": (stamp - measure_start) / 1e9,
                                             "request": request})
                    if name == "restore_model":
                        # The invalid Configure and restore are FIFO on the same
                        # IPC stream. Keep a small grace for its asynchronous error.
                        # Clear at the next action below; unrelated errors still fail.
                        pass
                    elif name == "clear_motion":
                        with capture.lock:
                            capture.expect_model_error = False
                    exercise_index += 1
            if now >= next_sample:
                sample = sampler.sample()
                phase = "measure" if measure_start is not None else "warmup" if warmup_deadline else "startup"
                artifacts.write("resource", phase=phase, sample=sample)
                if phase == "measure":
                    resources.append(sample)
                next_sample = now + args.sample_interval
            if measure_start is not None and time.monotonic_ns() >= measure_start + duration * 1e9:
                # Final resource interval is needed for weighted CPU/engine means.
                sample = sampler.sample()
                resources.append(sample)
                artifacts.write("resource", phase="measure", sample=sample)
                measure_end = time.monotonic_ns()
                artifacts.write("phase", phase="complete", boundary_monotonic_ns=measure_end)
                status = "ok"
                break
            time.sleep(min(.05, args.sample_interval))
    except KeyboardInterrupt:
        status, failure = "interrupted", "user interrupted benchmark"
        artifacts.write("failure", message=failure)
    except Exception as error:
        failure = str(error)
        artifacts.write("failure", message=failure)
    finally:
        if measure_end is None:
            measure_end = time.monotonic_ns()
        if process:
            shutdown_method = stop_process(process)
        if capture:
            capture.finish()
        # No next run can launch before the child and both drains have finished.
        artifacts.write("exit", status=status, failure=failure,
                        returncode=process.returncode if process else None, shutdown_method=shutdown_method)
        artifacts.close()
    summary = {**metadata, "status": status, "failure": failure,
               "returncode": process.returncode if process else None, "shutdown_method": shutdown_method,
               "ready_monotonic_ns": capture.ready_ns if capture else None,
               "measure_start_monotonic_ns": measure_start, "measure_end_monotonic_ns": measure_end,
               "actual_measure_seconds": (measure_end - measure_start) / 1e9 if measure_start else 0,
               "invalid_metric_lines": capture.invalid_metrics if capture else 0,
               "ipc_errors": capture.errors if capture else [],
               "exercise_actions": exercise_actions,
               "expected_model_errors": capture.expected_errors if capture else [],
               **resource_summary(resources),
               **metric_summary(capture.metrics if capture else [], measure_start, measure_end, verify)}
    if status == "ok" and process.returncode != 0:
        summary["status"] = "failed"
        summary["failure"] = "avatar returned nonzero during shutdown"
    if status == "ok" and not summary["metric_records"]:
        summary["status"] = "incomplete"
        summary["failure"] = "no profile telemetry; binary may lack integration"
    if status == "ok" and summary["invalid_metric_lines"]:
        summary["status"] = "incomplete"
        summary["failure"] = "invalid or mismatched telemetry envelopes"
    if status == "ok" and not verify and not summary["presentation"]["available"]:
        summary["status"] = "incomplete"
        summary["failure"] = "presentation FPS unavailable"
    if status == "ok" and verify and not summary["accuracy"]["available"]:
        summary["status"] = "incomplete"
        summary["failure"] = "verification telemetry unavailable"
    if status == "ok" and args.exercise and len(exercise_actions) != len(exercises):
        summary["status"] = "incomplete"
        summary["failure"] = "not all lifecycle actions were sent"
    if status == "ok" and args.exercise and not summary["expected_model_errors"]:
        summary["status"] = "incomplete"
        summary["failure"] = "bad-model exercise emitted no expected error"
    if args.exercise:
        summary["exercise_note"] = "Lifecycle workload includes hidden/paused/resized intervals; FPS is not a steady-state comparison. IPC actions and observed events are preserved; sending an action alone does not prove its visual result."
        summary["output_change_distinct"] = args.exercise_output != settings["output"]
    observed_scales = {dimension[2] for dimension in summary["observed_dimensions"]}
    summary["test_scale_matches_observed"] = (all(s == scale for s in observed_scales)
                                               if observed_scales and scale is not None else None)
    if status == "ok" and scale is not None and summary["test_scale_matches_observed"] is False:
        summary["status"] = "incomplete"
        summary["failure"] = "ENE_AVATAR_TEST_SCALE differs from observed telemetry scale"
    write_json(directory / "summary.json", summary)
    return summary


def write_json(path, value):
    # Replace only files owned by this newly created benchmark output directory.
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, ensure_ascii=False, allow_nan=False) + "\n")
    temporary.replace(path)


def flatten(value, prefix=""):
    result = {}
    for key, item in value.items():
        name = prefix + key
        if isinstance(item, dict):
            result.update(flatten(item, name + "."))
        else:
            result[name] = json.dumps(item, ensure_ascii=False) if isinstance(item, list) else item
    return result


def write_reports(directory, metadata, runs):
    verification = {(r["scenario"], r["backend"]): r for r in runs if r["verify"]}
    performance = [copy.deepcopy(r) for r in runs if not r["verify"]]
    for run in performance:
        reference = verification.get((run["scenario"], run["backend"]))
        if reference:
            run["accuracy"] = reference["accuracy"]
            run["accuracy"]["verification_raw_json_path"] = reference["raw_json_path"]
            run["accuracy"]["verification_status"] = reference["status"]
        else:
            run["accuracy"]["reason"] = "separate verification run skipped or not yet completed"
    groups = []
    for key, group in itertools.groupby(sorted(performance, key=lambda r: (r["scenario"], r["backend"])),
                                      key=lambda r: (r["scenario"], r["backend"])):
        group = list(group)
        valid = [r for r in group if r["status"] == "ok" and not r["exercise"]]
        groups.append({"scenario": key[0], "backend": key[1], "successful_repeats": len(valid),
                       "requested_repeats": metadata["repeats"],
                       "steady_state": not metadata.get("exercise", False),
                       "presentation_fps_across_repeats": distribution([r["presentation"]["fps"] for r in valid]),
                       "cpu_one_core_percent_across_repeats": distribution([
                           r["cpu_one_core_percent"]["time_weighted_mean"] for r in valid]),
                       "rss_mean_bytes_across_repeats": distribution([r["rss_bytes"]["mean"] for r in valid])})
    summary = {**metadata, "performance_runs": performance,
               "verification_runs": [r for r in runs if r["verify"]], "repeat_summary": groups}
    write_json(directory / "summary.json", summary)
    rows = [flatten(r) for r in performance]
    columns = sorted({k for row in rows for k in row})
    with (directory / "summary.csv").open("w", newline="", encoding="utf-8") as file:
        writer = csv.DictWriter(file, fieldnames=columns)
        writer.writeheader()
        writer.writerows(rows)


def load_settings(path):
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "ene"
    settings = {"model": str(cache / "models/Seed-san.vrm"),
                "motion": str(cache / "motions/three-vrm-sample.vrma"),
                "motion_playing": True, "motion_loop": True, "visible": True,
                "fps": 30, "size_percent": 100, "right": 24, "bottom": 24, "output": None}
    if path.exists():
        config = json.loads(path.read_text())
        avatar = config.get("avatar")
        if not isinstance(avatar, dict):
            raise ValueError("client settings need an avatar object")
        # AvatarSettings has serde(default), so absent keys retain defaults.
        # Unlike ClientSettings::default, AvatarSettings::default.model is empty.
        settings["model"], settings["motion"] = "", None
        settings.update({k: v for k, v in avatar.items() if k in settings})
    return settings


def positive_float(value):
    parsed = float(value)
    if not math.isfinite(parsed) or parsed <= 0:
        raise argparse.ArgumentTypeError("must be finite and greater than zero")
    return parsed


def nonnegative_float(value):
    parsed = float(value)
    if not math.isfinite(parsed) or parsed < 0:
        raise argparse.ArgumentTypeError("must be finite and nonnegative")
    return parsed


def positive_int(value):
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def parse_args(argv=None):
    config = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "ene/client.json"
    candidates = [ROOT / "target/release/ene-avatar", ROOT / "target/debug/ene-avatar"]
    binary = os.environ.get("ENE_AVATAR_BIN") or next((str(p) for p in candidates if p.exists()), str(candidates[0]))
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, default=Path(binary))
    parser.add_argument("--config", type=Path, default=config, help="read existing settings only")
    parser.add_argument("--model", type=Path, help="in-memory Configure override")
    parser.add_argument("--motion", type=Path, help="in-memory Configure override")
    parser.add_argument("--static", action="store_true", help="remove motion for a neutral static pose in this run")
    parser.add_argument("--exercise", action="store_true", help="timed lifecycle Configure/Recenter IPC during measurement")
    parser.add_argument("--exercise-model", type=Path, help="alternate valid VRM for lifecycle model replacement")
    parser.add_argument("--exercise-output", help="alternate compositor output name; default clears output selection")
    parser.add_argument("--backends", nargs="+", choices=BACKENDS, default=list(BACKENDS))
    parser.add_argument("--fps", nargs="+", type=int, choices=(30, 60))
    parser.add_argument("--sizes", nargs="+", type=int, choices=range(50, 201), metavar="50..200")
    parser.add_argument("--scales", nargs="+", type=positive_int, help="ENE_AVATAR_TEST_SCALE overrides; default uses compositor")
    parser.add_argument("--warmup", type=nonnegative_float)
    parser.add_argument("--duration", type=positive_float)
    parser.add_argument("--repeats", type=positive_int)
    parser.add_argument("--sample-interval", type=positive_float, default=.5)
    parser.add_argument("--ready-timeout", type=positive_float, default=120)
    parser.add_argument("--smoke", action="store_true", help="defaults: 1s warmup, 3s measure, 1 repeat")
    parser.add_argument("--skip-verify", action="store_true", help="skip separate accuracy runs; mark accuracy unavailable")
    parser.add_argument("--verify-duration", type=positive_float, default=5)
    parser.add_argument("--verify-warmup", type=nonnegative_float, default=0)
    parser.add_argument("--output-dir", type=Path, help="new or empty artifact directory")
    parser.add_argument("--dry-run", action="store_true", help="print matrix/settings; do not spawn or create artifacts")
    parser.add_argument("--self-test", action="store_true", help="run parser/counter/metric/process regression tests")
    args = parser.parse_args(argv)
    args.warmup = args.warmup if args.warmup is not None else (1 if args.smoke else 10)
    args.duration = args.duration if args.duration is not None else (3 if args.smoke else 60)
    args.repeats = args.repeats if args.repeats is not None else (1 if args.smoke else 3)
    return args


def main(argv=None):
    args = parse_args(argv)
    if args.self_test:
        return self_test()
    settings = load_settings(args.config.expanduser())
    for name in ("model", "motion"):
        if getattr(args, name):
            settings[name] = str(getattr(args, name).expanduser().resolve())
    if args.static:
        settings["motion"] = None
        settings["motion_playing"] = False
    sizes = list(dict.fromkeys(args.sizes or [settings["size_percent"]]))
    scales = list(dict.fromkeys(args.scales or [None]))
    fps = list(dict.fromkeys(args.fps or [settings["fps"]]))
    backends = list(dict.fromkeys(args.backends))
    scenarios = list(itertools.product(sizes, scales, fps))
    metadata = {"schema_version": SCHEMA_VERSION, "started_unix_ns": time.time_ns(),
                "system": {"kernel": platform.release(), "machine": platform.machine(), "python": platform.python_version()},
                "binary": str(args.binary.expanduser().resolve()),
                "config_read_only": str(args.config.expanduser().resolve()),
                "base_settings": settings, "backends": backends, "repeats": args.repeats,
                "warmup_seconds": args.warmup, "measure_seconds": args.duration,
                "resource_sample_interval_seconds": args.sample_interval,
                "exercise": args.exercise, "exercise_output": args.exercise_output,
                "scenarios": [{"size_percent": s, "test_scale": d, "fps": f} for s, d, f in scenarios],
                "performance_run_count": len(scenarios) * len(backends) * args.repeats,
                "verification_run_count": 0 if args.skip_verify else len(scenarios) * len([b for b in backends if b in ("gpu", "geometry")]),
                "minimum_performance_seconds": len(scenarios) * len(backends) * args.repeats * (args.warmup + args.duration),
                "notes": ["Startup/Ready wait is excluded from warmup and measurement.",
                          "100% CPU equals one core; process totals include all native threads.",
                          "GPU process ns counters are deduplicated by device/client; engines are kept separate.",
                          "GPU queries and CPU work are profiled; overhead is part of these measurements.",
                          "Metrics are selected by host receipt in the measurement window; raw timestamps are preserved.",
                          "Verification processes are separate and excluded from performance aggregates.",
                          "No configuration files are modified; overrides affect only this process's Configure."]}
    if args.dry_run:
        print(json.dumps(metadata, indent=2, ensure_ascii=False))
        return 0
    args.binary = args.binary.expanduser().resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        raise ValueError(f"executable not found: {args.binary}; build ene-avatar first")
    for name in ("model", "motion"):
        path = settings[name]
        if path is not None and (not path or not Path(path).expanduser().is_file()):
            raise ValueError(f"{name} path not available: {path!r}; pass --{name} to override in memory")
    if settings["fps"] not in (30, 60) or settings["size_percent"] not in range(50, 201):
        raise ValueError("existing avatar fps/size settings are invalid")
    directory = (args.output_dir or ROOT / "benchmarks/avatar" / time.strftime("%Y%m%d-%H%M%S")
                 / str(time.time_ns())).expanduser().resolve()
    if directory.exists() and any(directory.iterdir()):
        raise ValueError(f"artifact directory is not empty: {directory}")
    directory.mkdir(parents=True, exist_ok=True)
    metadata["artifact_directory"] = str(directory)
    write_json(directory / "metadata.json", metadata)
    runs = []
    print(f"Artifacts: {directory}", flush=True)
    for index, (size, scale, target_fps) in enumerate(scenarios, start=1):
        scenario = f"s{index:02d}-size{size}-scale{scale or 'native'}-fps{target_fps}"
        configured = {**settings, "size_percent": size, "fps": target_fps}
        for backend in backends:
            for repeat in range(1, args.repeats + 1):
                print(f"{scenario} {backend} repeat {repeat}/{args.repeats}", flush=True)
                result = run_one(args, configured, backend, repeat, scale, scenario, False,
                                 directory / f"{scenario}-{backend}-r{repeat}")
                runs.append(result)
                write_reports(directory, metadata, runs)
                print(f"  {result['status']}: fps={result['presentation']['fps']} "
                      f"cpu={result['cpu_one_core_percent']['time_weighted_mean']}%", flush=True)
                if result["status"] == "interrupted":
                    raise KeyboardInterrupt
            if not args.skip_verify and backend in ("gpu", "geometry"):
                print(f"{scenario} {backend} separate verification", flush=True)
                result = run_one(args, configured, backend, 1, scale, scenario, True,
                                 directory / f"{scenario}-{backend}-verify")
                runs.append(result)
                write_reports(directory, metadata, runs)
                if result["status"] == "interrupted":
                    raise KeyboardInterrupt
    print(f"Summary JSON: {directory / 'summary.json'}\nSummary CSV: {directory / 'summary.csv'}", flush=True)
    return 0 if all(r["status"] == "ok" for r in runs) else 1


def self_test():
    """Focused regression checks for parsing, timing, counters and process cleanup."""
    import tempfile
    import unittest

    class RegressionTests(unittest.TestCase):
        def test_defaults_and_smoke_overrides(self):
            full = parse_args([])
            self.assertEqual((full.warmup, full.duration, full.repeats), (10, 60, 3))
            self.assertEqual(full.backends, list(BACKENDS))
            smoke = parse_args(["--smoke", "--duration", "2"])
            self.assertEqual((smoke.warmup, smoke.duration, smoke.repeats), (1, 2, 1))

        def test_drm_dedup_and_units(self):
            text = "drm-driver: test\ndrm-pdev: 0000:01:00.0\ndrm-client-id: 7\ndrm-engine-render: 100 ns\ndrm-engine-capacity-render: 2\ndrm-engine-copy: 900 cycles\n"
            client = parse_drm_fdinfo(text)
            self.assertEqual(client["engine_ns"], {"render": 100})
            duplicate = parse_drm_fdinfo(text.replace("100 ns", "110 ns"))
            self.assertEqual(len(deduplicate_clients([client, duplicate])), 1)
            self.assertEqual(deduplicate_clients([client, duplicate])[0]["engine_ns"]["render"], 110)
            other_device = parse_drm_fdinfo(text.replace("01:00.0", "02:00.0"))
            self.assertEqual(len(deduplicate_clients([client, other_device])), 2)

        def test_resource_capacity_regression_memory_and_one_core_cpu(self):
            from unittest.mock import patch
            with tempfile.TemporaryDirectory(prefix="ene-resource-test-") as temporary:
                root = Path(temporary)
                proc = root / "proc/42"
                (proc / "fdinfo").mkdir(parents=True)
                def populate(ticks, engine, memory):
                    fields = ["0"] * 22
                    fields[0], fields[11], fields[19], fields[21] = "R", str(ticks), "77", "3"
                    (proc / "stat").write_text("42 (comm with ) parentheses) " + " ".join(fields))
                    text = (f"drm-driver: test\ndrm-pdev: 0000:01:00.0\ndrm-client-id: 7\n"
                            f"drm-engine-render: {engine} ns\ndrm-engine-capacity-render: 2\n"
                            f"drm-total-vram: {memory} MiB\ndrm-memory-gtt: 12 KiB\n")
                    for fd in (4, 5):
                        (proc / "fdinfo" / str(fd)).write_text(text)
                sampler = ResourceSampler(42, root / "proc", root / "sys")
                with patch.object(time, "monotonic_ns", side_effect=[1, 1_000_000_001, 2_000_000_001]):
                    populate(10, 100, 4)
                    sampler.sample()
                    populate(30, 1_000_000_100, 8)
                    sample = sampler.sample()
                    self.assertAlmostEqual(sample["cpu_one_core_percent"], 20 / sampler.hz * 100)
                    self.assertEqual(sample["rss_bytes"], 3 * sampler.page_size)
                    self.assertEqual(sample["gpu_process_engine_percent"], {"0000:01:00.0/render": 50})
                    self.assertEqual(sample["gpu_process_memory_bytes"]["0000:01:00.0/total-vram"], 8 * 1024 ** 2)
                    self.assertEqual(sample["gpu_process_memory_bytes"]["0000:01:00.0/memory-gtt"], 12 * 1024)
                    populate(40, 1, 8)
                    self.assertEqual(sampler.sample()["gpu_process_engine_percent"]["0000:01:00.0/render"], 0)

        def test_gpu_durations_and_buffered_scene_have_distinct_scopes(self):
            metrics = [(110, {"kind": "gpu_durations", "sample": {"frame_id": 1,
                        "mask_ns": 12, "copy_mask_ns": 34, "present_ns": 56,
                        "transport_latency_ns": 78, "readback_bytes": 1024,
                        "timestamp_readback_bytes": 16, "overflow_retries": 0}}),
                       (120, {"kind": "gpu_timestamps", "sample": {"frame_id": 2,
                        "timestamp_period_ns": 10, "scene_queue_elapsed": {"start_ticks": 10, "end_ticks": 30},
                        "scene_render_batch": {"start_ticks": 5, "end_ticks": 15}}})]
            summary = metric_summary(metrics, 100, 200, False)
            self.assertEqual(summary["gpu_stage_ns"]["scene_ns"]["mean"], 100)
            self.assertEqual(summary["gpu_stage_ns"]["scene_queue_elapsed_ns"]["mean"], 200)
            self.assertEqual(summary["gpu_stage_ns"]["copy_mask_ns"]["mean"], 34)
            self.assertEqual(summary["gpu_transfer"]["readback_bytes"]["total"], 1024)
            self.assertFalse(metric_summary([], 100, 200, False)["gpu_stage_available"])

        def test_presentation_and_verification(self):
            def record(kind, sample, stamp=110):
                return stamp, {"kind": kind, "sample": sample}
            metrics = [record("presented", {"clock_id": 1, "presentation_ns": 0}),
                       record("presented", {"clock_id": 1, "presentation_ns": 20_000_000}),
                       record("presented", {"clock_id": 2, "presentation_ns": 0}),
                       record("presented", {"clock_id": 2, "presentation_ns": 40_000_000}),
                       record("verification", {"frame_id": 4, "width": 2, "height": 2, "scale": 1,
                           "reference_rule": "alpha", "true_positives": 1, "false_positive": 1,
                           "false_negative": 1, "true_negatives": 1})]
            result = metric_summary(metrics, 100, 1_000_000_100, True)
            self.assertEqual(result["presentation"]["fps"], 4)
            self.assertEqual(result["presentation"]["interval_ns"]["mean"], 30_000_000)
            self.assertEqual(result["presentation"]["clock_resets"], 1)
            self.assertEqual(result["accuracy"]["false_positive_rate"], .5)
            self.assertFalse(metric_summary(metrics, 100, 1_000_000_100, False)["accuracy"]["available"])
            self.assertIsNone(metric_summary([], 100, 1000, False)["presentation"]["fps"])

        def test_display_latency_requires_matching_clock_and_consistent_bounds(self):
            sample = {"clock_id": time.CLOCK_MONOTONIC, "presentation_ns": 1050,
                      "enqueue_to_callback_ns": 1000, "enqueue_to_display_ns_estimate": 65,
                      "enqueue_to_display_ns_lower_bound": 59, "enqueue_to_display_ns_upper_bound": 71,
                      "enqueue_to_display_ns_uncertainty": 6,
                      "enqueue_clock_mapping": {"clock_id": time.CLOCK_MONOTONIC,
                          "enqueue_clock_lower_ns": 979, "enqueue_clock_upper_ns": 991,
                          "calibration_window_ns": 10, "clock_resolution_ns": 1,
                          "registration_age_ns": 10, "assumptions": "fixture same clock/rate assumption"}}
            summary = display_latency_summary([sample])
            self.assertTrue(summary["display_latency_available"])
            self.assertEqual(summary["enqueue_to_display_ns_estimate"]["mean"], 65)
            wrong_clock = {**sample, "clock_id": time.CLOCK_REALTIME}
            self.assertFalse(display_latency_summary([wrong_clock])["display_latency_available"])
            invalid_bound = {**sample, "enqueue_to_display_ns_upper_bound": 70}
            self.assertEqual(display_latency_summary([invalid_bound])["display_latency_invalid_samples"], 1)
            absent = display_latency_summary([{"enqueue_to_callback_ns": 1000}])
            self.assertFalse(absent["display_latency_available"])
            self.assertIsNone(absent["enqueue_to_display_ns_estimate"]["mean"])

        def test_process_drains_and_separate_verification(self):
            with tempfile.TemporaryDirectory(prefix="ene-benchmark-test-") as temporary:
                directory = Path(temporary)
                child = directory / "avatar"
                # A mock avatar exercises the real process, IPC, both pipe drains,
                # profile envelope, shutdown, and separate verification switches.
                child.write_text("#!/usr/bin/env python3\n" +
                    "import json,os,sys,time,threading\n"
                    "settings=json.loads(sys.stdin.readline())\n"
                    "assert settings['type']=='configure'\n"
                    "assert os.environ['ENE_AVATAR_PROFILE']=='1'\n"
                    "print(json.dumps({'type':'ready','model':'fixture'}),flush=True)\n"
                    "stop=threading.Event()\n"
                    "def emit():\n"
                    " i=0\n"
                    " while not stop.is_set():\n"
                    "  sample={'clock_id':1,'presentation_ns':time.monotonic_ns(),'frame_id':i}\n"
                    "  kind='presented'\n"
                    "  if os.environ.get('ENE_AVATAR_VERIFY')=='1':\n"
                    "   kind='verification'\n"
                    "   sample={'frame_id':i,'width':2,'height':2,'scale':1,'reference_rule':'fixture alpha','true_positives':1,'false_positive':1,'false_negative':1,'true_negatives':1}\n"
                    "  record={'schema_version':1,'kind':kind,'backend':os.environ['ENE_AVATAR_BACKEND'],'verify':os.environ.get('ENE_AVATAR_VERIFY')=='1','pid':os.getpid(),'sample':sample}\n"
                    "  print('ENE_AVATAR_METRIC '+json.dumps(record),file=sys.stderr,flush=True)\n"
                    "  print(json.dumps({'type':'motion_state','state':'playing','padding':'x'*8192}),flush=True)\n"
                    "  i+=1;time.sleep(.01)\n"
                    "t=threading.Thread(target=emit);t.start()\n"
                    "for line in sys.stdin:\n"
                    " request=json.loads(line)\n"
                    " if request['type']=='shutdown':break\n"
                    " if request['type']=='configure' and request['settings'].get('model','').endswith('missing-model.vrm'):\n"
                    "  print(json.dumps({'type':'error','message':'fixture missing model'}),flush=True)\n"
                    "stop.set();t.join()\n")
                child.chmod(0o700)
                args = parse_args(["--binary", str(child), "--warmup", "0", "--duration", ".2",
                                   "--verify-duration", ".2", "--sample-interval", ".05"])
                performance = run_one(args, {}, "gpu", 1, None, "fixture", False, directory / "performance")
                accuracy = run_one(args, {}, "gpu", 1, None, "fixture", True, directory / "verification")
                self.assertEqual(performance["status"], "ok")
                self.assertTrue(performance["presentation"]["available"])
                self.assertFalse(performance["accuracy"]["available"])
                self.assertTrue(accuracy["accuracy"]["available"])
                self.assertEqual(accuracy["status"], "ok")
                raw = [json.loads(line) for line in Path(performance["raw_json_path"]).read_text().splitlines()]
                self.assertTrue(any(r["kind"] == "resource" for r in raw))
                self.assertTrue(any(r["kind"] == "ipc" for r in raw))
                self.assertTrue(any(r["kind"] == "metric" for r in raw))
                lifecycle_args = parse_args(["--binary", str(child), "--warmup", "0", "--duration", "1.4",
                                            "--exercise", "--sample-interval", ".05"])
                baseline = load_settings(directory / "nonexistent-client.json")
                lifecycle = run_one(lifecycle_args, baseline, "gpu", 1, None, "exercise", False, directory / "exercise")
                self.assertEqual(lifecycle["status"], "ok")
                self.assertEqual(len(lifecycle["exercise_actions"]), 13)
                self.assertEqual(len(lifecycle["expected_model_errors"]), 1)
                self.assertEqual(lifecycle["exercise_actions"][-1]["request"]["settings"], baseline)

    suite = unittest.defaultTestLoader.loadTestsFromTestCase(RegressionTests)
    return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError) as error:
        print(f"benchmark-avatar: {error}", file=sys.stderr)
        sys.exit(2)
    except KeyboardInterrupt:
        print("benchmark-avatar: interrupted; completed raw records and summaries are preserved", file=sys.stderr)
        sys.exit(130)
