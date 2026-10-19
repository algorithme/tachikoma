# Base Python execution image

`ghcr.io/algorithme/base-execution-py3.x` is the project's Python-only runtime.
It derives from the Docker Official Python 3.13 slim Debian image, pinned by its
multi-platform digest in the Dockerfile. Python and bundled pip are reused;
there is no Python compilation, package installation, or application source in
this build. NumPy and pandas belong to a later phase.

The image defaults to UID/GID `10001:10001` and `/workspace`, also its `HOME`.
The build context excludes everything except the Dockerfile and `.dockerignore`.
The public image contains no repository content, approved-image reference,
credentials or personal files. Its source label links it to the private repository
for GitHub package permissions.

## Core-supplied workers

Core embeds the trusted launcher in `src/execution/worker.py`. It passes this
fixed code to isolated, unbuffered Python. Generated source arrives through
standard input after the launcher verifies its restrictions. The image does not
contain the launcher or generated code.

The private runtime uses one container per session and a fresh worker per call.
The keeper uses UID/GID 10000 in `/`. Workers use UID/GID 10001 in `/workspace`.
Healthy calls retain files in bounded temporary mounts. Python variables and
imports do not survive a call. No agent tool uses this runtime yet.

The runtime checks cgroup v2, seccomp, mount limits, the actual Python version and
seven standard-library modules before it accepts source. Its fixed limits and
live test commands are in the [core runtime guide](../../README.md#private-python-runtime).
The 2026-10-17 Docker Desktop ARM64 run verified Python 3.13.16 through a Unix
socket. Native AMD64 and ARM64 runtime CI jobs are configured; their results for
this change remain pending. Image smoke tests and runtime tests are separate gates.

## Build and check locally

Image maintenance requires Python 3.10+, a running Linux-container Docker daemon,
and a Docker CLI with Buildx and `docker build --check` support. These tools are
not prerequisites for the Rust application's Docker API client.

From the repository root:

```sh
python3 core/images/base-execution-py3.x/build.py
python3 core/images/base-execution-py3.x/smoke.py
```

Both commands accept an optional image tag, defaulting to
`base-execution-py3.x:dev`. The smoke command additionally accepts
`--platform linux/amd64` or `--platform linux/arm64` to require that native daemon
architecture. The maintenance commands use the Docker CLI's selected context;
application boot uses its independently validated local endpoint.

The build has a ten-minute deadline. The smoke test checks image metadata and
runs a thirty-second Python check with no network or host mounts, a read-only
root, a small private workspace, and resource limits. It checks the Dockerfile's
Python version, native architecture, non-root identity, environment and file I/O.
Cleanup targets only its uniquely named test container, including after timeout.

## Publishing and approving an image

```text
Dockerfile change -> native AMD64 and ARM64 build + smoke
                           |
                  main: push tested images
                           |
                  combine immutable digests
                           |
                  candidate in job summary
                           |
                  review and approve digest
```

The `Execution image` workflow checks relevant pull requests without registry
credentials. On relevant `main` changes or manual dispatch on `main`, it publishes
each tested platform image and a combined `sha-<commit>` image. Only publishing
jobs have `packages: write`. The workflow neither uses a moving `latest` tag nor
changes the approved runtime reference.
Automatic build attestations are disabled so each tested native image has one
manifest and the publishing job can verify both exact child digests consistently
across Docker image-store implementations.

First publication creates a private GHCR package. Change only this package to
Public in its GitHub package settings, retaining the repository's private
visibility. Verify the exact combined digest is anonymously pullable before
enabling startup against it. A package made public cannot be changed back to
private through GitHub's visibility settings.

For every promotion, review the successful workflow run and its combined digest.
Verify the manifest contains the tested Linux AMD64 and ARM64 child digests, then
write the complete `ghcr.io/algorithme/base-execution-py3.x@sha256:...` reference
from the job summary to `approved-image.ref` in this directory. Commit that real
reference in a reviewed change. Never use a tag or placeholder digest. Core embeds
the reference at compile time, so promotion requires rebuilding core.

Use an empty temporary Docker credential configuration to test anonymous access
without logging out of the user's existing registry sessions. Pull by the exact
approved digest, not by its commit tag.

Renovate maintains the upstream Dockerfile tag and digest within Python 3.13.
Patch/digest changes can automerge after checks. Newer Python minor or major
versions are blocked until the project explicitly revisits this policy and
updates the Python rule in `renovate.json5`'s user-managed region and the matching
inventory notes. Publishing a new candidate does not change an existing core
binary's approved image.

Image availability and these smoke checks do not establish the ADR's future
untrusted-code containment gates. No agent workloads run in this phase.
