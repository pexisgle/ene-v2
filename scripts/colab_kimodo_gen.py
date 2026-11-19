#!/usr/bin/env python3
"""Generate SOMA-77 motions with NVIDIA Kimodo on a Colab GPU runtime."""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
import tarfile
from pathlib import Path

if os.path.isdir("/content/kimodo") and "/content/kimodo" not in sys.path:
    sys.path.insert(0, "/content/kimodo")
from typing import Any

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
import torch

if not hasattr(sys, "_kimodo_model_cache"):
    sys._kimodo_model_cache = {}
_MODEL_CACHE: dict[str, Any] = sys._kimodo_model_cache


def _configure_env() -> None:
    os.environ.setdefault("TEXT_ENCODER_MODE", "local")
    os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")
    if torch.cuda.is_available():
        vram_gb = torch.cuda.get_device_properties(0).total_memory / (1024**3)
        if vram_gb < 21.0:
            os.environ.setdefault("TEXT_ENCODER_DEVICE", "cpu")
            print(f"[kimodo] VRAM={vram_gb:.1f} GB (<21 GB); offloading text encoder to CPU.")
        else:
            print(f"[kimodo] GPU={torch.cuda.get_device_name(0)} VRAM={vram_gb:.1f} GB")
    else:
        raise RuntimeError("CUDA GPU is required for Kimodo motion generation.")


def get_model(model_name: str):
    if model_name in _MODEL_CACHE:
        return _MODEL_CACHE[model_name]
    _configure_env()
    from kimodo import load_model
    from kimodo.tools import seed_everything

    seed_everything(42)
    print(f"[kimodo] Loading model {model_name} ...")
    model = load_model(modelname=model_name, device="cuda")
    model.eval()
    _MODEL_CACHE[model_name] = model
    print(f"[kimodo] Loaded {model_name}; skeleton={model.skeleton.name} ({model.skeleton.nbjoints}j), out={model.output_skeleton.name} ({model.output_skeleton.nbjoints}j), fps={model.fps}")
    return model


def _bone_index(skeleton) -> dict[str, int]:
    return {name: idx for idx, name in enumerate(skeleton.bone_order_names)}


def _compute_yaw_error_rad(posed_joints_77: np.ndarray, bidx: dict[str, int]) -> float:
    """Measure how far forward vector deviates from +Z (radians, 0 = facing +Z)."""
    l_hip = posed_joints_77[bidx["LeftLeg"]]
    r_hip = posed_joints_77[bidx["RightLeg"]]
    l_sh = posed_joints_77[bidx["LeftArm"]]
    r_sh = posed_joints_77[bidx["RightArm"]]
    left_vec = 0.5 * ((l_hip - r_hip) + (l_sh - r_sh))
    # Right-handed +Y up, +X = character's left, +Z = forward => forward = left_vec x up
    up = np.array([0.0, 1.0, 0.0], dtype=np.float64)
    fwd = np.cross(np.array([left_vec[0], 0.0, left_vec[2]], dtype=np.float64), up)
    norm = np.linalg.norm(fwd)
    if norm < 1e-6:
        return 0.0
    fwd /= norm
    return float(math.atan2(fwd[0], fwd[2]))


def _knee_flexion_deg(posed_joints_77: np.ndarray, bidx: dict[str, int]) -> float:
    """Average knee bend angle in degrees (0 = straight leg, 90 = right-angle sit)."""
    angles = []
    for hip_k, knee_k, ankle_k in [
        ("LeftLeg", "LeftShin", "LeftFoot"),
        ("RightLeg", "RightShin", "RightFoot"),
    ]:
        thigh = posed_joints_77[bidx[knee_k]] - posed_joints_77[bidx[hip_k]]
        shin = posed_joints_77[bidx[ankle_k]] - posed_joints_77[bidx[knee_k]]
        t_n = np.linalg.norm(thigh)
        s_n = np.linalg.norm(shin)
        if t_n > 1e-6 and s_n > 1e-6:
            cos_a = float(np.clip(np.dot(thigh / t_n, shin / s_n), -1.0, 1.0))
            angles.append(math.degrees(math.acos(cos_a)))
    return float(np.mean(angles)) if angles else 0.0


def _recenter_and_align_pose(
    local_rot_77_frame: torch.Tensor,
    root_pos_frame: torch.Tensor,
    out_skel,
    bidx: dict[str, int],
) -> tuple[torch.Tensor, torch.Tensor]:
    """Rotate Hips around +Y so the single pose faces +Z and center root XZ at (0, 0)."""
    local_rot = local_rot_77_frame.clone().unsqueeze(0)
    root_pos = root_pos_frame.clone().unsqueeze(0)
    root_pos[0, 0] = 0.0
    root_pos[0, 2] = 0.0
    _, posed, _ = out_skel.fk(local_rot, root_pos)
    yaw = _compute_yaw_error_rad(posed[0].detach().cpu().numpy(), bidx)
    c = math.cos(-yaw)
    s = math.sin(-yaw)
    r_yaw = torch.tensor(
        [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]],
        device=local_rot.device,
        dtype=local_rot.dtype,
    )
    local_rot[0, 0] = r_yaw @ local_rot[0, 0]
    return local_rot[0], root_pos[0]


def _make_fullbody_anchor(
    model,
    pose_local_77: torch.Tensor,
    pose_root: torch.Tensor,
    frame_indices: list[int],
):
    """Build a FullBodyConstraintSet on model.skeleton for the given anchor frames."""
    from kimodo.constraints import FullBodyConstraintSet

    skel = model.skeleton
    if skel.name == "somaskel30":
        local_30 = skel.from_SOMASkeleton77(pose_local_77.unsqueeze(0))[0]
    else:
        local_30 = pose_local_77

    k = len(frame_indices)
    local_k = local_30.unsqueeze(0).repeat(k, 1, 1, 1)
    root_k = pose_root.unsqueeze(0).repeat(k, 1)
    global_rots_k, posed_k, _ = skel.fk(local_k, root_k)
    idx_tensor = torch.tensor(frame_indices, device=model.device, dtype=torch.long)
    return FullBodyConstraintSet(
        skel,
        idx_tensor,
        posed_k,
        global_rots_k,
    )


def _make_root2d_anchor(model, frame_indices: list[int], heading_vec: torch.Tensor | None = None):
    from kimodo.constraints import Root2DConstraintSet

    idx_tensor = torch.tensor(frame_indices, device=model.device, dtype=torch.long)
    xz = torch.zeros((len(frame_indices), 2), device=model.device, dtype=torch.float32)
    heading = None
    if heading_vec is not None:
        heading = heading_vec.to(device=model.device, dtype=torch.float32).reshape(1, 2).repeat(len(frame_indices), 1)
    return Root2DConstraintSet(model.skeleton, idx_tensor, xz, global_root_heading=heading)


def _build_canonical_pose(model, posture: str, cache_dir: Path) -> tuple[torch.Tensor, torch.Tensor]:
    """Generate and select a clean canonical standing or seated pose."""
    from kimodo.tools import seed_everything

    cache_file = cache_dir / f"_canonical_{posture}.pt"
    if cache_file.exists():
        data = torch.load(cache_file, map_location=model.device, weights_only=True)
        return data["local_rot_77"], data["root_pos"]

    out_skel = model.output_skeleton
    bidx = _bone_index(out_skel)

    if posture == "standing":
        prompt = "A person stands still in a relaxed natural upright posture facing forward with both arms resting comfortably at their sides and feet flat on the floor."
        num_frames = 75
        seed_everything(7701)
    elif posture == "seated":
        prompt = "A person sits upright on the edge of a high bench facing forward with knees bent at ninety degrees, both legs hanging down off the edge, and hands resting lightly near their thighs."
        num_frames = 105
        seed_everything(7702)
    else:
        raise ValueError(f"Unsupported canonical posture: {posture}")

    num_samples = 4
    output = model(
        prompt,
        num_frames,
        num_denoising_steps=100,
        num_samples=num_samples,
        first_heading_angle=torch.zeros(num_samples, device=model.device),
        post_processing=(posture == "standing"),
        return_numpy=False,
    )

    best_score = float("inf")
    best_pose = None
    best_root = None

    local_rots = output["local_rot_mats"]  # [B, T, 77, 3, 3]
    root_pos = output["root_positions"]  # [B, T, 3]
    posed_np = output["posed_joints"].detach().cpu().numpy()

    for bi in range(num_samples):
        # Skip edge frames, pick the calmest middle frame
        for ti in range(15, num_frames - 15, 2):
            p = posed_np[bi, ti]
            vel = float(np.linalg.norm(posed_np[bi, ti + 1] - posed_np[bi, ti - 1]))
            yaw_err = abs(_compute_yaw_error_rad(p, bidx))
            knee_deg = _knee_flexion_deg(p, bidx)
            # Spine uprightness: Chest vs Hips XZ offset
            spine_lean = float(np.linalg.norm(p[bidx["Chest"], [0, 2]] - p[bidx["Hips"], [0, 2]]))

            if posture == "standing":
                min_foot_y = float(min(p[bidx["LeftFoot"], 1], p[bidx["RightFoot"], 1]))
                score = vel * 10.0 + yaw_err * 3.0 + spine_lean * 4.0 + abs(knee_deg - 10.0) * 0.03 + abs(min_foot_y - 0.05) * 4.0
            else:
                # Require bent knees (~80-100 deg) and feet clearly below hips
                hip_y = float(p[bidx["Hips"], 1])
                foot_y = float(0.5 * (p[bidx["LeftFoot"], 1] + p[bidx["RightFoot"], 1]))
                knee_pen = abs(knee_deg - 88.0) * 0.08 + (5.0 if knee_deg < 55.0 else 0.0)
                drop_pen = 5.0 if (hip_y - foot_y) < 0.25 else 0.0
                score = vel * 8.0 + yaw_err * 3.0 + spine_lean * 4.0 + knee_pen + drop_pen

            if score < best_score:
                best_score = score
                best_pose = local_rots[bi, ti]
                best_root = root_pos[bi, ti]

    assert best_pose is not None and best_root is not None
    aligned_pose, aligned_root = _recenter_and_align_pose(best_pose, best_root, out_skel, bidx)
    if posture == "standing":
        # Ground standing canonical pose cleanly
        _, p_chk, _ = out_skel.fk(aligned_pose.unsqueeze(0), aligned_root.unsqueeze(0))
        p_np = p_chk[0].detach().cpu().numpy()
        lowest = min(
            p_np[bidx["LeftToeBase"], 1],
            p_np[bidx["RightToeBase"], 1],
            p_np[bidx["LeftFoot"], 1] - 0.04,
            p_np[bidx["RightFoot"], 1] - 0.04,
        )
        aligned_root[1] -= float(lowest)
    elif posture == "seated":
        # Ensure seated pose hips height is comfortable (~0.75-0.95m) with feet above y=0.05
        _, p_chk, _ = out_skel.fk(aligned_pose.unsqueeze(0), aligned_root.unsqueeze(0))
        p_np = p_chk[0].detach().cpu().numpy()
        lowest_toe = float(min(p_np[bidx["LeftToeBase"], 1], p_np[bidx["RightToeBase"], 1], p_np[bidx["LeftFoot"], 1], p_np[bidx["RightFoot"], 1]))
        if lowest_toe < 0.08:
            aligned_root[1] += (0.12 - lowest_toe)
        print(f"[kimodo] Canonical seated knee_flex={_knee_flexion_deg(p_np, bidx):.1f} deg, hips_y={aligned_root[1]:.3f}m, lowest_foot={lowest_toe:.3f}m")

    torch.save({"local_rot_77": aligned_pose.detach().cpu(), "root_pos": aligned_root.detach().cpu()}, cache_file)
    print(f"[kimodo] Built canonical {posture} pose (score={best_score:.3f}, root_y={aligned_root[1]:.3f})")
    return aligned_pose, aligned_root


def _score_candidate(
    posed: np.ndarray,
    local_rots: np.ndarray,
    bidx: dict[str, int],
    entry: dict[str, Any],
) -> tuple[float, dict[str, float]]:
    """Score one generated sample (lower is better)."""
    T = posed.shape[0]
    hips_xz = posed[:, bidx["Hips"], [0, 2]]
    max_drift = float(np.max(np.linalg.norm(hips_xz - hips_xz[0:1], axis=-1)))

    # Heading deviation averaged over middle frames
    yaws = [abs(_compute_yaw_error_rad(posed[t], bidx)) for t in range(0, T, max(1, T // 10))]
    mean_yaw_deg = float(math.degrees(np.mean(yaws)))

    # Upper-body motion energy (Chest, Head, LeftArm, LeftForeArm, RightArm, RightForeArm)
    upper_indices = [bidx[n] for n in ["Chest", "Head", "LeftArm", "LeftForeArm", "RightArm", "RightForeArm"]]
    upper_pos = posed[:, upper_indices] - posed[:, [bidx["Hips"]]]
    upper_disp = float(np.mean(np.linalg.norm(upper_pos - upper_pos[0:1], axis=-1)))

    # Frame-to-frame smoothness (max joint jump in meters)
    frame_deltas = np.linalg.norm(np.diff(posed, axis=0), axis=-1)
    max_jump = float(np.max(frame_deltas))
    mean_jump = float(np.mean(frame_deltas))

    # Start vs end pose gap (important for loops and clean return-to-idle)
    end_gap = float(np.mean(np.linalg.norm((posed[-1] - posed[-1, bidx["Hips"]]) - (posed[0] - posed[0, bidx["Hips"]]), axis=-1)))

    # Lower-body / posture checks
    mean_knee = float(np.mean([_knee_flexion_deg(posed[t], bidx) for t in range(0, T, max(1, T // 8))]))
    hip_y = float(np.mean(posed[:, bidx["Hips"], 1]))
    foot_y = float(np.mean(0.5 * (posed[:, bidx["LeftFoot"], 1] + posed[:, bidx["RightFoot"], 1])))

    posture = entry["posture"]
    posture_pen = 0.0
    if posture == "seated":
        if mean_knee < 50.0:
            posture_pen += 20.0
        if (hip_y - foot_y) < 0.22:
            posture_pen += 10.0
    elif posture == "standing":
        if mean_knee > 45.0:
            posture_pen += 8.0

    # Penalize near-frozen motions unless idle
    target_energy = 0.025 if entry["preset"].startswith("idle") or entry["preset"] == "sit_idle" else 0.06
    energy_pen = max(0.0, target_energy - upper_disp) * 40.0

    # Penalize extreme spikes
    spike_pen = max(0.0, max_jump - 0.14) * 35.0
    drift_pen = max(0.0, max_drift - 0.18) * 12.0
    yaw_pen = max(0.0, mean_yaw_deg - 15.0) * 0.25
    end_pen = end_gap * (6.0 if entry["mode"] == "loop" else 3.0)

    total = posture_pen + energy_pen + spike_pen + drift_pen + yaw_pen + end_pen
    metrics = {
        "score": round(total, 3),
        "drift": round(max_drift, 3),
        "yaw_deg": round(mean_yaw_deg, 1),
        "upper_disp": round(upper_disp, 3),
        "max_jump": round(max_jump, 3),
        "end_gap": round(end_gap, 3),
        "knee_deg": round(mean_knee, 1),
    }
    return total, metrics


def _render_contact_sheet(
    posed: np.ndarray,
    parents: np.ndarray,
    bidx: dict[str, int],
    preset: str,
    prompt: str,
    metrics: dict[str, float],
    out_path: Path,
) -> None:
    """Render a 2-row (Front +Z view, 3/4 Side view) x 6-timestamp skeleton sheet."""
    T = posed.shape[0]
    frame_ids = [min(T - 1, int(round(r * (T - 1)))) for r in [0.0, 0.2, 0.4, 0.6, 0.8, 1.0]]
    # Focus on primary body + hands/feet (skip redundant fingertip subjoints for visual clarity)
    skip_suffix = ("End", "2", "3", "4")
    # Build bone edges from parents
    edges = [(i, int(parents[i])) for i in range(1, len(parents)) if parents[i] >= 0]

    fig = plt.figure(figsize=(15.0, 5.4), dpi=110)
    short_prompt = (prompt[:92] + "...") if len(prompt) > 95 else prompt
    fig.suptitle(
        f"{preset}  |  {short_prompt}\n"
        f"score={metrics['score']}  drift={metrics['drift']}m  yaw={metrics['yaw_deg']}deg  "
        f"disp={metrics['upper_disp']}m  knee={metrics['knee_deg']}deg  end_gap={metrics['end_gap']}m",
        fontsize=9.5,
        y=0.98,
    )

    center = np.mean(posed[:, bidx["Hips"]], axis=0)
    span = 0.95
    y_min = float(min(-0.05, np.min(posed[:, :, 1]) - 0.08))
    y_max = float(max(y_min + 1.85, np.max(posed[:, :, 1]) + 0.12))

    views = [
        ("Front (+Z)", 10, 90),   # looking from +Z toward origin: azim=90 in mpl? Let's project explicitly in 2D/3D!
        ("3/4 Side", 14, 35),
    ]

    for row, (view_name, _, _) in enumerate(views):
        for col, fi in enumerate(frame_ids):
            ax = fig.add_subplot(2, 6, row * 6 + col + 1)
            pts = posed[fi]
            if row == 0:
                # Front view facing viewer (+Z toward camera): screen X = -pts[:, 0] (char's right on viewer left), Y = pts[:, 1]
                xs = -pts[:, 0]
                ys = pts[:, 1]
                cx = -center[0]
            else:
                # 3/4 view rotated around Y by 42 deg so +Z forward goes toward right-front
                ang = math.radians(48.0)
                dx = pts[:, 0] - center[0]
                dz = pts[:, 2] - center[2]
                xs = dx * math.cos(ang) + dz * math.sin(ang)
                ys = pts[:, 1]
                cx = 0.0

            # Ground line y=0
            ax.axhline(0.0, color="#cbd5e1", linewidth=0.9, linestyle="--", zorder=1)
            # Facing +Z indicator on 3/4 row
            if row == 1:
                hz = 0.28 * math.sin(math.radians(48.0))
                ax.annotate("", xy=(hz, 0.02), xytext=(0.0, 0.02),
                            arrowprops=dict(arrowstyle="->", color="#ef4444", lw=1.2))

            for c_idx, p_idx in edges:
                color = "#2563eb"
                lw = 1.6
                if c_idx in (bidx["LeftArm"], bidx["LeftForeArm"], bidx["LeftHand"], bidx["LeftLeg"], bidx["LeftShin"], bidx["LeftFoot"]):
                    color = "#059669"  # Character's left = emerald
                elif c_idx in (bidx["RightArm"], bidx["RightForeArm"], bidx["RightHand"], bidx["RightLeg"], bidx["RightShin"], bidx["RightFoot"]):
                    color = "#e11d48"  # Character's right = rose
                elif c_idx in (bidx["Head"], bidx["Neck1"], bidx["Neck2"]):
                    color = "#7c3aed"
                ax.plot([xs[p_idx], xs[c_idx]], [ys[p_idx], ys[c_idx]], color=color, lw=lw, zorder=2)

            head = pts[bidx["Head"]]
            hx = -head[0] if row == 0 else ((head[0] - center[0]) * math.cos(math.radians(48.0)) + (head[2] - center[2]) * math.sin(math.radians(48.0)))
            ax.scatter([hx], [head[1] + 0.07], s=55, facecolors="none", edgecolors="#7c3aed", lw=1.4, zorder=3)

            ax.set_xlim(cx - span, cx + span)
            ax.set_ylim(y_min, y_max)
            ax.set_aspect("equal", adjustable="box")
            ax.set_xticks([])
            ax.set_yticks([])
            if row == 0:
                ax.set_title(f"f{fi} ({fi / 30.0:.1f}s)", fontsize=8)
            if col == 0:
                ax.set_ylabel(view_name, fontsize=8)

    plt.tight_layout(rect=(0, 0, 1, 0.90))
    fig.savefig(out_path, dpi=110)
    plt.close(fig)


def _build_overview_grids(sheet_paths: list[Path], out_dir: Path, per_grid: int = 6) -> list[Path]:
    from PIL import Image

    grids = []
    for gi in range(0, len(sheet_paths), per_grid):
        chunk = sheet_paths[gi : gi + per_grid]
        imgs = [Image.open(p).convert("RGB") for p in chunk]
        w = max(im.width for im in imgs)
        h_total = sum(im.height for im in imgs)
        canvas = Image.new("RGB", (w, h_total), (255, 255, 255))
        y = 0
        for im in imgs:
            canvas.paste(im, (0, y))
            y += im.height
        grid_path = out_dir / f"grid_{gi // per_grid + 1}.png"
        canvas.save(grid_path)
        grids.append(grid_path)
    return grids


def run_generation(catalog_path: Path, out_dir: Path, only: set[str] | None = None) -> None:
    catalog = json.loads(catalog_path.read_text(encoding="utf-8"))
    model_name = catalog["model"]
    fps = int(catalog.get("fps", 30))
    entries = catalog["motions"]
    if only:
        entries = [e for e in entries if e["preset"] in only]
        if not entries:
            raise ValueError(f"No catalog entries matched --only {sorted(only)}")

    out_dir.mkdir(parents=True, exist_ok=True)
    npz_dir = out_dir / "npz"
    sheet_dir = out_dir / "sheets"
    npz_dir.mkdir(exist_ok=True)
    sheet_dir.mkdir(exist_ok=True)

    model = get_model(model_name)
    out_skel = model.output_skeleton
    bidx = _bone_index(out_skel)
    neutral_joints = out_skel.neutral_joints.detach().cpu().numpy().astype(np.float32)
    joint_parents = out_skel.joint_parents.detach().cpu().numpy().astype(np.int32)
    bone_names = np.array(list(out_skel.bone_order_names))

    needed_postures = {"standing"} | {
        e["posture"] for e in entries if e.get("anchor_ends", True) and e["posture"] in ("standing", "seated")
    }
    canonical_poses = {p: _build_canonical_pose(model, p, out_dir) for p in sorted(needed_postures)}

    # Derive exact +Z global_root_heading vector via Kimodo's own compute_global_heading
    from kimodo.constraints import compute_global_heading

    st_rot, st_root = canonical_poses["standing"]
    st_skel = model.skeleton
    st_local = st_skel.from_SOMASkeleton77(st_rot.unsqueeze(0)) if st_skel.name == "somaskel30" else st_rot.unsqueeze(0)
    _, st_posed, _ = st_skel.fk(st_local, st_root.unsqueeze(0))
    forward_heading = compute_global_heading(st_posed, st_skel)[0].detach()
    print(f"[kimodo] Forward (+Z) heading vector: {forward_heading.cpu().numpy().round(3)}")

    summary = []
    from kimodo.tools import seed_everything

    for entry in entries:
        preset = entry["preset"]
        num_frames = int(round(float(entry["duration_sec"]) * fps))
        num_samples = int(entry.get("num_samples", 4))
        seed = int(entry.get("seed", 100))
        posture = entry["posture"]
        post_proc = (posture != "dangling") and (not entry.get("no_postprocess", False))

        constraints = []
        if entry.get("anchor_ends", True) and posture in canonical_poses:
            pose_rot, pose_root = canonical_poses[posture]
            if entry.get("elevate_root_y", 0.0):
                pose_root = pose_root.clone()
                pose_root[1] = pose_root[1] + float(entry["elevate_root_y"])
            constraints.append(_make_fullbody_anchor(model, pose_rot, pose_root, [0, num_frames - 1]))
        if entry.get("pin_mid_root", False):
            constraints.append(_make_root2d_anchor(model, [0, num_frames // 2, num_frames - 1], heading_vec=forward_heading))
        elif entry.get("pin_heading", False):
            constraints.append(_make_root2d_anchor(model, [0, num_frames // 3, (2 * num_frames) // 3, num_frames - 1], heading_vec=forward_heading))

        seed_everything(seed)
        print(f"[kimodo] Generating {preset} ({num_frames}f, {num_samples}x, posture={posture}, constraints={len(constraints)}) ...")
        output = model(
            entry["prompt"],
            num_frames,
            num_denoising_steps=100,
            constraint_lst=constraints,
            cfg_weight=[2.2, 2.0],
            num_samples=num_samples,
            first_heading_angle=torch.zeros(num_samples, device=model.device),
            post_processing=post_proc,
            return_numpy=True,
        )

        best_idx = 0
        best_score = float("inf")
        best_metrics: dict[str, float] = {}
        for bi in range(num_samples):
            sc, metrics = _score_candidate(output["posed_joints"][bi], output["local_rot_mats"][bi], bidx, entry)
            if sc < best_score:
                best_score = sc
                best_idx = bi
                best_metrics = metrics

        print(f"  -> {preset}: picked sample {best_idx}/{num_samples} {best_metrics}")
        npz_path = npz_dir / f"{preset}.npz"
        np.savez_compressed(
            npz_path,
            local_rot_mats=output["local_rot_mats"][best_idx].astype(np.float32),
            global_rot_mats=output["global_rot_mats"][best_idx].astype(np.float32),
            posed_joints=output["posed_joints"][best_idx].astype(np.float32),
            root_positions=output["root_positions"][best_idx].astype(np.float32),
            foot_contacts=output["foot_contacts"][best_idx].astype(np.float32),
            neutral_joints=neutral_joints,
            joint_parents=joint_parents,
            bone_order_names=bone_names,
            fps=np.int32(fps),
        )
        sheet_path = sheet_dir / f"{preset}_sheet.png"
        _render_contact_sheet(
            output["posed_joints"][best_idx],
            joint_parents,
            bidx,
            preset,
            entry["prompt"],
            best_metrics,
            sheet_path,
        )
        summary.append({"preset": preset, "file": entry["file"], "sample": best_idx, **best_metrics})

    all_sheets = sorted(sheet_dir.glob("*_sheet.png"))
    grids = _build_overview_grids(all_sheets, out_dir, per_grid=6)
    (out_dir / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")

    tar_path = out_dir.parent / "kimodo_out.tar.gz"
    with tarfile.open(tar_path, "w:gz") as tf:
        for p in sorted(npz_dir.glob("*.npz")):
            tf.add(p, arcname=f"npz/{p.name}")
        for p in all_sheets:
            tf.add(p, arcname=f"sheets/{p.name}")
        for p in grids:
            tf.add(p, arcname=p.name)
        tf.add(out_dir / "summary.json", arcname="summary.json")
    print(f"[kimodo] Bundled {len(all_sheets)} sheets + {len(list(npz_dir.glob('*.npz')))} npz -> {tar_path} ({tar_path.stat().st_size / (1024*1024):.2f} MiB)")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--catalog", type=Path, default=Path("/content/vrma-catalog.json"))
    parser.add_argument("--out", type=Path, default=Path("/content/kimodo_out"))
    parser.add_argument("--only", type=str, default="")
    args = parser.parse_args()
    only = {s.strip() for s in args.only.split(",") if s.strip()} if args.only else None
    run_generation(args.catalog, args.out, only=only)


if __name__ == "__main__":
    main()
