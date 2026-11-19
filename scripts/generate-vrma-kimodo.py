#!/usr/bin/env python3
"""Local orchestrator to provision Google Colab GPU, run Kimodo generation, and convert outputs to VRMA."""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tarfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
DEFAULT_SESSION = "kimodo-gen"


def read_hf_token(env_path: Path) -> str:
    if env_token := os.environ.get("HF_TOKEN"):
        return env_token.strip()
    if not env_path.is_file():
        raise RuntimeError(f"Missing {env_path} and HF_TOKEN env var")
    for line in env_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        if k.strip() == "HF_TOKEN":
            val = v.strip().strip('"').strip("'")
            if val:
                return val
    raise RuntimeError(f"HF_TOKEN not found in {env_path}")


def run_cmd(cmd: list[str], stdin_text: str | None = None, check: bool = True) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(cmd, input=stdin_text, text=True, check=False)
    if check and proc.returncode != 0:
        raise RuntimeError(f"Command failed (exit {proc.returncode}): {' '.join(cmd)}")
    return proc


def provision_session(session: str, gpus: list[str]) -> str:
    chk = subprocess.run(["colab", "status", "-s", session], capture_output=True, text=True)
    if chk.returncode == 0 and "IDLE" in chk.stdout:
        print(f"[orchestrator] Reusing active Colab session '{session}'.")
        return "existing"
    for gpu in gpus:
        print(f"[orchestrator] Provisioning Colab {gpu} session '{session}' ...")
        proc = subprocess.run(["colab", "new", "-s", session, "--gpu", gpu], text=True)
        if proc.returncode == 0:
            return gpu
    raise RuntimeError(f"Failed to provision any Colab GPU from {gpus}")


def setup_remote(session: str, hf_token: str) -> None:
    setup_script = """
import os, subprocess, sys
print("Python:", sys.version)
subprocess.run(["nvidia-smi"], check=False)

import torch
print("PyTorch:", torch.__version__, "CUDA available:", torch.cuda.is_available(), "Device:", torch.cuda.get_device_name(0) if torch.cuda.is_available() else "none")

# Colab preinstalls an older torchao that conflicts with newer peft; uninstall it cleanly
subprocess.run([sys.executable, "-m", "pip", "uninstall", "-y", "-q", "torchao"], check=False)
# Clone kimodo and install with --no-build-isolation so setuptools doesn't fail inside isolated env
if not os.path.exists("/content/kimodo"):
    subprocess.run(["git", "clone", "--depth", "1", "https://github.com/nv-tlabs/kimodo.git", "/content/kimodo"], check=True)
r = subprocess.run([sys.executable, "-m", "pip", "install", "-q", "--no-build-isolation", "-e", "/content/kimodo", "matplotlib"], capture_output=True, text=True)
if r.returncode != 0:
    print("STDOUT:", r.stdout[-3000:])
    print("STDERR:", r.stderr[-3000:])
    raise RuntimeError(f"pip install failed with exit {r.returncode}")

token = os.environ.pop("HF_TOKEN", "")
if not token:
    raise RuntimeError("HF_TOKEN missing in remote env")
from huggingface_hub import login, HfApi
login(token=token, add_to_git_credential=False)
me = HfApi().whoami()
print("HF authenticated:", me.get("name") or me.get("fullname") or "ok")
# Verify gated Meta-Llama-3-8B-Instruct access immediately before spending GPU time
HfApi().model_info("meta-llama/Meta-Llama-3-8B-Instruct", token=token)
print("Verified access to meta-llama/Meta-Llama-3-8B-Instruct")
"""
    proc = subprocess.run(
        ["colab", "exec", "-s", session, "--timeout", "0", "--env", f"HF_TOKEN={hf_token}"],
        input=setup_script,
        text=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"Remote Colab setup failed (exit {proc.returncode})")


def upload_and_generate(session: str, catalog: Path, only: str | None) -> None:
    run_cmd(["colab", "upload", "-s", session, str(catalog), "/content/vrma-catalog.json"])
    run_cmd(["colab", "upload", "-s", session, str(ROOT / "scripts" / "colab_kimodo_gen.py"), "/content/colab_kimodo_gen.py"])
    only_arg = only or ""
    exec_code = f"""
import sys
sys.argv = ["colab_kimodo_gen.py", "--catalog", "/content/vrma-catalog.json", "--out", "/content/kimodo_out"]
if {only_arg!r}:
    sys.argv.extend(["--only", {only_arg!r}])
import colab_kimodo_gen
# Reload in case colab_kimodo_gen was updated in the same persistent kernel
import importlib
importlib.reload(colab_kimodo_gen)
colab_kimodo_gen.main()
"""
    run_cmd(["colab", "exec", "-s", session, "--timeout", "0"], stdin_text=exec_code)


def download_and_extract(session: str, scratch_dir: Path) -> None:
    scratch_dir.mkdir(parents=True, exist_ok=True)
    tar_local = scratch_dir / "kimodo_out.tar.gz"
    run_cmd(["colab", "download", "-s", session, "/content/kimodo_out.tar.gz", str(tar_local)])
    with tarfile.open(tar_local, "r:gz") as tf:
        tf.extractall(scratch_dir, filter="data")
    print(f"[orchestrator] Extracted outputs to {scratch_dir}")


def convert_all(catalog: Path, scratch_dir: Path, out_dir: Path) -> None:
    run_cmd([
        "uv", "run", "--no-project", "--with", "numpy", "--with", "scipy",
        "python", str(ROOT / "scripts" / "kimodo_to_vrma.py"),
        "--catalog", str(catalog),
        "--npz-dir", str(scratch_dir / "npz"),
        "--out-dir", str(out_dir),
    ])


def main() -> None:
    parser = argparse.ArgumentParser(description="Drive Kimodo on Google Colab and convert to VRMA")
    parser.add_argument("action", choices=["setup", "generate", "download", "convert", "stop", "all"])
    parser.add_argument("--session", default=DEFAULT_SESSION)
    parser.add_argument("--gpu", default="A100,L4", help="Comma-separated GPU fallback list")
    parser.add_argument("--catalog", type=Path, default=ROOT / "scripts" / "vrma-catalog.json")
    parser.add_argument("--scratch", type=Path, required=False, default=ROOT / "target" / "kimodo_scratch")
    parser.add_argument("--out-dir", type=Path, default=ROOT / "assets" / "vrma")
    parser.add_argument("--only", default="", help="Comma-separated preset names to regenerate")
    args = parser.parse_args()

    if args.action == "stop":
        run_cmd(["colab", "stop", "-s", args.session], check=False)
        run_cmd(["colab", "sessions"], check=False)
        return

    if args.action == "convert":
        convert_all(args.catalog, args.scratch, args.out_dir)
        return

    if args.action == "download":
        download_and_extract(args.session, args.scratch)
        return

    if args.action == "setup":
        hf_token = read_hf_token(ROOT / ".env")
        gpus = [g.strip() for g in args.gpu.split(",") if g.strip()]
        provision_session(args.session, gpus)
        setup_remote(args.session, hf_token)
        return

    if args.action == "generate":
        upload_and_generate(args.session, args.catalog, args.only)
        download_and_extract(args.session, args.scratch)
        return

    if args.action == "all":
        hf_token = read_hf_token(ROOT / ".env")
        gpus = [g.strip() for g in args.gpu.split(",") if g.strip()]
        provision_session(args.session, gpus)
        try:
            setup_remote(args.session, hf_token)
            upload_and_generate(args.session, args.catalog, args.only)
            download_and_extract(args.session, args.scratch)
            convert_all(args.catalog, args.scratch, args.out_dir)
        finally:
            run_cmd(["colab", "stop", "-s", args.session], check=False)


if __name__ == "__main__":
    main()
