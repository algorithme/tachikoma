#!/usr/bin/env python3
"""Check the trusted image without host mounts or container networking."""

import argparse
import json
from pathlib import Path
import re
import signal
import subprocess
import sys
import uuid


SMOKE_CODE = """
import os, pathlib, platform, sys
assert platform.python_version() == sys.argv[1], platform.python_version()
assert platform.machine() == sys.argv[2], platform.machine()
assert os.getuid() == os.getgid() == 10001
assert os.getcwd() == os.environ['HOME'] == '/workspace'
assert os.environ['PYTHONDONTWRITEBYTECODE'] == '1'
assert os.environ['PYTHONUNBUFFERED'] == '1'
workspace = pathlib.Path('/workspace/smoke.txt')
workspace.write_text('execution image ready')
assert workspace.read_text() == 'execution image ready'
workspace.unlink()
print('Execution image smoke test passed:', platform.python_version(), platform.machine())
"""


def docker_json(*arguments: str) -> dict:
    result = subprocess.run(
        ["docker", *arguments, "--format", "{{json .}}"],
        check=True,
        capture_output=True,
        text=True,
        timeout=15,
    )
    return json.loads(result.stdout)


def smoke(image: str, expected_platform: str | None) -> None:
    dockerfile = (Path(__file__).resolve().parent / "Dockerfile").read_text()
    version = re.search(r"^FROM python:(\d+\.\d+\.\d+)-", dockerfile, re.MULTILINE)
    if version is None:
        raise ValueError("Dockerfile must pin a full Python version")

    daemon = docker_json("info")
    architecture = {
        "x86_64": "amd64", "amd64": "amd64", "aarch64": "arm64", "arm64": "arm64"
    }.get(daemon["Architecture"])
    if daemon["OSType"] != "linux" or architecture is None:
        raise ValueError("Smoke tests require a native Linux AMD64 or ARM64 Docker daemon")
    if expected_platform is not None and expected_platform != f"linux/{architecture}":
        raise ValueError("Docker daemon does not match the requested native platform")

    details = docker_json("image", "inspect", image)
    if details["Os"] != "linux" or details["Architecture"] != architecture:
        raise ValueError("Image architecture must match the Docker daemon; emulation is not used")
    if details["Config"]["User"] != "10001:10001":
        raise ValueError("Image must default to user 10001:10001")
    if details["Config"]["WorkingDir"] != "/workspace":
        raise ValueError("Image must default to /workspace")

    name = f"execution-image-smoke-{uuid.uuid4().hex}"
    machine = {"amd64": "x86_64", "arm64": "aarch64"}[architecture]
    try:
        subprocess.run(
            [
                "docker", "run", "--rm", "--name", name, "--pull", "never",
                "--network", "none", "--read-only", "--cap-drop", "ALL",
                "--security-opt", "no-new-privileges", "--pids-limit", "32",
                "--memory", "128m", "--cpus", "1", "--log-driver", "none",
                "--tmpfs", "/workspace:rw,nosuid,nodev,noexec,size=16m,uid=10001,gid=10001,mode=700",
                image, "python3", "-I", "-B", "-c", SMOKE_CODE, version[1], machine,
            ],
            check=True,
            timeout=30,
        )
    finally:
        # A timed-out Docker client does not imply that its container stopped.
        cleanup = subprocess.run(
            ["docker", "rm", "--force", name],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if cleanup.returncode != 0 and "No such container" not in cleanup.stderr:
            raise RuntimeError(f"Could not remove smoke container {name}: {cleanup.stderr.strip()}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", nargs="?", default="base-execution-py3.x:dev")
    parser.add_argument("--platform", choices=["linux/amd64", "linux/arm64"])
    args = parser.parse_args()
    # Convert cancellation into normal stack unwinding so the container cleanup runs.
    def cancelled(signum: int, _frame: object) -> None:
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, cancelled)
    try:
        smoke(args.image, args.platform)
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"Execution image smoke test failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
