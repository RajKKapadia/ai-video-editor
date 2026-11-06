#!/usr/bin/env python3
"""Exercise the packaged worker with a real model, without starting the editor."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser()
parser.add_argument("--worker", type=Path, required=True)
parser.add_argument("--model", type=Path)
parser.add_argument("--device", type=int)
args = parser.parse_args()
with tempfile.TemporaryDirectory(prefix="cutline-speech-check-") as directory:
    request = Path(directory) / "request.json"
    response = Path(directory) / "response.json"
    def run(payload):
        request.write_text(json.dumps(payload))
        response.write_text("")
        subprocess.run([str(args.worker.resolve()), "--cutline-speech-worker", str(request), str(response)], check=True, timeout=180)
        data = json.loads(response.read_text())
        if "Err" in data:
            raise RuntimeError(data["Err"])
        return data["Ok"]
    probe = run({"operation": "devices"})
    print(json.dumps(probe, indent=2), flush=True)
    if args.model:
        result = run({"operation": "benchmark", "model": str(args.model.resolve()), "device": args.device, "threads": 8})
        assert result["loadSeconds"] > 0
        assert result["realtimeFactor"] > 0
        print(json.dumps(result, indent=2), flush=True)
