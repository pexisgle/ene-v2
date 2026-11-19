#!/usr/bin/env python3
"""Convert Kimodo SOMA-77 .npz motions into VRM 1.0 .vrma (GLB 2.0) files, and validate .vrma assets.

Includes an ENE.vrm-aware retargeting & kinematic clearance pass so human adult SOMA mocap
adapts cleanly to stylized anime proportions (narrow shoulders, flared A-line coat/skirt,
twin-bun head volume, modest closed-knee seated poses, and desktop-facing head framing).
"""

from __future__ import annotations

import argparse
import json
import math
import struct
from pathlib import Path
from typing import Any

import numpy as np
from scipy.spatial.transform import Rotation, Slerp

# Canonical 52-bone VRM 1.0 Humanoid hierarchy: (vrm_bone, vrm_parent, soma_position_joint)
VRM_BONES: list[tuple[str, str | None, str]] = [
    ("hips", None, "Hips"),
    ("spine", "hips", "Spine1"),
    ("chest", "spine", "Spine2"),
    ("upperChest", "chest", "Chest"),
    ("neck", "upperChest", "Neck1"),
    ("head", "neck", "Head"),
    ("leftShoulder", "upperChest", "LeftShoulder"),
    ("leftUpperArm", "leftShoulder", "LeftArm"),
    ("leftLowerArm", "leftUpperArm", "LeftForeArm"),
    ("leftHand", "leftLowerArm", "LeftHand"),
    ("leftThumbMetacarpal", "leftHand", "LeftHandThumb1"),
    ("leftThumbProximal", "leftThumbMetacarpal", "LeftHandThumb2"),
    ("leftThumbDistal", "leftThumbProximal", "LeftHandThumb3"),
    ("leftIndexProximal", "leftHand", "LeftHandIndex2"),
    ("leftIndexIntermediate", "leftIndexProximal", "LeftHandIndex3"),
    ("leftIndexDistal", "leftIndexIntermediate", "LeftHandIndex4"),
    ("leftMiddleProximal", "leftHand", "LeftHandMiddle2"),
    ("leftMiddleIntermediate", "leftMiddleProximal", "LeftHandMiddle3"),
    ("leftMiddleDistal", "leftMiddleIntermediate", "LeftHandMiddle4"),
    ("leftRingProximal", "leftHand", "LeftHandRing2"),
    ("leftRingIntermediate", "leftRingProximal", "LeftHandRing3"),
    ("leftRingDistal", "leftRingIntermediate", "LeftHandRing4"),
    ("leftLittleProximal", "leftHand", "LeftHandPinky2"),
    ("leftLittleIntermediate", "leftLittleProximal", "LeftHandPinky3"),
    ("leftLittleDistal", "leftLittleIntermediate", "LeftHandPinky4"),
    ("rightShoulder", "upperChest", "RightShoulder"),
    ("rightUpperArm", "rightShoulder", "RightArm"),
    ("rightLowerArm", "rightUpperArm", "RightForeArm"),
    ("rightHand", "rightLowerArm", "RightHand"),
    ("rightThumbMetacarpal", "rightHand", "RightHandThumb1"),
    ("rightThumbProximal", "rightThumbMetacarpal", "RightHandThumb2"),
    ("rightThumbDistal", "rightThumbProximal", "RightHandThumb3"),
    ("rightIndexProximal", "rightHand", "RightHandIndex2"),
    ("rightIndexIntermediate", "rightIndexProximal", "RightHandIndex3"),
    ("rightIndexDistal", "rightIndexIntermediate", "RightHandIndex4"),
    ("rightMiddleProximal", "rightHand", "RightHandMiddle2"),
    ("rightMiddleIntermediate", "rightMiddleProximal", "RightHandMiddle3"),
    ("rightMiddleDistal", "rightMiddleIntermediate", "RightHandMiddle4"),
    ("rightRingProximal", "rightHand", "RightHandRing2"),
    ("rightRingIntermediate", "rightRingProximal", "RightHandRing3"),
    ("rightRingDistal", "rightRingIntermediate", "RightHandRing4"),
    ("rightLittleProximal", "rightHand", "RightHandPinky2"),
    ("rightLittleIntermediate", "rightLittleProximal", "RightHandPinky3"),
    ("rightLittleDistal", "rightLittleIntermediate", "RightHandPinky4"),
    ("leftUpperLeg", "hips", "LeftLeg"),
    ("leftLowerLeg", "leftUpperLeg", "LeftShin"),
    ("leftFoot", "leftLowerLeg", "LeftFoot"),
    ("leftToes", "leftFoot", "LeftToeBase"),
    ("rightUpperLeg", "hips", "RightLeg"),
    ("rightLowerLeg", "rightUpperLeg", "RightShin"),
    ("rightFoot", "rightLowerLeg", "RightFoot"),
    ("rightToes", "rightFoot", "RightToeBase"),
]

# Measured ENE.vrm T-pose local bone offsets (meters, identity rest orientation)
ENE_OFFSETS: dict[str, np.ndarray] = {
    "hips": np.array([0.0, 0.680, -0.002]),
    "spine": np.array([0.0, 0.073, 0.011]),
    "chest": np.array([0.0, 0.048, -0.004]),
    "upperChest": np.array([0.0, 0.061, -0.002]),
    "neck": np.array([0.0, 0.068, -0.017]),
    "head": np.array([0.0, 0.046, 0.002]),
    "leftShoulder": np.array([0.018, 0.045, -0.017]),
    "leftUpperArm": np.array([0.082, -0.023, -0.002]),
    "leftLowerArm": np.array([0.1273, 0.0, 0.0]),
    "leftHand": np.array([0.1256, 0.0, 0.0]),
    "rightShoulder": np.array([-0.018, 0.045, -0.017]),
    "rightUpperArm": np.array([-0.082, -0.023, -0.002]),
    "rightLowerArm": np.array([-0.1273, 0.0, 0.0]),
    "rightHand": np.array([-0.1256, 0.0, 0.0]),
    "leftUpperLeg": np.array([0.048, 0.0, 0.001]),
    "leftLowerLeg": np.array([0.0, -0.2201, 0.0]),
    "leftFoot": np.array([0.0, -0.3765, 0.0]),
    "rightUpperLeg": np.array([-0.048, 0.0, 0.001]),
    "rightLowerLeg": np.array([0.0, -0.2201, 0.0]),
    "rightFoot": np.array([0.0, -0.3765, 0.0]),
}

DIRECT_SOMA_MAP: dict[str, str] = {
    "hips": "Hips",
    "spine": "Spine1",
    "chest": "Spine2",
    "upperChest": "Chest",
    "head": "Head",
    "leftShoulder": "LeftShoulder",
    "leftUpperArm": "LeftArm",
    "leftLowerArm": "LeftForeArm",
    "leftHand": "LeftHand",
    "leftThumbMetacarpal": "LeftHandThumb1",
    "leftThumbProximal": "LeftHandThumb2",
    "leftThumbDistal": "LeftHandThumb3",
    "leftIndexIntermediate": "LeftHandIndex3",
    "leftIndexDistal": "LeftHandIndex4",
    "leftMiddleIntermediate": "LeftHandMiddle3",
    "leftMiddleDistal": "LeftHandMiddle4",
    "leftRingIntermediate": "LeftHandRing3",
    "leftRingDistal": "LeftHandRing4",
    "leftLittleIntermediate": "LeftHandPinky3",
    "leftLittleDistal": "LeftHandPinky4",
    "rightShoulder": "RightShoulder",
    "rightUpperArm": "RightArm",
    "rightLowerArm": "RightForeArm",
    "rightHand": "RightHand",
    "rightThumbMetacarpal": "RightHandThumb1",
    "rightThumbProximal": "RightHandThumb2",
    "rightThumbDistal": "RightHandThumb3",
    "rightIndexIntermediate": "RightHandIndex3",
    "rightIndexDistal": "RightHandIndex4",
    "rightMiddleIntermediate": "RightHandMiddle3",
    "rightMiddleDistal": "RightHandMiddle4",
    "rightRingIntermediate": "RightHandRing3",
    "rightRingDistal": "RightHandRing4",
    "rightLittleIntermediate": "RightHandPinky3",
    "rightLittleDistal": "RightHandPinky4",
    "leftUpperLeg": "LeftLeg",
    "leftLowerLeg": "LeftShin",
    "leftFoot": "LeftFoot",
    "leftToes": "LeftToeBase",
    "rightUpperLeg": "RightLeg",
    "rightLowerLeg": "RightShin",
    "rightFoot": "RightFoot",
    "rightToes": "RightToeBase",
}

COMPOSED_SOMA_MAP: dict[str, tuple[str, str]] = {
    "neck": ("Neck1", "Neck2"),
    "leftIndexProximal": ("LeftHandIndex1", "LeftHandIndex2"),
    "leftMiddleProximal": ("LeftHandMiddle1", "LeftHandMiddle2"),
    "leftRingProximal": ("LeftHandRing1", "LeftHandRing2"),
    "leftLittleProximal": ("LeftHandPinky1", "LeftHandPinky2"),
    "rightIndexProximal": ("RightHandIndex1", "RightHandIndex2"),
    "rightMiddleProximal": ("RightHandMiddle1", "RightHandMiddle2"),
    "rightRingProximal": ("RightHandRing1", "RightHandRing2"),
    "rightLittleProximal": ("RightHandPinky1", "RightHandPinky2"),
}


def _smoothstep(x: np.ndarray | float) -> np.ndarray | float:
    t = np.clip(x, 0.0, 1.0)
    return t * t * (3.0 - 2.0 * t)


def _smooth_1d(arr: np.ndarray, radius: int = 3) -> np.ndarray:
    if radius <= 0 or len(arr) < 3:
        return arr
    k = np.exp(-0.5 * (np.arange(-radius, radius + 1) / (radius * 0.55)) ** 2)
    k /= np.sum(k)
    if arr.ndim == 1:
        padded = np.pad(arr, radius, mode="edge")
        return np.convolve(padded, k, mode="valid")
    out = np.empty_like(arr)
    for c in range(arr.shape[1]):
        padded = np.pad(arr[:, c], radius, mode="edge")
        out[:, c] = np.convolve(padded, k, mode="valid")
    return out


def _window_envelope(T: int, start_ratio: float, end_ratio: float, ramp_ratio: float = 0.14) -> np.ndarray:
    u = np.linspace(0.0, 1.0, T, dtype=np.float64)
    ramp = max(0.04, min(ramp_ratio, 0.45 * (end_ratio - start_ratio)))
    rise = _smoothstep((u - start_ratio) / ramp)
    fall = _smoothstep((end_ratio - u) / ramp)
    env = rise * fall
    env[0] = 0.0
    env[-1] = 0.0
    return env


def _compute_yaw_angle(posed: np.ndarray, bidx: dict[str, int], frame: int = 0) -> float:
    p = posed[frame]
    left_vec = 0.5 * ((p[bidx["LeftLeg"]] - p[bidx["RightLeg"]]) + (p[bidx["LeftArm"]] - p[bidx["RightArm"]]))
    # Right-handed +Y up, +X = character's left, +Z = forward => forward = left_vec x up
    fwd = np.cross(np.array([left_vec[0], 0.0, left_vec[2]]), np.array([0.0, 1.0, 0.0]))
    n = np.linalg.norm(fwd)
    if n < 1e-6:
        return 0.0
    fwd /= n
    return float(math.atan2(fwd[0], fwd[2]))


def _scale_rotation(r: Rotation, factor: float) -> Rotation:
    rv = r.as_rotvec() * factor
    return Rotation.from_rotvec(rv)


def _slerp_quats(q_a: np.ndarray, q_b: np.ndarray, w: np.ndarray | float) -> np.ndarray:
    T = q_a.shape[0]
    ra = Rotation.from_quat(q_a)
    rb = Rotation.from_quat(q_b)
    out = np.empty((T, 4), dtype=np.float64)
    for t in range(T):
        wt = float(w[t]) if isinstance(w, np.ndarray) else float(w)
        if wt <= 1e-5:
            out[t] = q_a[t]
        elif wt >= 1.0 - 1e-5:
            out[t] = q_b[t]
        else:
            out[t] = Slerp([0.0, 1.0], Rotation.concatenate([ra[t], rb[t]]))([wt])[0].as_quat()
    return out


def _ene_fk_frame(rots_t: dict[str, np.ndarray], hips_pos: np.ndarray) -> tuple[dict[str, np.ndarray], dict[str, np.ndarray]]:
    """Compute world position & rotation matrix of primary ENE.vrm bones for one frame."""
    pos: dict[str, np.ndarray] = {"hips": hips_pos.copy()}
    rot: dict[str, np.ndarray] = {"hips": rots_t["hips"]}
    order = [
        ("spine", "hips"),
        ("chest", "spine"),
        ("upperChest", "chest"),
        ("neck", "upperChest"),
        ("head", "neck"),
        ("leftShoulder", "upperChest"),
        ("leftUpperArm", "leftShoulder"),
        ("leftLowerArm", "leftUpperArm"),
        ("leftHand", "leftLowerArm"),
        ("rightShoulder", "upperChest"),
        ("rightUpperArm", "rightShoulder"),
        ("rightLowerArm", "rightUpperArm"),
        ("rightHand", "rightLowerArm"),
        ("leftUpperLeg", "hips"),
        ("leftLowerLeg", "leftUpperLeg"),
        ("leftFoot", "leftLowerLeg"),
        ("rightUpperLeg", "hips"),
        ("rightLowerLeg", "rightUpperLeg"),
        ("rightFoot", "rightLowerLeg"),
    ]
    for b, p in order:
        pos[b] = pos[p] + rot[p] @ ENE_OFFSETS[b]
        rot[b] = rot[p] @ rots_t[b]
    return pos, rot


def _refine_standing_legs(quats: dict[str, np.ndarray], T: int) -> None:
    """Keep standing legs neat, anime-proportioned, and skirt-friendly (no masculine splay)."""
    hips_e = Rotation.from_quat(quats["hips"]).as_euler("XYZ", degrees=True)
    hips_e[:, 0] = np.clip(hips_e[:, 0] * 0.55, -7.0, 7.0)
    hips_e[:, 1] = np.clip(hips_e[:, 1] * 0.65, -14.0, 14.0)
    hips_e[:, 2] = np.clip(hips_e[:, 2] * 0.55, -6.0, 6.0)
    quats["hips"] = Rotation.from_euler("XYZ", hips_e, degrees=True).as_quat()

    for side in ("left", "right"):
        ul = Rotation.from_quat(quats[f"{side}UpperLeg"]).as_euler("XYZ", degrees=True)
        # Keep slight natural knee softness while removing outward leg splay & bow-legged yaw
        ul[:, 0] = np.clip(ul[:, 0], -22.0, 10.0)
        ul[:, 1] = np.clip(ul[:, 1] * 0.22, -5.0, 5.0)
        ul[:, 2] = np.clip(ul[:, 2] * 0.30, -3.5, 3.5)
        quats[f"{side}UpperLeg"] = Rotation.from_euler("XYZ", ul, degrees=True).as_quat()

        ll = Rotation.from_quat(quats[f"{side}LowerLeg"]).as_euler("XYZ", degrees=True)
        ll[:, 0] = np.clip(ll[:, 0], 0.0, 42.0)
        ll[:, 1] = np.clip(ll[:, 1] * 0.25, -4.0, 4.0)
        ll[:, 2] = np.clip(ll[:, 2] * 0.25, -4.0, 4.0)
        quats[f"{side}LowerLeg"] = Rotation.from_euler("XYZ", ll, degrees=True).as_quat()

        ft = Rotation.from_quat(quats[f"{side}Foot"]).as_euler("XYZ", degrees=True)
        ft[:, 0] = np.clip(ft[:, 0], -22.0, 22.0)
        ft[:, 1] = np.clip(ft[:, 1] * 0.35, -8.0, 8.0)
        ft[:, 2] = np.clip(ft[:, 2] * 0.35, -8.0, 8.0)
        quats[f"{side}Foot"] = Rotation.from_euler("XYZ", ft, degrees=True).as_quat()


def _refine_shoulders(quats: dict[str, np.ndarray], T: int) -> None:
    """Prevent adult SOMA shoulder hunch/drop from pushing ENE.vrm's wide sleeves into the torso."""
    for side, sgn in [("left", 1.0), ("right", -1.0)]:
        se = Rotation.from_quat(quats[f"{side}Shoulder"]).as_euler("XYZ", degrees=True)
        se *= 0.25
        se[:, 0] = np.clip(se[:, 0], -5.0, 5.0)
        se[:, 1] = np.clip(se[:, 1], -6.0, 6.0)
        # Never drop shoulders downward (left Z < -2 or right Z > +2 collapses sleeves into coat)
        if side == "left":
            se[:, 2] = np.clip(se[:, 2], -2.0, 12.0)
        else:
            se[:, 2] = np.clip(se[:, 2], -12.0, 2.0)
        quats[f"{side}Shoulder"] = Rotation.from_euler("XYZ", se, degrees=True).as_quat()


def _refine_seated_modest_pose(
    quats: dict[str, np.ndarray],
    root_positions: np.ndarray,
    preset: str,
    h_rest: float,
    T: int,
) -> np.ndarray:
    """Apply a modest, skirt-safe perched seated lower-body & lap framing verified on ENE.vrm."""
    phase = np.linspace(0.0, 2.0 * math.pi, T, endpoint=False)

    # Keep hips upright / slightly pitched forward (+8 deg) so ENE's mini-skirt drapes cleanly DOWN
    # over her lap instead of tilting open toward the camera.
    hips_e = Rotation.from_quat(quats["hips"]).as_euler("XYZ", degrees=True)
    hips_out = np.zeros((T, 3), dtype=np.float64)
    hips_out[:, 0] = 8.0 + 1.4 * np.sin(phase)
    hips_out[:, 1] = np.clip(hips_e[:, 1] * 0.35, -5.0, 5.0)
    hips_out[:, 2] = np.clip(hips_e[:, 2] * 0.35, -3.5, 3.5)
    quats["hips"] = Rotation.from_euler("XYZ", hips_out, degrees=True).as_quat()

    # Keep spine upright & cozy (countering hips +8 deg forward tilt cleanly)
    sp_e = Rotation.from_quat(quats["spine"]).as_euler("XYZ", degrees=True)
    sp_e[:, 0] = np.clip(sp_e[:, 0] - 5.0, -8.0, 6.0)
    sp_e[:, 1] = np.clip(sp_e[:, 1] * 0.5, -8.0, 8.0)
    sp_e[:, 2] = np.clip(sp_e[:, 2] * 0.5, -6.0, 6.0)
    quats["spine"] = Rotation.from_euler("XYZ", sp_e, degrees=True).as_quat()

    # Thighs neatly closed together (inward adduction + slight downward angle) -> ZERO skirt exposure
    for side, sgn, kick_off in [("left", 1.0, 0.0), ("right", -1.0, math.pi)]:
        ul = np.zeros((T, 3), dtype=np.float64)
        ll = np.zeros((T, 3), dtype=np.float64)
        ft = np.zeros((T, 3), dtype=np.float64)
        if preset == "sit_swing_legs":
            kick_phase = 2.0 * phase + kick_off  # 2 playful cycles per loop
            ul[:, 0] = -58.0 + 3.5 * np.sin(kick_phase)
            ul[:, 1] = -sgn * 6.0  # slight feminine inward knee turn
            ul[:, 2] = -sgn * 4.5  # press thighs cleanly together
            ll[:, 0] = 64.0 - 17.0 * np.sin(kick_phase)
            ft[:, 0] = 18.0 + 7.0 * np.sin(kick_phase - 0.4)
        else:
            sway = 1.8 * np.sin(phase + kick_off * 0.3)
            ul[:, 0] = -56.0 + sway
            ul[:, 1] = -sgn * 6.5
            ul[:, 2] = -sgn * 4.8
            ll[:, 0] = 63.0 - 0.6 * sway
            ft[:, 0] = 16.0
        quats[f"{side}UpperLeg"] = Rotation.from_euler("XYZ", ul, degrees=True).as_quat()
        quats[f"{side}LowerLeg"] = Rotation.from_euler("XYZ", ll, degrees=True).as_quat()
        quats[f"{side}Foot"] = Rotation.from_euler("XYZ", ft, degrees=True).as_quat()
        quats[f"{side}Toes"] = np.tile(np.array([0.0, 0.0, 0.0, 1.0]), (T, 1))

    # Seated resting arms: keep elbows gently outside coat and hands resting neatly beside thighs
    # (for sit_wave, preserve Kimodo's waving right arm while cleaning the resting left arm!)
    resting_sides = [("left", 1.0)] if preset == "sit_wave" else [("left", 1.0), ("right", -1.0)]
    for side, sgn in resting_sides:
        ua = np.zeros((T, 3), dtype=np.float64)
        la = np.zeros((T, 3), dtype=np.float64)
        ha = np.zeros((T, 3), dtype=np.float64)
        sw = 2.0 * np.sin(phase + (0.0 if sgn > 0 else 0.7))
        ua[:, 0] = -16.0 + 0.5 * sw
        ua[:, 1] = -sgn * 8.0
        ua[:, 2] = -sgn * (62.0 + sw)
        la[:, 0] = 6.0
        la[:, 1] = -sgn * 32.0
        la[:, 2] = -sgn * 6.0
        ha[:, 2] = sgn * 8.0
        tgt_ua = Rotation.from_euler("XYZ", ua, degrees=True).as_quat()
        tgt_la = Rotation.from_euler("XYZ", la, degrees=True).as_quat()
        tgt_ha = Rotation.from_euler("XYZ", ha, degrees=True).as_quat()
        quats[f"{side}UpperArm"] = _slerp_quats(quats[f"{side}UpperArm"], tgt_ua, 0.88)
        quats[f"{side}LowerArm"] = _slerp_quats(quats[f"{side}LowerArm"], tgt_la, 0.88)
        quats[f"{side}Hand"] = _slerp_quats(quats[f"{side}Hand"], tgt_ha, 0.82)

    # Seated hips height nicely elevated on ledge (~0.72 * h_rest)
    root_out = root_positions.copy()
    root_out[:, 0] *= 0.35
    root_out[:, 2] *= 0.35
    root_out[:, 1] = 0.72 * h_rest + 0.006 * np.sin(phase)
    return root_out


def _refine_drag_held_pose(
    quats: dict[str, np.ndarray],
    root_positions: np.ndarray,
    h_rest: float,
    T: int,
) -> np.ndarray:
    """DesktopMate pinched/suspended dangling pose with modest closed knees & coat-clear arms."""
    phase = np.linspace(0.0, 2.0 * math.pi, T, endpoint=False)

    # Slight lifted sway like being pinched by the collar
    root_out = np.zeros((T, 3), dtype=np.float64)
    root_out[:, 0] = 0.028 * np.sin(phase)
    root_out[:, 1] = 1.04 * h_rest + 0.010 * np.cos(2.0 * phase)
    root_out[:, 2] = 0.008 * np.sin(phase)

    # Gentle pendulum torso sway
    hips_e = np.zeros((T, 3), dtype=np.float64)
    hips_e[:, 0] = 4.0 + 2.0 * np.cos(2.0 * phase)
    hips_e[:, 1] = 4.0 * np.sin(phase)
    hips_e[:, 2] = -5.5 * np.sin(phase)
    quats["hips"] = Rotation.from_euler("XYZ", hips_e, degrees=True).as_quat()

    # Arms held slightly outward in cute helpless balance (100% clear of coat/skirt!)
    for side, sgn, ph in [("left", 1.0, 0.0), ("right", -1.0, 0.6)]:
        ua = np.zeros((T, 3), dtype=np.float64)
        la = np.zeros((T, 3), dtype=np.float64)
        ha = np.zeros((T, 3), dtype=np.float64)
        ua[:, 0] = -10.0 + 5.0 * np.sin(phase + ph)
        ua[:, 1] = -sgn * (8.0 + 4.0 * np.cos(phase + ph))
        ua[:, 2] = -sgn * (42.0 + 6.0 * np.sin(phase + ph))
        la[:, 0] = 4.0
        la[:, 1] = -sgn * (28.0 + 6.0 * np.cos(phase + ph))
        la[:, 2] = -sgn * 8.0
        ha[:, 2] = sgn * (10.0 + 5.0 * np.sin(phase + ph))
        quats[f"{side}UpperArm"] = _slerp_quats(quats[f"{side}UpperArm"], Rotation.from_euler("XYZ", ua, degrees=True).as_quat(), 0.85)
        quats[f"{side}LowerArm"] = _slerp_quats(quats[f"{side}LowerArm"], Rotation.from_euler("XYZ", la, degrees=True).as_quat(), 0.85)
        quats[f"{side}Hand"] = _slerp_quats(quats[f"{side}Hand"], Rotation.from_euler("XYZ", ha, degrees=True).as_quat(), 0.85)

        # Modest dangling legs: thighs gently bent & closed together, knees & ankles swinging softly
        ul = np.zeros((T, 3), dtype=np.float64)
        ll = np.zeros((T, 3), dtype=np.float64)
        ft = np.zeros((T, 3), dtype=np.float64)
        ul[:, 0] = -14.0 + 5.0 * np.sin(phase + ph)
        ul[:, 1] = -sgn * 3.5
        ul[:, 2] = -sgn * 2.5 + 4.5 * np.sin(phase - 0.5)
        ll[:, 0] = 30.0 + 9.0 * np.sin(phase + ph + 0.5)
        ft[:, 0] = 22.0 + 6.0 * np.sin(phase + ph + 0.9)
        quats[f"{side}UpperLeg"] = Rotation.from_euler("XYZ", ul, degrees=True).as_quat()
        quats[f"{side}LowerLeg"] = Rotation.from_euler("XYZ", ll, degrees=True).as_quat()
        quats[f"{side}Foot"] = Rotation.from_euler("XYZ", ft, degrees=True).as_quat()

    return root_out


def _refine_preset_specific(quats: dict[str, np.ndarray], preset: str, T: int) -> None:
    """Targeted artistic adjustments for presets whose adult mocap needed anime styling on ENE.vrm."""
    env = _window_envelope(T, 0.08, 0.92, ramp_ratio=0.18)

    if preset == "bow":
        # Graceful unmistakable Japanese desktop bow (~32 deg peak forward bend, feet firmly planted)
        u = np.linspace(0.0, 1.0, T, dtype=np.float64)
        bow_curve = np.sin(math.pi * np.clip((u - 0.10) / 0.78, 0.0, 1.0)) ** 1.4
        # Keep hips nearly level & legs straight/planted so feet stay flat on the floor during the bow
        he_hips = Rotation.from_quat(quats["hips"]).as_euler("XYZ", degrees=True)
        he_hips[:, 0] = 2.0 * bow_curve
        he_hips[:, 1] *= 0.25
        he_hips[:, 2] *= 0.25
        quats["hips"] = Rotation.from_euler("XYZ", he_hips, degrees=True).as_quat()
        for side in ("left", "right"):
            ul = Rotation.from_quat(quats[f"{side}UpperLeg"]).as_euler("XYZ", degrees=True)
            ll = Rotation.from_quat(quats[f"{side}LowerLeg"]).as_euler("XYZ", degrees=True)
            ft = Rotation.from_quat(quats[f"{side}Foot"]).as_euler("XYZ", degrees=True)
            ul[:, 0] = ul[:, 0] * (1.0 - 0.85 * bow_curve) - 2.0 * bow_curve
            ll[:, 0] = ll[:, 0] * (1.0 - 0.85 * bow_curve) + 2.0 * bow_curve
            ft[:, 0] = ft[:, 0] * (1.0 - 0.85 * bow_curve) - 1.5 * bow_curve
            quats[f"{side}UpperLeg"] = Rotation.from_euler("XYZ", ul, degrees=True).as_quat()
            quats[f"{side}LowerLeg"] = Rotation.from_euler("XYZ", ll, degrees=True).as_quat()
            quats[f"{side}Foot"] = Rotation.from_euler("XYZ", ft, degrees=True).as_quat()
        for bname, peak_x in [("spine", 11.0), ("chest", 11.0), ("upperChest", 10.0), ("neck", 4.0), ("head", 4.0)]:
            e = Rotation.from_quat(quats[bname]).as_euler("XYZ", degrees=True)
            e[:, 0] = peak_x * bow_curve
            e[:, 1] = np.clip(e[:, 1] * 0.25, -4.0, 4.0)
            e[:, 2] = np.clip(e[:, 2] * 0.25, -3.0, 3.0)
            quats[bname] = Rotation.from_euler("XYZ", e, degrees=True).as_quat()
        for side, sgn in [("left", 1.0), ("right", -1.0)]:
            ua = Rotation.from_euler("XYZ", [-20.0, -sgn * 10.0, -sgn * 63.0], degrees=True).as_quat()
            la = Rotation.from_euler("XYZ", [8.0, -sgn * 48.0, sgn * 6.0], degrees=True).as_quat()
            quats[f"{side}UpperArm"] = _slerp_quats(quats[f"{side}UpperArm"], np.tile(ua, (T, 1)), 0.75 * bow_curve)
            quats[f"{side}LowerArm"] = _slerp_quats(quats[f"{side}LowerArm"], np.tile(la, (T, 1)), 0.75 * bow_curve)

    elif preset == "idle_hands_behind":
        # Hands neatly clasped behind the lower back (FK-verified on ENE.vrm: el=[±0.14,0.78,-0.08], wr=[±0.034,0.74,-0.133])
        phase = np.linspace(0.0, 2.0 * math.pi, T, endpoint=False)
        for side, sgn in [("left", 1.0), ("right", -1.0)]:
            ua = np.zeros((T, 3), dtype=np.float64)
            la = np.zeros((T, 3), dtype=np.float64)
            ha = np.zeros((T, 3), dtype=np.float64)
            sw = 1.4 * np.sin(phase)
            ua[:, 0] = 20.0 + 0.5 * sw
            ua[:, 1] = sgn * 32.0
            ua[:, 2] = -sgn * (68.0 + sw)
            la[:, 0] = 0.0
            la[:, 1] = sgn * 74.0
            la[:, 2] = -sgn * 44.0
            ha[:, 1] = sgn * 10.0
            ha[:, 2] = -sgn * 8.0
            quats[f"{side}UpperArm"] = _slerp_quats(quats[f"{side}UpperArm"], Rotation.from_euler("XYZ", ua, degrees=True).as_quat(), 0.94)
            quats[f"{side}LowerArm"] = _slerp_quats(quats[f"{side}LowerArm"], Rotation.from_euler("XYZ", la, degrees=True).as_quat(), 0.94)
            quats[f"{side}Hand"] = _slerp_quats(quats[f"{side}Hand"], Rotation.from_euler("XYZ", ha, degrees=True).as_quat(), 0.90)

    elif preset == "shy":
        # Cute bashful chest-level hands + gentle shy tilt (face 100% visible, hands clear in front!)
        phase = np.linspace(0.0, 2.0 * math.pi, T, endpoint=False)
        for side, sgn in [("left", 1.0), ("right", -1.0)]:
            ua = np.zeros((T, 3), dtype=np.float64)
            la = np.zeros((T, 3), dtype=np.float64)
            ua[:, 0] = -32.0 + 2.5 * np.sin(phase)
            ua[:, 1] = -sgn * 20.0
            ua[:, 2] = -sgn * 54.0
            la[:, 0] = 16.0
            la[:, 1] = -sgn * 84.0
            la[:, 2] = sgn * 20.0
            quats[f"{side}UpperArm"] = _slerp_quats(quats[f"{side}UpperArm"], Rotation.from_euler("XYZ", ua, degrees=True).as_quat(), 0.86 * env)
            quats[f"{side}LowerArm"] = _slerp_quats(quats[f"{side}LowerArm"], Rotation.from_euler("XYZ", la, degrees=True).as_quat(), 0.88 * env)
        he = Rotation.from_quat(quats["head"]).as_euler("XYZ", degrees=True)
        he[:, 0] = 6.0 * env
        he[:, 1] = -10.0 * env + 5.0 * np.sin(phase) * env
        he[:, 2] = 10.0 * env
        quats["head"] = Rotation.from_euler("XYZ", he, degrees=True).as_quat()

    elif preset == "head_pat":
        # Sweet happy head tilt leaning softly into the pat + cozy chest-height hands
        phase = np.linspace(0.0, 2.0 * math.pi, T, endpoint=False)
        he = Rotation.from_quat(quats["head"]).as_euler("XYZ", degrees=True)
        he[:, 0] = -5.0 * env
        he[:, 1] = 4.0 * np.sin(phase) * env
        he[:, 2] = (17.0 + 4.0 * np.sin(2.0 * phase)) * env
        quats["head"] = Rotation.from_euler("XYZ", he, degrees=True).as_quat()
        for side, sgn in [("left", 1.0), ("right", -1.0)]:
            ua = Rotation.from_euler("XYZ", [-28.0, -sgn * 16.0, -sgn * 56.0], degrees=True).as_quat()
            la = Rotation.from_euler("XYZ", [14.0, -sgn * 76.0, sgn * 16.0], degrees=True).as_quat()
            quats[f"{side}UpperArm"] = _slerp_quats(quats[f"{side}UpperArm"], np.tile(ua, (T, 1)), 0.82 * env)
            quats[f"{side}LowerArm"] = _slerp_quats(quats[f"{side}LowerArm"], np.tile(la, (T, 1)), 0.82 * env)

    elif preset == "peek":
        # Cute curious forward lean + head tilt without toppling sideways
        for bname in ("spine", "chest", "upperChest"):
            e = Rotation.from_quat(quats[bname]).as_euler("XYZ", degrees=True)
            e[:, 2] = np.clip(e[:, 2] * 0.35, -4.5, 4.5)
            e[:, 0] = np.clip(e[:, 0], -4.0, 7.0)
            quats[bname] = Rotation.from_euler("XYZ", e, degrees=True).as_quat()

    elif preset == "deep_breath":
        # Gentle chest inhale upward lift (keep head relaxed/upward, arms breathing softly)
        u = np.linspace(0.0, 1.0, T, dtype=np.float64)
        inhale = np.sin(math.pi * np.clip((u - 0.1) / 0.75, 0.0, 1.0)) ** 1.3
        ce = Rotation.from_quat(quats["chest"]).as_euler("XYZ", degrees=True)
        ce[:, 0] = -7.0 * inhale
        ce[:, 1] *= 0.3
        ce[:, 2] *= 0.3
        quats["chest"] = Rotation.from_euler("XYZ", ce, degrees=True).as_quat()
        he = Rotation.from_quat(quats["head"]).as_euler("XYZ", degrees=True)
        he[:, 0] = np.clip(-6.0 * inhale, -10.0, 4.0)
        he[:, 1] *= 0.3
        he[:, 2] *= 0.3
        quats["head"] = Rotation.from_euler("XYZ", he, degrees=True).as_quat()
        for side, sgn in [("left", 1.0), ("right", -1.0)]:
            ua = Rotation.from_quat(quats[f"{side}UpperArm"]).as_euler("XYZ", degrees=True)
            ua[:, 2] = ua[:, 2] + sgn * 8.0 * inhale
            quats[f"{side}UpperArm"] = Rotation.from_euler("XYZ", ua, degrees=True).as_quat()

    elif preset == "shake_head":
        # Clear polite head shake left-right + waving right hand cleanly in front of chest
        u = np.linspace(0.0, 1.0, T, dtype=np.float64)
        wave_osc = np.sin(5.0 * math.pi * u) * env
        he = Rotation.from_quat(quats["head"]).as_euler("XYZ", degrees=True)
        he[:, 1] = 18.0 * np.sin(4.0 * math.pi * u) * env
        he[:, 0] = np.clip(he[:, 0] * 0.4, -6.0, 6.0)
        quats["head"] = Rotation.from_euler("XYZ", he, degrees=True).as_quat()
        ru = np.zeros((T, 3), dtype=np.float64)
        rl = np.zeros((T, 3), dtype=np.float64)
        rh = np.zeros((T, 3), dtype=np.float64)
        ru[:, 0] = -36.0
        ru[:, 1] = 16.0
        ru[:, 2] = 44.0
        rl[:, 0] = 12.0
        rl[:, 1] = 86.0 + 14.0 * wave_osc
        rl[:, 2] = -26.0
        rh[:, 2] = 12.0 * wave_osc
        quats["rightUpperArm"] = _slerp_quats(quats["rightUpperArm"], Rotation.from_euler("XYZ", ru, degrees=True).as_quat(), 0.85 * env)
        quats["rightLowerArm"] = _slerp_quats(quats["rightLowerArm"], Rotation.from_euler("XYZ", rl, degrees=True).as_quat(), 0.88 * env)
        quats["rightHand"] = _slerp_quats(quats["rightHand"], Rotation.from_euler("XYZ", rh, degrees=True).as_quat(), 0.80 * env)

    elif preset == "explain":
        for side, sgn in [("left", 1.0), ("right", -1.0)]:
            ua = Rotation.from_quat(quats[f"{side}UpperArm"]).as_euler("XYZ", degrees=True)
            ua[:, 0] = np.minimum(ua[:, 0], -22.0 * env + ua[:, 0] * (1.0 - env))
            ua[:, 2] = -sgn * np.minimum(np.abs(ua[:, 2]), 56.0)
            quats[f"{side}UpperArm"] = Rotation.from_euler("XYZ", ua, degrees=True).as_quat()
            la = Rotation.from_quat(quats[f"{side}LowerArm"]).as_euler("XYZ", degrees=True)
            la[:, 1] = -sgn * np.maximum(np.abs(la[:, 1]), 58.0 * env)
            quats[f"{side}LowerArm"] = Rotation.from_euler("XYZ", la, degrees=True).as_quat()


def _enforce_desktop_head_and_spine_framing(quats: dict[str, np.ndarray], preset: str, T: int) -> None:
    """Guarantee ENE's face always stays clearly visible toward the desktop viewer."""
    if preset != "bow":
        for bname, max_x, max_y, max_z in [
            ("spine", 7.0, 10.0, 6.0),
            ("chest", 8.0, 10.0, 6.0),
            ("upperChest", 8.0, 10.0, 6.0),
            ("neck", 8.0, 12.0, 8.0),
        ]:
            e = Rotation.from_quat(quats[bname]).as_euler("XYZ", degrees=True)
            e[:, 0] = np.clip(e[:, 0], -max_x, max_x)
            e[:, 1] = np.clip(e[:, 1], -max_y, max_y)
            e[:, 2] = np.clip(e[:, 2], -max_z, max_z)
            quats[bname] = Rotation.from_euler("XYZ", e, degrees=True).as_quat()

    pitch_min, pitch_max = (-16.0, 38.0) if preset == "bow" else (-15.0, 12.0)
    yaw_max = 22.0
    roll_max = 20.0

    head_corr = np.zeros((T, 3), dtype=np.float64)
    chain_mats = {b: Rotation.from_quat(quats[b]).as_matrix() for b in ("hips", "spine", "chest", "upperChest", "neck", "head")}
    for t in range(T):
        r_neck_g = chain_mats["hips"][t] @ chain_mats["spine"][t] @ chain_mats["chest"][t] @ chain_mats["upperChest"][t] @ chain_mats["neck"][t]
        r_head_g = r_neck_g @ chain_mats["head"][t]
        yaw, pitch, roll = Rotation.from_matrix(r_head_g).as_euler("YXZ", degrees=True)
        c_yaw = float(np.clip(yaw, -yaw_max, yaw_max))
        c_pitch = float(np.clip(pitch, pitch_min, pitch_max))
        c_roll = float(np.clip(roll, -roll_max, roll_max))
        if abs(c_yaw - yaw) > 0.1 or abs(c_pitch - pitch) > 0.1 or abs(c_roll - roll) > 0.1:
            r_target_g = Rotation.from_euler("YXZ", [c_yaw, c_pitch, c_roll], degrees=True).as_matrix()
            r_head_new = r_neck_g.T @ r_target_g
            head_corr[t] = Rotation.from_matrix(r_head_new).as_euler("XYZ", degrees=True)
        else:
            head_corr[t] = Rotation.from_matrix(chain_mats["head"][t]).as_euler("XYZ", degrees=True)

    head_corr = _smooth_1d(head_corr, radius=3)
    quats["head"] = Rotation.from_euler("XYZ", head_corr, degrees=True).as_quat()


def _enforce_ene_arm_clearance(
    quats: dict[str, np.ndarray],
    posture: str,
    preset: str,
    T: int,
) -> None:
    """Run exact ENE.vrm FK per frame and push elbows/hands out of coat, skirt, torso & twin-buns."""
    if preset == "idle_hands_behind":
        # Hands are intentionally clasped behind the waist; skip outward side-cone push
        return

    bone_keys = [
        "hips", "spine", "chest", "upperChest", "neck", "head",
        "leftShoulder", "leftUpperArm", "leftLowerArm", "leftHand",
        "rightShoulder", "rightUpperArm", "rightLowerArm", "rightHand",
        "leftUpperLeg", "leftLowerLeg", "leftFoot",
        "rightUpperLeg", "rightLowerLeg", "rightFoot",
    ]

    dz_corr = {"left": np.zeros(T, dtype=np.float64), "right": np.zeros(T, dtype=np.float64)}
    dx_corr = {"left": np.zeros(T, dtype=np.float64), "right": np.zeros(T, dtype=np.float64)}

    all_mats = {b: Rotation.from_quat(quats[b]).as_matrix() for b in bone_keys}
    hips_rest = ENE_OFFSETS["hips"]

    for t in range(T):
        rots_t = {b: all_mats[b][t] for b in bone_keys}
        pos, _ = _ene_fk_frame(rots_t, hips_rest)
        chest_c = pos["chest"]
        hips_c = pos["hips"]
        head_c = pos["head"] + np.array([0.0, 0.085, 0.0])

        for side, sgn in [("left", 1.0), ("right", -1.0)]:
            elbow = pos[f"{side}LowerArm"]
            hand = pos[f"{side}Hand"]
            mid_forearm = 0.5 * (elbow + hand)

            max_pen = 0.0
            need_fwd = 0.0
            for pt, pt_rad in [(elbow, 0.022), (mid_forearm, 0.022), (hand, 0.024)]:
                y = float(pt[1])
                if 0.36 <= y <= 0.87:
                    frac = float(np.clip((0.86 - y) / (0.86 - 0.48), 0.0, 1.0))
                    # Tighter realistic ENE.vrm coat/skirt profile (keeps relaxed arms graceful, not A-pose!)
                    cone_r = 0.100 + 0.068 * frac
                    axis_xz = (1.0 - frac) * chest_c[[0, 2]] + frac * hips_c[[0, 2]]
                    rel_x = float(pt[0] - axis_xz[0])
                    rel_z = float(pt[2] - axis_xz[1])
                    in_front_chest = (rel_z > 0.095 and y > 0.66)
                    if not in_front_chest:
                        dist_xz = math.hypot(rel_x, rel_z)
                        min_r = cone_r + pt_rad
                        if dist_xz < min_r:
                            pen = min_r - dist_xz
                            if pen > max_pen:
                                max_pen = pen
                            if y < 0.72 and rel_z > -0.02:
                                need_fwd = max(need_fwd, pen)

                if posture == "seated":
                    thigh_mid = 0.5 * (pos[f"{side}UpperLeg"] + pos[f"{side}LowerLeg"])
                    d_thigh = float(np.linalg.norm(pt - thigh_mid))
                    if d_thigh < 0.095:
                        max_pen = max(max_pen, 0.095 - d_thigh)

                allow_near_face = (preset in ("yawn", "listen") and side == "right")
                if not allow_near_face:
                    d_head = float(np.linalg.norm(pt - head_c))
                    if d_head < 0.155:
                        max_pen = max(max_pen, 0.155 - d_head)

            if max_pen > 0.0:
                deg_out = min(22.0, max_pen * 210.0)
                dz_corr[side][t] = sgn * deg_out
                if need_fwd > 0.0:
                    dx_corr[side][t] = -min(8.0, need_fwd * 75.0)

    for side, sgn in [("left", 1.0), ("right", -1.0)]:
        dz = _smooth_1d(dz_corr[side], radius=4)
        dx = _smooth_1d(dx_corr[side], radius=4)
        orig = Rotation.from_quat(quats[f"{side}UpperArm"])
        out = np.empty((T, 4), dtype=np.float64)
        for t in range(T):
            delta = Rotation.from_euler("XYZ", [dx[t], 0.0, dz[t]], degrees=True)
            r_new = delta * orig[t]
            e = r_new.as_euler("XYZ", degrees=True)
            # Match idle-breath.vrma's natural relaxed arm hang (~69 deg max drop)
            if side == "left" and e[2] < -69.0:
                e[2] = -69.0
                r_new = Rotation.from_euler("XYZ", e, degrees=True)
            elif side == "right" and e[2] > 69.0:
                e[2] = 69.0
                r_new = Rotation.from_euler("XYZ", e, degrees=True)
            out[t] = r_new.as_quat()
        quats[f"{side}UpperArm"] = out


def _apply_hand_style(
    quats_by_bone: dict[str, np.ndarray],
    hand_style: str,
    T: int,
) -> None:
    if hand_style == "relaxed":
        return

    env = _window_envelope(T, 0.08, 0.92, ramp_ratio=0.14)

    def blend_euler(bone: str, deg_xyz: tuple[float, float, float]) -> None:
        if bone not in quats_by_bone:
            return
        target = Rotation.from_euler("XYZ", deg_xyz, degrees=True)
        orig = Rotation.from_quat(quats_by_bone[bone])
        out = np.empty((T, 4), dtype=np.float64)
        for t in range(T):
            w = float(env[t])
            if w <= 1e-5:
                out[t] = quats_by_bone[bone][t]
            else:
                s = Slerp([0.0, 1.0], Rotation.concatenate([orig[t], target]))
                out[t] = s([w])[0].as_quat()
        quats_by_bone[bone] = out

    if hand_style == "open":
        for side, sgn in [("left", -1.0), ("right", 1.0)]:
            for finger in ("Index", "Middle", "Ring", "Little"):
                blend_euler(f"{side}{finger}Proximal", (0.0, 0.0, sgn * 4.5))
                blend_euler(f"{side}{finger}Intermediate", (0.0, 0.0, sgn * 6.5))
                blend_euler(f"{side}{finger}Distal", (0.0, 0.0, sgn * 4.0))
            blend_euler(f"{side}ThumbProximal", (2.0, -sgn * 4.0, sgn * 3.0))
    elif hand_style == "fist":
        for side, sgn in [("left", -1.0), ("right", 1.0)]:
            for finger in ("Index", "Middle", "Ring", "Little"):
                blend_euler(f"{side}{finger}Proximal", (0.0, 0.0, sgn * 48.0))
                blend_euler(f"{side}{finger}Intermediate", (0.0, 0.0, sgn * 68.0))
                blend_euler(f"{side}{finger}Distal", (0.0, 0.0, sgn * 42.0))
            blend_euler(f"{side}ThumbMetacarpal", (8.0, -sgn * 14.0, sgn * 10.0))
            blend_euler(f"{side}ThumbProximal", (12.0, -sgn * 20.0, sgn * 16.0))
            blend_euler(f"{side}ThumbDistal", (6.0, -sgn * 10.0, sgn * 14.0))
    elif hand_style == "point_right":
        blend_euler("rightIndexProximal", (0.0, 0.0, 2.0))
        blend_euler("rightIndexIntermediate", (0.0, 0.0, 3.0))
        blend_euler("rightIndexDistal", (0.0, 0.0, 2.0))
        for finger in ("Middle", "Ring", "Little"):
            blend_euler(f"right{finger}Proximal", (0.0, 0.0, 52.0))
            blend_euler(f"right{finger}Intermediate", (0.0, 0.0, 70.0))
            blend_euler(f"right{finger}Distal", (0.0, 0.0, 44.0))
        blend_euler("rightThumbMetacarpal", (10.0, -16.0, 12.0))
        blend_euler("rightThumbProximal", (12.0, -22.0, 16.0))


def _make_loop_seamless(
    quats_by_bone: dict[str, np.ndarray],
    root_positions: np.ndarray,
    blend_frames: int = 14,
) -> tuple[dict[str, np.ndarray], np.ndarray]:
    T = root_positions.shape[0]
    k = min(blend_frames, max(4, T // 6))
    w = _smoothstep(np.linspace(0.0, 1.0, k))
    root_out = root_positions.copy()
    for i in range(k):
        ti = T - k + i
        root_out[ti] = (1.0 - w[i]) * root_out[ti] + w[i] * root_out[0]
    root_out[-1] = root_out[0]

    for bname, q in quats_by_bone.items():
        q_out = q.copy()
        r0 = Rotation.from_quat(q_out[0])
        for i in range(k):
            ti = T - k + i
            ri = Rotation.from_quat(q_out[ti])
            q_out[ti] = Slerp([0.0, 1.0], Rotation.concatenate([ri, r0]))([float(w[i])])[0].as_quat()
        q_out[-1] = q_out[0]
        quats_by_bone[bname] = q_out
    return quats_by_bone, root_out


def _ensure_quat_continuity(q: np.ndarray) -> np.ndarray:
    q = q.astype(np.float64, copy=True)
    norms = np.linalg.norm(q, axis=-1, keepdims=True)
    norms = np.maximum(norms, 1e-8)
    q /= norms
    for t in range(1, len(q)):
        if float(np.dot(q[t], q[t - 1])) < 0.0:
            q[t] = -q[t]
    return q.astype(np.float32)


def _build_blink_curve(T: int, fps: int) -> np.ndarray:
    times = np.arange(T, dtype=np.float64) / float(fps)
    dur = times[-1] if T > 1 else 1.0
    curve = np.zeros(T, dtype=np.float32)
    blink_times = []
    t_cur = min(1.4, 0.32 * dur)
    while t_cur < dur - 0.55:
        blink_times.append(t_cur)
        t_cur += 2.65
    if not blink_times and dur >= 1.2:
        blink_times.append(0.5 * dur)
    half_w = 0.08
    for bt in blink_times:
        dt = np.abs(times - bt)
        mask = dt < half_w
        vals = 0.98 * (0.5 * (1.0 + np.cos(math.pi * dt[mask] / half_w)))
        curve[mask] = np.maximum(curve[mask], vals.astype(np.float32))
    curve[0] = 0.0
    curve[-1] = 0.0
    return curve


def _build_gaze_quats(head_quats: np.ndarray, gaze_mode: str, T: int) -> np.ndarray:
    eul = Rotation.from_quat(head_quats).as_euler("YXZ", degrees=True)
    out = np.zeros((T, 4), dtype=np.float32)
    phase = np.linspace(0.0, 2.0 * math.pi, T, endpoint=False)
    for t in range(T):
        yaw, pitch = float(eul[t, 0]), float(eul[t, 1])
        if gaze_mode == "follow_head":
            gy = float(np.clip(0.30 * yaw, -7.0, 7.0))
            gp = float(np.clip(0.20 * pitch, -4.0, 4.0))
        elif gaze_mode == "shy":
            env = math.sin(0.5 * phase[t])
            gy = float(np.clip(-6.0 * env, -7.0, 7.0))
            gp = float(np.clip(-2.5 * env, -4.0, 4.0))
        else:
            gy = float(np.clip(-0.22 * yaw, -6.0, 6.0))
            gp = float(np.clip(-0.18 * pitch, -4.0, 4.0))
        out[t] = Rotation.from_euler("YXZ", [gy, gp, 0.0], degrees=True).as_quat()
    return _ensure_quat_continuity(out)


def _pack_glb(gltf_json: dict[str, Any], bin_bytes: bytes) -> bytes:
    json_raw = json.dumps(gltf_json, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    json_pad = (4 - (len(json_raw) % 4)) % 4
    json_chunk = json_raw + b" " * json_pad

    bin_pad = (4 - (len(bin_bytes) % 4)) % 4
    bin_chunk = bin_bytes + b"\x00" * bin_pad

    total_len = 12 + 8 + len(json_chunk) + 8 + len(bin_chunk)
    header = struct.pack("<III", 0x46546C67, 2, total_len)
    c0 = struct.pack("<II", len(json_chunk), 0x4E4F534A)
    c1 = struct.pack("<II", len(bin_chunk), 0x004E4942)
    return header + c0 + json_chunk + c1 + bin_chunk


def convert_npz_to_vrma(npz_path: Path, out_path: Path, entry: dict[str, Any]) -> None:
    data = np.load(npz_path, allow_pickle=False)
    local_rots = data["local_rot_mats"].astype(np.float64)
    posed_joints = data["posed_joints"].astype(np.float64)
    root_positions = data["root_positions"].astype(np.float64)
    neutral_joints = data["neutral_joints"].astype(np.float64)
    bone_names = [str(x) for x in data["bone_order_names"]]
    fps = int(data["fps"]) if "fps" in data else 30

    bidx = {name: i for i, name in enumerate(bone_names)}
    T = local_rots.shape[0]
    preset = entry["preset"]
    posture = entry["posture"]

    unit_scale = 0.01 if abs(neutral_joints[bidx["Hips"], 1]) > 10.0 else 1.0
    neutral_joints = neutral_joints * unit_scale
    posed_joints = posed_joints * unit_scale
    root_positions = root_positions * unit_scale

    # Align yaw so character faces +Z
    if entry["mode"] == "loop" or posture == "dangling":
        angles = [_compute_yaw_angle(posed_joints, bidx, t) for t in range(0, T, max(1, T // 10))]
        yaw = float(math.atan2(np.mean(np.sin(angles)), np.mean(np.cos(angles))))
    else:
        yaw = _compute_yaw_angle(posed_joints, bidx, 0)
    c, s = math.cos(-yaw), math.sin(-yaw)
    r_yaw = np.array([[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]], dtype=np.float64)

    local_rots[:, 0] = r_yaw[None, :, :] @ local_rots[:, 0]
    root_positions = (r_yaw @ root_positions.T).T

    # Center XZ cleanly
    ref_xz = np.mean(root_positions[:, [0, 2]], axis=0) if entry["mode"] == "loop" else root_positions[0, [0, 2]]
    root_positions[:, 0] -= ref_xz[0]
    root_positions[:, 2] -= ref_xz[1]
    xz_r = np.linalg.norm(root_positions[:, [0, 2]], axis=-1)
    max_r = 0.09
    over = xz_r > max_r
    if np.any(over):
        scale = max_r / xz_r[over]
        root_positions[over, 0] *= scale
        root_positions[over, 2] *= scale

    h_rest = float(neutral_joints[bidx["Hips"], 1])
    if h_rest <= 0.2:
        toe_min_y = float(
            min(
                neutral_joints[bidx["LeftToeBase"], 1],
                neutral_joints[bidx["RightToeBase"], 1],
                neutral_joints[bidx["LeftFoot"], 1] - 0.05,
                neutral_joints[bidx["RightFoot"], 1] - 0.05,
            )
        )
        h_rest = max(0.85, -toe_min_y)
    hips_neutral_origin = neutral_joints[bidx["Hips"]].copy()

    if posture == "standing":
        foot_min = np.minimum(
            np.minimum(posed_joints[:, bidx["LeftToeBase"], 1], posed_joints[:, bidx["RightToeBase"], 1]),
            np.minimum(posed_joints[:, bidx["LeftFoot"], 1], posed_joints[:, bidx["RightFoot"], 1]) - 0.04,
        )
        ground_err = float(np.percentile(foot_min, 5))
        if abs(ground_err) > 0.005:
            root_positions[:, 1] -= ground_err

    # Map SOMA-77 local rotation matrices -> VRM 52-bone [x, y, z, w] quaternions
    quats_by_bone: dict[str, np.ndarray] = {}
    for vrm_bone, soma_joint in DIRECT_SOMA_MAP.items():
        quats_by_bone[vrm_bone] = Rotation.from_matrix(local_rots[:, bidx[soma_joint]]).as_quat()
    for vrm_bone, (j1, j2) in COMPOSED_SOMA_MAP.items():
        quats_by_bone[vrm_bone] = Rotation.from_matrix(local_rots[:, bidx[j1]] @ local_rots[:, bidx[j2]]).as_quat()

    # === ENE.vrm-aware retargeting & kinematic safety pipeline ===
    _refine_shoulders(quats_by_bone, T)

    if posture == "standing":
        _refine_standing_legs(quats_by_bone, T)
    elif posture == "seated":
        root_positions = _refine_seated_modest_pose(quats_by_bone, root_positions, preset, h_rest, T)
    elif posture == "dangling":
        root_positions = _refine_drag_held_pose(quats_by_bone, root_positions, h_rest, T)

    _refine_preset_specific(quats_by_bone, preset, T)
    _enforce_desktop_head_and_spine_framing(quats_by_bone, preset, T)
    _enforce_ene_arm_clearance(quats_by_bone, posture, preset, T)
    _apply_hand_style(quats_by_bone, entry.get("hand_style", "relaxed"), T)

    if entry["mode"] == "loop":
        quats_by_bone, root_positions = _make_loop_seamless(quats_by_bone, root_positions, blend_frames=14)

    for bname in list(quats_by_bone):
        quats_by_bone[bname] = _ensure_quat_continuity(quats_by_bone[bname])

    # Build VRM rest positions relative to ground (hips at [0, h_rest, 0])
    hips_target = np.array([0.0, h_rest, 0.0], dtype=np.float64)
    rest_world: dict[str, np.ndarray] = {}
    for vrm_bone, _, soma_pos_name in VRM_BONES:
        rest_world[vrm_bone] = (neutral_joints[bidx[soma_pos_name]] - hips_neutral_origin) + hips_target
    rest_world["hips"] = hips_target

    bone_idx_map = {b: idx for idx, (b, _, _) in enumerate(VRM_BONES)}
    children_map: dict[int, list[int]] = {i: [] for i in range(len(VRM_BONES))}
    for idx, (_, parent_name, _) in enumerate(VRM_BONES):
        if parent_name is not None:
            children_map[bone_idx_map[parent_name]].append(idx)

    nodes: list[dict[str, Any]] = []
    human_bones: dict[str, dict[str, int]] = {}
    for idx, (vrm_bone, parent_name, _) in enumerate(VRM_BONES):
        trans = rest_world[vrm_bone] if parent_name is None else rest_world[vrm_bone] - rest_world[parent_name]
        node: dict[str, Any] = {
            "name": vrm_bone,
            "translation": [float(trans[0]), float(trans[1]), float(trans[2])],
        }
        if children_map[idx]:
            node["children"] = children_map[idx]
        nodes.append(node)
        human_bones[vrm_bone] = {"node": idx}

    preset_exprs: dict[str, dict[str, int]] = {}
    expr_curves: list[tuple[int, np.ndarray]] = []
    scene_roots = [0]

    for expr_spec in entry.get("expressions", []):
        ename = expr_spec["name"]
        node_idx = len(nodes)
        nodes.append({"name": f"Expression_{ename}"})
        preset_exprs[ename] = {"node": node_idx}
        scene_roots.append(node_idx)
        env = _window_envelope(T, float(expr_spec["start_ratio"]), float(expr_spec["end_ratio"]))
        expr_curves.append((node_idx, (float(expr_spec["peak"]) * env).astype(np.float32)))

    blink_idx = len(nodes)
    nodes.append({"name": "Expression_blink"})
    preset_exprs["blink"] = {"node": blink_idx}
    scene_roots.append(blink_idx)
    expr_curves.append((blink_idx, _build_blink_curve(T, fps)))

    gaze_idx = len(nodes)
    nodes.append({"name": "Gaze"})
    scene_roots.append(gaze_idx)
    gaze_quats = _build_gaze_quats(quats_by_bone["head"], entry.get("gaze_mode", "follow_head"), T)

    buf = bytearray()
    buffer_views: list[dict[str, Any]] = []
    accessors: list[dict[str, Any]] = []

    def append_accessor(arr: np.ndarray, acc_type: str) -> int:
        arr = np.ascontiguousarray(arr, dtype=np.float32)
        raw = arr.tobytes()
        offset = len(buf)
        buf.extend(raw)
        pad = (4 - (len(buf) % 4)) % 4
        if pad:
            buf.extend(b"\x00" * pad)
        bv_idx = len(buffer_views)
        buffer_views.append({"buffer": 0, "byteOffset": offset, "byteLength": len(raw)})
        acc: dict[str, Any] = {
            "bufferView": bv_idx,
            "componentType": 5126,
            "count": int(arr.shape[0]),
            "type": acc_type,
        }
        if acc_type == "SCALAR":
            acc["min"] = [float(np.min(arr))]
            acc["max"] = [float(np.max(arr))]
        else:
            acc["min"] = [float(x) for x in np.min(arr, axis=0)]
            acc["max"] = [float(x) for x in np.max(arr, axis=0)]
        acc_idx = len(accessors)
        accessors.append(acc)
        return acc_idx

    times = np.arange(T, dtype=np.float32) / float(fps)
    time_acc = append_accessor(times, "SCALAR")

    samplers: list[dict[str, Any]] = []
    channels: list[dict[str, Any]] = []

    def add_channel(node_idx: int, path: str, values: np.ndarray, acc_type: str) -> None:
        val_acc = append_accessor(values, acc_type)
        s_idx = len(samplers)
        samplers.append({"input": time_acc, "interpolation": "LINEAR", "output": val_acc})
        channels.append({"sampler": s_idx, "target": {"node": node_idx, "path": path}})

    add_channel(0, "translation", root_positions.astype(np.float32), "VEC3")
    for idx, (vrm_bone, _, _) in enumerate(VRM_BONES):
        add_channel(idx, "rotation", quats_by_bone[vrm_bone], "VEC4")

    for node_idx, curve in expr_curves:
        vec = np.zeros((T, 3), dtype=np.float32)
        vec[:, 0] = curve
        add_channel(node_idx, "translation", vec, "VEC3")

    add_channel(gaze_idx, "rotation", gaze_quats, "VEC4")

    gltf = {
        "asset": {"version": "2.0", "generator": "ENE Kimodo SOMA-to-VRMA"},
        "extensionsUsed": ["VRMC_vrm_animation"],
        "extensions": {
            "VRMC_vrm_animation": {
                "specVersion": "1.0",
                "humanoid": {"humanBones": human_bones},
                "expressions": {"preset": preset_exprs},
                "lookAt": {"node": gaze_idx, "offsetFromHeadBone": [0.0, 0.05, 0.03]},
            }
        },
        "scene": 0,
        "scenes": [{"nodes": scene_roots}],
        "nodes": nodes,
        "animations": [{"name": preset, "channels": channels, "samplers": samplers}],
        "accessors": accessors,
        "bufferViews": buffer_views,
        "buffers": [{"byteLength": len(buf)}],
    }

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_bytes(_pack_glb(gltf, bytes(buf)))


def validate_vrma(path: Path, expect_loop: bool = False) -> tuple[bool, str]:
    raw = path.read_bytes()
    if len(raw) < 20:
        return False, "file too short"
    magic, ver, length = struct.unpack("<III", raw[:12])
    if magic != 0x46546C67 or ver != 2 or length != len(raw):
        return False, f"invalid GLB header (magic={magic:#x}, ver={ver}, len={length}/{len(raw)})"
    c0_len, c0_type = struct.unpack("<II", raw[12:20])
    if c0_type != 0x4E4F534A:
        return False, "first chunk is not JSON"
    gltf = json.loads(raw[20 : 20 + c0_len])
    c1_off = 20 + c0_len
    c1_len, c1_type = struct.unpack("<II", raw[c1_off : c1_off + 8])
    if c1_type != 0x004E4942:
        return False, "second chunk is not BIN"
    bin_data = raw[c1_off + 8 : c1_off + 8 + c1_len]

    ext = gltf.get("extensions", {}).get("VRMC_vrm_animation")
    if not ext or ext.get("specVersion") != "1.0":
        return False, "missing VRMC_vrm_animation 1.0"
    bones = ext.get("humanoid", {}).get("humanBones", {})
    if "hips" not in bones or "head" not in bones:
        return False, "missing required humanoid bones"

    names = [n.get("name") for n in gltf.get("nodes", [])]
    if any(not n for n in names) or len(names) != len(set(names)):
        return False, "node names must be present and unique"

    nodes = gltf["nodes"]
    visited = set()
    stack = list(gltf["scenes"][gltf.get("scene", 0)]["nodes"])
    while stack:
        ni = stack.pop()
        if ni in visited:
            continue
        visited.add(ni)
        stack.extend(nodes[ni].get("children", []))
    if len(visited) != len(nodes):
        return False, f"unreachable nodes in scene 0 ({len(visited)}/{len(nodes)})"

    def read_acc(idx: int) -> np.ndarray:
        acc = gltf["accessors"][idx]
        bv = gltf["bufferViews"][acc["bufferView"]]
        off = bv.get("byteOffset", 0) + acc.get("byteOffset", 0)
        comps = {"SCALAR": 1, "VEC2": 2, "VEC3": 3, "VEC4": 4}[acc["type"]]
        arr = np.frombuffer(bin_data, dtype=np.float32, count=acc["count"] * comps, offset=off)
        return arr.reshape(acc["count"], comps) if comps > 1 else arr

    anims = gltf.get("animations", [])
    if not anims:
        return False, "no animations"
    anim = anims[0]
    dur = 0.0
    max_seam = 0.0
    for ch in anim["channels"]:
        samp = anim["samplers"][ch["sampler"]]
        t = read_acc(samp["input"])
        v = read_acc(samp["output"])
        if not np.all(np.isfinite(t)) or not np.all(np.isfinite(v)):
            return False, f"non-finite values in {nodes[ch['target']['node']]['name']}"
        if len(t) > 1 and np.any(np.diff(t) <= 0):
            return False, "timestamps not strictly increasing"
        dur = max(dur, float(t[-1]))
        if ch["target"]["path"] == "rotation":
            norms = np.linalg.norm(v, axis=-1)
            if np.max(np.abs(norms - 1.0)) > 1e-3:
                return False, f"non-unit quaternion in {nodes[ch['target']['node']]['name']}"
            if expect_loop and len(v) > 1:
                seam = 1.0 - abs(float(np.dot(v[0], v[-1])))
                max_seam = max(max_seam, seam)
        elif expect_loop and ch["target"]["path"] == "translation" and ch["target"]["node"] == 0 and len(v) > 1:
            seam_t = float(np.linalg.norm(v[0] - v[-1]))
            if seam_t > 1e-3:
                return False, f"loop translation seam {seam_t:.4f}"
    if expect_loop and max_seam > 1e-3:
        return False, f"loop rotation seam {max_seam:.4f}"

    return True, f"ok (bones={len(bones)}, ch={len(anim['channels'])}, dur={dur:.2f}s)"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--catalog", type=Path, default=Path("scripts/vrma-catalog.json"))
    parser.add_argument("--npz-dir", type=Path)
    parser.add_argument("--out-dir", type=Path, default=Path("assets/vrma"))
    parser.add_argument("--validate", type=Path, help="Validate all .vrma files in directory")
    args = parser.parse_args()

    catalog = json.loads(args.catalog.read_text(encoding="utf-8"))
    loop_files = {e["file"] for e in catalog["motions"] if e["mode"] == "loop"}

    if args.validate:
        files = sorted(args.validate.glob("*.vrma"))
        failed = 0
        for p in files:
            ok, msg = validate_vrma(p, expect_loop=(p.name in loop_files))
            print(f"{'PASS' if ok else 'FAIL'} {p.name:26s} {msg}")
            if not ok:
                failed += 1
        if failed:
            raise SystemExit(f"{failed} VRMA file(s) failed validation")
        print(f"Validated {len(files)} VRMA file(s) cleanly.")
        return

    if not args.npz_dir:
        parser.error("--npz-dir is required unless --validate is used")

    for entry in catalog["motions"]:
        npz_path = args.npz_dir / f"{entry['preset']}.npz"
        if not npz_path.is_file():
            raise FileNotFoundError(f"Missing generated NPZ: {npz_path}")
        out_path = args.out_dir / entry["file"]
        convert_npz_to_vrma(npz_path, out_path, entry)
        ok, msg = validate_vrma(out_path, expect_loop=(entry["mode"] == "loop"))
        print(f"[convert] {entry['preset']:20s} -> {out_path.name:24s} [{msg}]")
        if not ok:
            raise RuntimeError(f"Validation failed for {out_path}: {msg}")


if __name__ == "__main__":
    main()
