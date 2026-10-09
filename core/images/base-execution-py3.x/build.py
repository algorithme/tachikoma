#!/usr/bin/env python3
"""Build the execution image on the Docker daemon's native architecture."""

import argparse
from pathlib import Path
import subprocess
import sys


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", nargs="?", default="base-execution-py3.x:dev")
    args = parser.parse_args()
    context = str(Path(__file__).resolve().parent)
    try:
        subprocess.run(["docker", "build", "--check", context], check=True, timeout=120)
        subprocess.run(
            # Publish one manifest per native image. Automatic attestations can
            # otherwise wrap it in an index on containerd-backed Docker stores.
            ["docker", "build", "--provenance=false", "--tag", args.image, context],
            check=True,
            timeout=600,
        )
    except (OSError, subprocess.SubprocessError) as error:
        print(f"Execution image build failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
