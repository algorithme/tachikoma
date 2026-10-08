# Base Python execution image

`ghcr.io/algorithme/base-execution-py3.x` is the project's Python-only runtime.
It derives from the Docker Official Python 3.13 slim Debian image, pinned by its
multi-platform digest in the Dockerfile. Python and bundled pip are reused;
there is no Python compilation, package installation, or application source in
this build. NumPy, pandas and execution workers belong to a later phase.

The image defaults to UID/GID `10001:10001` and `/workspace`, also its `HOME`.
The build context excludes everything except the Dockerfile and `.dockerignore`.
The public image contains no repository content, approved-image reference,
credentials or personal files. Its source label links it to the private repository
for GitHub package permissions.

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

Renovate maintains the upstream Dockerfile tag and digest. Patch/digest changes
can automerge after checks; Python minor changes require review, and major
changes require dashboard approval. Publishing a new candidate does not change
an existing core binary's approved image.

Image availability and these smoke checks do not establish the ADR's future
untrusted-code containment gates. No agent workloads run in this phase.
