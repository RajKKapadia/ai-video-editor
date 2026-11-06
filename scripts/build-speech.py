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
parser.add_argument("--no-bundle", action="store_true")
args = parser.parse_args()
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
    ], check=True, cwd=ROOT, env=environment)
    resource.mkdir(exist_ok=True)
    shutil.copy2(target / "release" / ("tauri-app" + extension), resource / ("cutline-speech-accelerated" + extension))
# Keep CUDA/Vulkan dependencies out of the editor: an unavailable sidecar can fail safely.
config = {"bundle": {"resources": [] if args.backend == "cpu" else ["speech-worker/*"]}}
command = ["bun", "run", "tauri", "build", "--config", json.dumps(config)]
if args.no_bundle:
    command.append("--no-bundle")
environment["CARGO_TARGET_DIR"] = str(portable)
print("Building portable editor…", flush=True)
subprocess.run(command, check=True, cwd=ROOT, env=environment)
if args.backend != "cpu":
    adjacent = portable / "release" / "speech-worker"
    adjacent.mkdir(exist_ok=True)
    shutil.copy2(resource / ("cutline-speech-accelerated" + extension), adjacent / ("cutline-speech-accelerated" + extension))
print("Editor and installers:", portable / "release", flush=True)
