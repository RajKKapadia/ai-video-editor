#!/usr/bin/env python3
"""Build an optional GPU worker and a CPU-capable editor with it bundled as a resource."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--backend", choices=("cpu", "cuda", "vulkan"), required=True)
bundling = parser.add_mutually_exclusive_group()
bundling.add_argument("--no-bundle", action="store_true")
bundling.add_argument("--bundles", help="Comma-separated Tauri bundle types")
parser.add_argument("--target", help="Tauri target triple or universal-apple-darwin for the portable editor")
parser.add_argument("--locked", action="store_true", help="Require an up-to-date Cargo.lock")
args = parser.parse_args()
if args.target and args.backend != "cpu":
    parser.error("--target is available only with --backend cpu")
cargo_lock_args = ["--locked"] if args.locked else []
extension = ".exe" if os.name == "nt" else ""
manifest = ROOT / "src-tauri" / "Cargo.toml"
target = ROOT / "src-tauri" / "target" / "speech-accelerator"
portable = ROOT / "src-tauri" / "target" / ("speech-portable-cpu" if args.backend == "cpu" else "speech-portable")
environment = os.environ.copy()
# A distributed binary must not inherit the build machine's instruction set.
for flag in ("NATIVE", "SSE42", "AVX", "AVX2", "AVX_VNNI", "AVX512", "AVX512_VBMI", "AVX512_VNNI", "AVX512_BF16", "FMA", "F16C", "BMI2"):
    environment["GGML_" + flag] = "OFF"
resource = ROOT / "src-tauri" / "speech-worker"
if args.backend != "cpu":
    print("Building optional", args.backend, "speech worker…", flush=True)
    subprocess.run([
        "cargo", "build", "--release", "--manifest-path", str(manifest),
        "--target-dir", str(target), "--features", "speech-" + args.backend,
        *cargo_lock_args,
    ], check=True, cwd=ROOT, env=environment)
    resource.mkdir(exist_ok=True)
    shutil.copy2(target / "release" / ("tauri-app" + extension), resource / ("cutline-speech-accelerated" + extension))
# Keep CUDA/Vulkan dependencies out of the editor: an unavailable sidecar can fail safely.
config = {"bundle": {"resources": [] if args.backend == "cpu" else ["speech-worker/*"]}}
command = ["bun", "run", "tauri", "build", "--config", json.dumps(config)]
if args.no_bundle:
    command.append("--no-bundle")
if args.bundles:
    command.extend(["--bundles", args.bundles])
if args.target:
    command.extend(["--target", args.target])
if cargo_lock_args:
    command.extend(["--", *cargo_lock_args])
environment["CARGO_TARGET_DIR"] = str(portable)
print("Building portable editor…", flush=True)
subprocess.run(command, check=True, cwd=ROOT, env=environment)
if args.backend != "cpu":
    adjacent = portable / "release" / "speech-worker"
    adjacent.mkdir(exist_ok=True)
    shutil.copy2(resource / ("cutline-speech-accelerated" + extension), adjacent / ("cutline-speech-accelerated" + extension))
output = portable / args.target if args.target else portable
print("Editor and installers:", output / "release", flush=True)
