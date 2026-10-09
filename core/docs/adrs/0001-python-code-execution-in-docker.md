# ADR 0001: Docker-backed Python execution with lazy input resources

- **Status:** Accepted architectural direction. Basic image and startup prerequisites are implemented; execution remains subject to the validation gates below.
- **Date:** 2026-10-06; decision revised 2026-10-09.
- **Decision owner:** Project architect.
- **Research observed:** 2026-10-05 to 2026-10-09.
- **Scope:** Core product code and image assets live in `core/`; image publishing and dependency-maintenance files are the necessary repository-level exceptions.

## First implementation: image and startup prerequisites

The first implementation deliberately contains Python only. The scientific
NumPy/pandas profile described below remains the next approved runtime direction,
not a dependency of this initial image. The Rust broker continues to run on the
host; no generated code or agent container runs in this phase.

`base-execution-py3.x` derives from the digest-pinned Docker Official
`python:3.13.16-slim-trixie` image. It reuses upstream Python and bundled pip,
installs no packages, defaults to UID/GID `10001:10001`, and provides `/workspace`.
The isolated build context contains no application source or personal data.
Native AMD64 and ARM64 CI builds are smoke-tested before publication to public
`ghcr.io/algorithme/base-execution-py3.x`. Published candidates are promoted by a
reviewed change to the compiled-in `approved-image.ref`; publication alone does
not update running or rebuilt applications.

Before binding HTTP, `serve()` verifies local Docker and the approved image.
Private models are `DockerEndpoint`, `ExecutionImage`, `LinuxArchitecture` and
`DockerPreflightError`. `DockerEndpoint::from_lookup()` accepts OS-specific local
defaults or `TACHIKOMA_DOCKER_HOST`, rejecting remote transports and ignoring CLI
contexts. Bollard `=0.21.1` uses only its `pipe` feature. The adapter explicitly
prefixes post-negotiation requests with the negotiated API version to work around
that release's URI-construction defect; a real-transport fixture checks this.

```text
load config -> connect local Docker -> negotiate API -> inspect daemon
                                                           |
                                              inspect approved image
                                                    /          \
                                               cached          absent
                                                  |              |
                                                  |       pull exact digest
                                                  |              |
                                                  +---- verify platform/digest
                                                                 |
                                                            start HTTP
```

`ensure_ready()`, `connect_local_docker()`, `ensure_image_available()` and
`validate_image()` implement this boundary. Daemon/request checks are limited to
five seconds and pulling to ten minutes. Only image absence permits a pull;
errors, mismatches or unsupported platforms stop startup. No image builds,
daemon startup, container creation or generated execution occur during boot.

Fixture tests cover endpoint restrictions, negotiation, deadlines, cached and
missing images, pull failures and digest/platform validation. Native image smoke
checks cover Python version, architecture, identity and workspace behavior.
These establish prerequisites only; they do not satisfy the full execution,
containment, lifecycle or data-transfer gates below. See the
[core README](../../README.md) and
[image guide](../../images/base-execution-py3.x/README.md) for operational details.

## Context and decision

Core is a Rust personal-agent library. Multiple agents should run efficiently on
one device while protecting user data. Generated code is untrusted, including
code produced in response to malicious instructions found in input documents.
Security takes priority over performance; the implementation should remain small
and explicit.

The initial investigation considered JavaScript in Wasmtime, followed by an
equivalent Python investigation. The requirement for the general Python package
ecosystem led to native CPython in Linux containers. The first approved scientific
profile contains NumPy and pandas; broader package availability is a capability
of future approved images, not permission to install arbitrary packages at runtime.

Use **Docker Engine with Bollard** behind a small core-owned Rust adapter, with
one minimal Linux container per agent. The initial compatibility target is local
Docker Engine on Linux and Docker Desktop in Linux-container mode on macOS and
Windows, subject to the capability and containment gates below. Use native
`linux/amd64` and `linux/arm64` images as appropriate for the daemon's architecture.

The architect's final 2026-10-09 decision restores Docker/Bollard for a simpler
initial integration and broader host compatibility. It supersedes the interim
Apple-container choice. Apple containers and other VM-backed runtimes remain
alternatives; they are not additional backends to implement now. The previously
researched Bollard release is **0.21.1**; pin the tested dependency and negotiate
Engine API compatibility explicitly. The first implementation above adds only
image preparation and startup prerequisites.

Prepare single-use Python processes with approved libraries imported in advance.
A worker executes one request and exits. Keep editable files for the container
session, but never reuse a Python interpreter after generated
code has run in it. Expose `code_execution` and separate typed `ls`, `find`, `grep`
and `cat` tools.

Input files and folders are declared in trusted configuration when the agent is
created. Copy the entire configured set into the container on the first
file-capable tool call, using contents available at that time. Copies are editable,
never synchronize back to the host, and disappear when the container is replaced
or closed.

This selects container isolation with a private filesystem per agent, rather than
a separate guest kernel per agent. Linux Engine containers share the host kernel;
Docker Desktop runs them inside shared Linux VM infrastructure. The host OS,
Docker stack, core broker, approved images and dependency-build process remain
trusted. Container isolation does not establish our additional process restrictions
or resource limits by itself. Protecting data from the device owner or
administrator is outside this decision.
[Docker security](https://docs.docker.com/engine/security/).

## Models and ownership

Here, a **model** is a proposed application data type, not an LLM. These names
describe responsibilities; they do not require one abstraction or file per row.

| Model | Responsibility and semantics |
| --- | --- |
| `AgentRuntimeConfig` | Host-provided runtime profile, execution policy and immutable list of authorized input resources for one agent. |
| `InputResource` | Host source file/folder and optional container name. The name defaults to the source basename and denotes one child of `/workspace/inputs`. Reject empty, absolute, traversal or duplicate destination names. Host paths are never accepted from tool arguments. |
| `RuntimeProfile` | Approved OCI image digest, native Linux CPU architecture, Python/package versions, preload list and trusted helper identities. The initial preload list is NumPy and pandas. |
| `ExecutionPolicy` | Explicit positive budgets for preparation, execution, memory, CPU, threads/PIDs, writable storage, input transfer, output and global admission. These are host authority, not LLM-selectable permissions. |
| `AgentRuntime` | Docker container identity, session generation, per-agent operation lock, worker readiness, resource state and owned cancellation/cleanup lifecycle. Never share this mutable state across agents. |
| `ResourceState` | `NotCopied`, `Copying` or `Ready`, held by core and scoped to a particular container session. Guest files are not proof of readiness. |
| `PreparedWorker` | Fresh Python process with approved imports and execution restrictions established. Accepts exactly one code request; cannot return to a reusable pool. |
| `CodeExecutionRequest` | Python source. Runtime selection, permissions, host paths and Docker options are absent from its model-facing arguments. |
| `ExecutionOutcome` | Bounded stdout/stderr and optional top-level exception, plus separately tracked supervisor-observed termination: completed, timeout, cancellation, resource failure or infrastructure failure. |
| File-tool requests | Separate purpose-specific requests for listing, finding, searching and reading workspace files. No shell text or arbitrary utility-option passthrough. |

The LLM receives the language, package availability, container input catalog and
paths, output convention and limits. It does not need Docker, image or worker
implementation details. Hiding those details is an abstraction, not a security
control.

```text
Trusted caller
  |
  +-- AgentRuntimeConfig
  |     +-- RuntimeProfile
  |     +-- ExecutionPolicy
  |     +-- InputResource *
  |
  +-- ConfigurableAgent -------- owns conversation history
          |
          +-- Tool instances --- bind one AgentRuntime
                                     |
                                     +-- ResourceState
                                     +-- PreparedWorker (at most one ready)
                                     +-- private container session

Shared between agents: Docker client/daemon, immutable images, global admission
Private to each agent: workspace, interpreter state and tool output
Shared kernel: host Linux kernel, or the Linux kernel inside Docker Desktop
```

## Filesystem boundary: one private session per container

**Each Docker container has its own filesystem view.** Writable layers are unique
per container; reusing an immutable base image does not share an agent's edits.
Volumes and host bind mounts can introduce sharing, so our session configuration
uses neither. Filesystem separation does not provide a distinct kernel per agent,
encryption, or protection against the trusted host owner.
[Docker storage model](https://docs.docker.com/engine/storage/).

Mount runtime files read-only and keep `/workspace`, including
`/workspace/inputs`, in a bounded private container `tmpfs`. Bound other necessary
writable temporary paths separately. Only broker-selected input bytes cross from
host sources through the trusted importer. Do not expose source directories
through bind mounts, even read-only, or mount the Docker socket into a workload.

```text
Core broker -> Bollard -> local Docker daemon + approved image cache
                              |
                 +------------+------------+
                 |                         |
           Container A               Container B
           read-only runtime         read-only runtime
           private workspace         private workspace
           +-- inputs/               +-- inputs/
           +-- generated files       +-- generated files
                 |                         |
                 +------ shared kernel ----+

No shared writable workspace or host-directory mount
```

Filesystem isolation and retention are different properties. Stopping a container
removes its tmpfs contents but does not remove the container itself. Our
`close_runtime` and `reset_runtime` terminate and remove the owned container.
Treat an unexpected stop/restart as a lost session: discard that generation and
lazily recopy current host inputs into a new one. Do not resume a stopped
container as though its temporary workspace survived. Never stop the shared
Docker daemon or Desktop VM to clean up one agent.
[Docker temporary storage](https://docs.docker.com/engine/storage/tmpfs/).

## Runtime and package preparation

Use an approved Debian-based Python image with only the required runtime,
dependencies, trusted helpers and file utilities. Pin the Python/Debian versions,
package artifacts and image digest during the trusted build process. Prefer
compatible binary wheels; source builds require a deliberate build step. Package
hashes identify artifacts but do not establish their safety.

Debian's glibc environment fits conventional `manylinux` wheels. Alpine's musl
environment requires compatible `musllinux` wheels or additional builds, without
a demonstrated benefit for this workload. Provide native `linux/amd64` and
`linux/arm64` images and verify the daemon/image architecture; do not silently
substitute cross-architecture emulation.
See [Python image variants](https://hub.docker.com/_/python),
[wheel platform tags](https://packaging.python.org/en/latest/specifications/platform-compatibility-tags/),
[pip artifact verification](https://pip.pypa.io/en/stable/topics/secure-installs/).

At agent creation, select the approved image and prepare a container. Attempt to
prepare a Python worker and import NumPy/pandas before a code call arrives. Count
container resources and idle preloaded workers against the global memory budget,
with separate allowance for shared Docker/Desktop and broker overhead. Acquire
admission before starting the container or preloading a worker. If capacity is
unavailable, the logical agent remains available with execution readiness pending;
prepare on demand when admission becomes available. Do not make the number of
logical agents equal the number of simultaneously resident or executing workers.

After a code request, confirm worker termination before preparing its replacement.
Do not preload the next worker concurrently with untrusted execution. Preparation
is owned by the runtime lifecycle, bounded and cancellable. No `pip install`,
package resolution, mutable shared dependency cache or image build occurs inside
a data-bearing execution session.

```text
Trusted image build: CPython + approved wheels + trusted helpers -> pinned image

Worker preparation:
  start Python -> trusted imports -> establish restrictions -> Ready
                                                               |
                                                         one code request
                                                               |
                                              compile(source) -> bytecode
                                                               |
                                                       execute bytecode
                                                               |
                                                 capture output -> exit
```

CPython compiles source to Python bytecode; native extensions run their native
machine code within the same container boundary. This design does not depend on
Python's experimental JIT. Source parsing and compilation are part of the bounded
execution, since pathological source can exhaust resources before useful code
runs. [Python compilation semantics](https://docs.python.org/3/library/functions.html#compile).

## Lazy resources and session filesystem

At creation, validate configuration and destination names, and retain the source
list. Do not read resource contents or recursively enumerate folders. Preparing
the container and importing trusted libraries does not materialize user inputs.
The input catalog can advertise configured container names without reading files.

All five tools are file-capable. In particular, every `code_execution` call crosses
the resource-readiness barrier; do not attempt to infer future filesystem accesses
from arbitrary Python source.

```text
Agent creation
  |
  +-- record configured host sources and container names
  +-- prepare container / Python worker as admission permits
  +-- ResourceState = NotCopied
                  |
First code_execution / ls / find / grep / cat
                  |
          acquire agent operation lock
                  |
          ensure_resources_ready()
                  |
        +---------v---------------------------+
        | Read every configured source now   |
        | Stream into trusted importer       |
        | Complete and validate entire copy  |
        | Mark ResourceState = Ready         |
        +-----------------+-------------------+
                          |
                     execute tool
                          |
Later calls reuse and may edit the private copies
```

The import contract is:

1. A file maps to `/workspace/inputs/<name>`; a folder maps to that directory,
   preserving its descendant structure. Reject colliding aliases before execution.
   Other generated files stay inside `/workspace`.
2. Read host contents at first copying. Include edits made before that copy, but
   never automatically refresh afterward. Container edits, renames and deletions
   affect only the copies. An empty resource list becomes ready without transfer.
3. Accept regular files and directories only. Reject symlinks, Windows reparse
   points and special files rather than following them or silently omitting them.
   Traverse approved sources through validated handles with platform-appropriate
   race protections; checking a canonical path and later opening it by name alone is
   insufficient. Do not reproduce hard links in the destination archive.
4. Enforce actual streamed bytes, per-file size, file/entry count and transfer
   duration. Count content as it is read, rather than trusting initial metadata.
   Use bounded buffers and normalized relative archive paths; do not forward
   host ownership, privileged modes or arbitrary archive entries.
5. Ordinary copying does not provide an atomic multi-file snapshot. Report detected
   disappearance, type changes or inconsistent source reads. Do not promise
   point-in-time consistency while external writers modify the selected tree.
6. Import while no untrusted execution or file operation is active. Wait for
   importer completion and validation before publishing host-owned `Ready` state.
   Concurrent first-use calls wait on the same per-agent operation lock.
7. If transfer is partial, fails, exceeds a limit or is cancelled, do not execute
   the requested tool. Quarantine and discard that container session. Do not add
   incremental rollback or silently run with a partial input set.
8. Replacement starts a new session in `NotCopied`. Its next file-capable call
   imports the then-current host contents. Prior edits and outputs are lost; this
   is reported as a session reset, not represented as preserved execution state.

Linux's [handle-relative path-resolution controls](https://man7.org/linux/man-pages/man2/openat2.2.html)
illustrate the required containment property; they are not a portable macOS or
Windows implementation. Race-safe traversal of approved sources on each supported
host is a validation gate.

### Transfer into temporary mounts

Use bounded temporary mounts for session files. **Do not use Docker archive
upload/download or `docker cp` for the live tmpfs workspace.** Docker documents
copy limitations for these mounts; its archive filesystem view must not be assumed
to populate the mount visible to the running workload.
[Docker copy corner cases](https://docs.docker.com/reference/cli/docker/container/cp/#corner-cases),
[Moby container filesystem view](https://github.com/moby/moby/blob/master/daemon/containerfs_linux.go).

Core constructs a validated bounded stream and starts a fixed trusted importer
through Bollard's attached execution stream in the running container. The importer
writes inside the actual workspace mount and its resource domain. Close stdin,
drain bounded diagnostics and confirm successful completion before marking inputs
ready. Use a reviewed streaming format, such as a host-constructed tar containing
only normalized regular-file/directory entries; never extract an arbitrary supplied
archive with unchecked paths. The importer is internal infrastructure, not an
LLM-callable command or a general host-file access service.

The workspace is writable by generated code. The read-only-rootfs requirement
applies to runtime files, not these editable copies. Bound all writable locations,
including `/tmp`, caches and `/dev/shm`, with byte and inode limits. Temporary-file
memory counts toward the container memory budget; it is not free additional
capacity. Temporary mounts may use swap, and deletion is not forensic secure
erasure. Closing or replacing the container discards session files.
[Docker tmpfs accounting](https://docs.docker.com/engine/storage/tmpfs/),
[Linux tmpfs limits](https://docs.kernel.org/filesystems/tmpfs.html).

Validate stdin EOF, exit status, bounded diagnostics and actual visibility in the
live mount for the supported Docker/Bollard combination. Durable artifact export
is deferred. If later introduced, it also needs a trusted stream from the live
mount, explicit ownership/size checks and its own retention policy; it must not
silently reuse the unsuitable archive-copy path.

## Rust Docker client and adapter

Select **Bollard**, behind a small core-owned adapter. The researched release is
**0.21.1**, under Apache-2.0. Its Tokio integration, local Unix sockets, Windows
named pipes and asynchronous execution streams fit core. Use local transports,
pin the tested dependency and keep generated Bollard types out of agent/tool
contracts. Image builds and dependency resolution remain separate from execution.
[Bollard documentation](https://docs.rs/bollard/0.21.1/bollard/),
[tagged manifest](https://github.com/fussybeaver/bollard/blob/v0.21.1/Cargo.toml).

| Candidate researched | Assessment |
| --- | --- |
| **Bollard 0.21.1** | Selected: direct Engine API control, explicit API negotiation, async exec streams and documented native Windows transport. |
| **docker-api 0.14.0** | Capable handle-based alternative with an older documented model baseline. Version adjustment follows the server rather than negotiating a client-supported ceiling. |
| **Shiplift 0.7.0** | Basic async container/image/exec operations; no identified advantage for this integration. |
| **Testcontainers 0.28.0** | Optional test-fixture library; its documentation warns against assuming suitability for production lifecycle management. |

Sources: [docker-api](https://docs.rs/docker-api/latest/docker_api/),
[version adjustment](https://docs.rs/docker-api/latest/src/docker_api/docker.rs.html),
[Shiplift](https://docs.rs/shiplift/latest/shiplift/),
[Testcontainers production guidance](https://docs.rs/testcontainers/latest/testcontainers/#usage-in-production-code).

Select and validate a local endpoint explicitly; do not silently follow a remote
Docker context or transmit personal inputs to a remote daemon. Call
`negotiate_version`, then check minimum capabilities and effective controls.
Negotiation alone does not establish policy enforcement. Missing prerequisites
produce an actionable error; do not silently switch runtimes.

| Adapter responsibility | Bollard integration |
| --- | --- |
| Local connection and compatibility | Unix socket/named-pipe connection, explicit API negotiation, daemon version/info and Linux-container mode checks. |
| Approved image availability | `inspect_image`, `create_image` for an approved pull, bounded progress consumption and immutable image/architecture verification. |
| Container lifecycle | Create, start and inspect; explicit kill, wait and remove operations for the owned container. |
| Importer and workers | `create_exec`, `start_exec`, `inspect_exec`; fixed helper argv, explicit identity/workdir and `tty = false`. |
| Resource observation | Inspection/statistics plus core-owned admission, output and deadline accounting. |

Attached execution provides asynchronous stdin and output streams for concurrent
transfer and draining. Pass source and imported bytes as data, never host shell
text. [Execution stream API](https://docs.rs/bollard/0.21.1/bollard/exec/enum.StartExecResults.html).

Use generated identities owned by this runtime and reconcile only those sessions.
Do not stop the shared daemon/Desktop VM, prune global resources or target unrelated
containers. Docker installation, upgrades and host configuration are outside tool
execution. Treat source, imported bytes and output as untrusted; diagnostics must
not become commands or lifecycle authority.

Application output limits are independent of library buffer settings. The reviewed
Bollard output-capacity option is an initial decoder capacity, not an aggregate
quota. A post-decode cap also does not establish a strict allocation ceiling when
a decoder waits for a complete frame. Validate output floods against the supported
daemon and account for broker buffers.
[Bollard exec implementation](https://github.com/fussybeaver/bollard/blob/v0.21.1/src/exec.rs),
[decoder](https://github.com/fussybeaver/bollard/blob/v0.21.1/src/read.rs).

## Proposed functions and project organization

Preserve `agent::tool::Tool::definition` and `execute`, the existing tool schema
validation, conversation history and agent-loop stopping rules. Tool implementations
bind the appropriate `AgentRuntime`; the LLM cannot select another agent's runtime.
The loop may still submit concurrent tools, but this runtime serializes its own
operations. Different agents can execute concurrently within global admission.

Place future adapters under `agent::tool` and runtime ownership in a dedicated
`execution` module within core. Keep configuration/models, runtime lifecycle and
Docker integration conceptually separate, without a generic multi-backend framework.
The private `execution` module initially contains only the prerequisite adapter;
the runtime ownership and tool APIs below remain proposed.

| Proposed boundary | Responsibility |
| --- | --- |
| `create_runtime(config)` | Validate configuration and Docker capabilities, select the approved image, create the container and arrange bounded preparation. Do not copy input contents. |
| `prepare_worker(runtime)` | Acquire resident-memory admission, preload from trusted files and establish restrictions before accepting code. |
| `ensure_resources_ready(runtime)` | Under the agent operation lock, perform the one-time import or return immediately when ready for this session generation. |
| `execute_code(runtime, request)` | Ensure resources and worker readiness, consume one worker, compile/execute source, capture bounded results and confirm worker exit. |
| `list_files`, `find_files`, `grep_files`, `read_file` | Ensure resources and perform typed operations within the workspace under the same lock. |
| `reset_runtime(runtime)` | Quarantine, terminate and confirm cleanup, discard session data, then create a new generation. |
| `close_runtime(runtime)` | Cancel owned preparation/execution, terminate and remove the owned container and release admission only after cleanup is confirmed. |

The model-facing file tools expose intended operations, not raw argv. In particular,
`find` execution/deletion actions and shell pipelines are not available. Validate
workspace paths and link behavior even though host files are not mounted. The host
launches approved utilities independently; generated Python cannot spawn them.

Normal code results contain bounded stdout/stderr; a caught top-level exception
provides its type and bounded message without wrapper stack frames or exception
chains. Explicit traceback text printed by generated code remains untrusted stderr.
An exception catcher cannot reliably observe native crashes, `os._exit`, external
kills or interpreter tampering. Core-observed exit and termination state therefore
remain authoritative. Report infrastructure failures using safe tool diagnostics,
not raw Docker responses or host tracebacks. Never unpickle worker-supplied data.

Drain output streams concurrently, enforce total byte caps and mark truncation or
limit termination explicitly. Do not mistake a library buffer setting for an
application-wide output bound. [Pipe and buffering hazards](https://docs.python.org/3/library/subprocess.html#subprocess.Popen.communicate).

## Containment and cancellation

### Mandatory execution policy

The broker owns all Docker options. Generated code receives no Docker socket,
registry/provider credentials, host environment, host directories or device
passthrough. Require non-root workloads, read-only runtime files, bounded writable
mounts, all capabilities dropped, `no-new-privileges`, seccomp filtering, no
privileged mode or host namespaces, and no published ports.
[Docker security](https://docs.docker.com/engine/security/),
[container controls](https://docs.docker.com/reference/cli/docker/container/run/).

Disable external networking with Docker's `none` network, then verify that host,
external and other-agent network access is unavailable. Loopback remains; omitting
DNS or published ports alone is insufficient.
[Network-none semantics](https://docs.docker.com/engine/network/drivers/none/).

Set and verify CPU, memory/swap, PID/thread and storage limits on the actual daemon,
including the workers started through exec. Bound output and duration in core.
Rootless Docker is preferred on Linux where required controls work, but rootless
operation alone does not prove enforcement. Missing mandatory controllers or
restrictions must fail closed. Global admission includes scratch memory, idle
workers and broker buffers, with separate allowance for shared Docker/Desktop
overhead. Container limits are not a complete bound on total host resource use.
[Docker resource constraints](https://docs.docker.com/engine/containers/resource_constraints/),
[rootless resource prerequisites](https://docs.docker.com/engine/security/rootless/tips/#limiting-resources).

### Code-phase process restriction

Docker's default seccomp policy permits ordinary subprocess creation. A minimal
utility set or PID limit does not satisfy the selected no-subprocess contract.
The required promise remains **no new processes or executable replacement;
bounded threads allowed**. The OS cannot distinguish numerical-library threads
from user-created threads. Validate the additional policy on both approved CPU
architectures. [Docker seccomp policy](https://docs.docker.com/engine/security/seccomp/),
[default profile](https://github.com/moby/profiles/blob/main/seccomp/default.json).

A candidate implementation is a trusted native code-phase transition after
approved imports, installing an additional irreversible seccomp filter synchronized
across all existing worker threads with `SECCOMP_FILTER_FLAG_TSYNC`. Accept source
only after confirmed success; any synchronization failure invalidates the worker.
Cover process creation, `execve`/`execveat`, alternate syscall ABIs and cross-process
injection. Only reviewed thread-creation forms may remain available. `clone3`
requires explicit handling because its flags are behind a pointer; denying it with
`ENOSYS` and using a tested libc fallback is a candidate, not a compatibility promise.
[Seccomp synchronization](https://man7.org/linux/man-pages/man2/seccomp.2.html),
[clone semantics](https://man7.org/linux/man-pages/man2/clone.2.html),
[kernel filter limitations](https://docs.kernel.org/userspace-api/seccomp_filter.html).

The transition covers the worker's thread group, not other container processes.
Protect any less-restricted keeper or helper through separate credentials and
validated process-access restrictions; provide no arbitrary command channel to it.
Confirm that process-memory interfaces cannot bypass the policy.

Every replacement worker bootstraps from trusted read-only paths, isolated Python
search paths, sanitized environment and controlled library configuration/cache
locations. Never allow editable session modules, configuration or serialized state
to execute during preparation before restrictions. Python `-I` is useful but is
not by itself a complete native-library initialization policy.
[Python isolated mode](https://docs.python.org/3/using/cmdline.html#cmdoption-I).

These mechanisms are feasibility gates for the later implementation, not controls
already established or tested by this research. Unsupported approved-package
behavior must fail explicitly rather than silently weakening confinement.

### Completion and interruption

```text
container session:
  Preparing -> Available -> Busy -> Available
                  |          |
                  +----------+-- failure / cancellation
                                      |
                                  Quarantined
                                      |
                             terminate and delete
                                      |
                         Closed or new session generation

input state: NotCopied -> Copying -> Ready
                            |
                         failure -> discard session
```

A completed worker is consumed permanently, including all its threads. Syntax and
ordinary Python exceptions can be reported without destroying a healthy container
when worker termination is confirmed. Timeout, native crash, resource failure or
uncertain process state requires whole-container recovery, with explicit loss of
session files. Never automatically retry generated code after an uncertain outcome.

Dropping a Rust future, closing stdin or dropping a Bollard attach stream is not
proof that execution stopped. On cancellation, the owned runtime supervisor must
request termination, quarantine the session and retain accounting until cleanup is
confirmed. Async cleanup cannot simply be awaited from `Drop`; reconcile this with
the existing `Tool` cancellation contract without untracked background execution.
The loop's interrupted-result/history behavior remains truthful while cleanup is
pending. Daemon disconnection leaves state uncertain and unavailable until
reconciled. Confirm container termination and removal, without stopping the shared
daemon or Desktop VM. Recover abandoned owned sessions on application restart;
never reuse an uncertain session or release its reserved capacity prematurely.
[Docker exec lifecycle](https://docs.docker.com/reference/cli/docker/container/exec/),
[attach/detach behavior](https://docs.docker.com/reference/cli/docker/container/attach/).

### Privacy and platform boundaries

Local execution does not imply local-only results. Code output and file-tool
results enter conversation history and are sent to the provider on subsequent
model turns. Existing explicit DEBUG content logging also prints tool arguments,
including generated source. Keep default runtime diagnostics to metadata, and do
not introduce input/output content or private host paths into ordinary logs.
File reads through `cat` or `grep` are explicit disclosures through the tool result.

| Host/backend | Deployment boundary and compatibility target |
| --- | --- |
| Linux / Docker Engine | Containers share the host kernel. Rootless operation reduces daemon authority but does not create a separate kernel. |
| macOS / Docker Desktop | Linux containers run in Docker's shared VM; there is not one VM per agent. |
| Windows / Docker Desktop | Require Linux-container mode. WSL2 and Hyper-V have different host-security properties; do not claim identical isolation or support Windows-container images. |
| Apple `container` and other per-agent VMs | Deferred alternatives; not required or selected for the initial Docker implementation. |

Docker's optional Enhanced Container Isolation is not assumed. Backend and
resource-control compatibility must be established on each supported configuration.
[Docker Desktop macOS boundary](https://docs.docker.com/desktop/setup/install/mac-permission-requirements/),
[Windows backend differences](https://docs.docker.com/desktop/enterprise/hardened-desktop/enhanced-container-isolation/limitations/),
[Microsoft WSL security model](https://wsl.dev/technical-documentation/security/).

Local execution remains the selected data flow. Managed execution, telemetry or
remote image/service endpoints must not receive private input contents as a side
effect of backend selection. Registry access for approved image preparation is
separate from network permission for generated code. Runtime metadata/diagnostic
logging must also be checked for content retention outside the temporary workspace.

## Alternatives: languages and isolation

Language choice and security boundary are separate decisions. JavaScript and Python
can both exhaust resources, disclose granted data or misuse available host APIs.
The boundary must contain the interpreter and its native dependencies, not merely
remove selected language functions.

### Language/runtime comparison

| Candidate | Compilation and packages | Boundary and operational tradeoff | Decision |
| --- | --- | --- | --- |
| QuickJS / QuickJS-NG in Wasmtime | JS source becomes interpreter bytecode. Wasmtime compiles the interpreter's Wasm to native code; this is not an optimizing JS JIT. No automatic Node/npm environment. | Small-runtime candidate; share compiled code, create fresh stores/instances and grant narrow imports. Exact footprint requires measurement. | Strong alternative for constrained JS transformations; does not satisfy the selected Python ecosystem goal. |
| Javy in Wasmtime | Packages QuickJS bytecode with a runtime or matching dynamic plugin. Compiled artifacts are runtime-version coupled; Node APIs are not supplied. | Convenient distribution of compiled JS applications. Per-snippet artifacts add preparation and cache complexity. | Not selected for this execution tool. |
| CPython WASI in Wasmtime | CPython bytecode execution with limited platform APIs. Native extensions need builds for the selected Wasm ABI. | Good candidate for stdlib/compatible packages; a maintained general scientific distribution was not established by this research. | Not selected for general Python packages. |
| componentize-py | Packages Python behind component interfaces; preinitialization can execute Python during the build. | Useful for trusted prebuilt applications/runners. Treating generated-code componentization as passive compilation is unsafe. | Possible trusted packaging tool, not a per-snippet compilation path. |
| RustPython WASI | Alternative interpreter; compatibility and production maturity differ from CPython. | Requires additional workload/package validation, with no demonstrated advantage here. | Research alternative. |
| Pyodide | CPython/Emscripten with a maintained scientific catalog, including NumPy/pandas/SciPy/matplotlib. | Requires a JS host rather than stock Wasmtime. Python intentionally accesses exposed JS globals, so the host's capabilities and outer isolation matter. Conventional native packages remain constrained. | Viable curated scientific option, but a more complex stack for this Rust core. |
| Native CPython | Broadest conventional wheel/native-extension compatibility. Source compiles to Python bytecode; native libraries run natively. | Requires OS/container/VM confinement and external resource supervision. | Selected inside Docker Linux containers. |
| Native embedded JS / V8 contexts | Can provide faster or simpler native JS integration for some workloads. | A language context is not an independent hostile-code OS boundary; native engine compromise remains relevant. | Not a replacement for containment. |

Primary evidence: [QuickJS](https://bellard.org/quickjs/quickjs.html),
[QuickJS-NG](https://github.com/quickjs-ng/quickjs),
[Javy](https://github.com/bytecodealliance/javy),
[Python WebAssembly limits](https://docs.python.org/3/library/intro.html#webassembly-platforms),
[componentize-py](https://github.com/bytecodealliance/componentize-py),
[RustPython](https://github.com/RustPython/RustPython),
[Pyodide packages](https://pyodide.org/en/stable/usage/packages-in-pyodide.html),
[Pyodide JS bridge](https://pyodide.org/en/stable/usage/type-conversions.html#importing-javascript-objects-into-python),
[Node VM security limitation](https://nodejs.org/api/vm.html).

NumPy/pandas on WASI is possible: experimental builds exist. The reviewed
[wasi-wheels project](https://github.com/dicej/wasi-wheels) explicitly warns that it
is an unmaintained proof of concept. This supports a maintenance/packaging concern,
not a claim that scientific Python in Wasm is impossible. WASIX also advertises a
separate Python package ecosystem; it is a different runtime/ABI candidate and does
not establish stock Wasmtime compatibility.
[WASIX package documentation](https://www.wasix.org/docs/language-guide/python/installation/).

If Wasmtime is revisited, compile generated source inside a bounded guest using a
trusted prebuilt interpreter. Share immutable compiled modules, not live guest
heaps. WASI capabilities can restrict filesystem access, but preopens do not supply
disk quotas or protection from the host administrator. Fuel/epochs do not bound
all host calls, allocations or compilation work. Serialized native Wasmtime
artifacts must come from a trusted path; never deserialize model-supplied compiled
artifacts. [Wasmtime security](https://docs.wasmtime.dev/security.html),
[interruption](https://docs.wasmtime.dev/examples-interrupting-wasm.html),
[resource limiting](https://docs.rs/wasmtime/latest/wasmtime/trait.ResourceLimiter.html),
[compiled-module trust](https://docs.rs/wasmtime/latest/wasmtime/struct.Module.html#method.deserialize).

### Deployment and lifecycle comparison

| Alternative | Benefits | Costs and limits |
| --- | --- | --- |
| Wasmtime in the core process | Portable embedding and shared compiled runtimes. | Runtime/host-binding compromise shares the core process failure domain. Wasm-compatible packages required. |
| Wasmtime in a separate process | Adds crash handling and termination control. | A same-user subprocess alone does not restrict host access after an escape; OS confinement is a separate control. |
| Native per-OS sandboxes | Native packages without a mandatory Linux VM. | Linux namespaces/seccomp/cgroups, Windows AppContainer/Job Objects and macOS App Sandbox need separate implementations and validation. |
| Per-agent Docker container | Common Linux runtime, shared immutable images, amortized container creation and simple session files. | Shares a kernel with other containers; Desktop adds a shared VM. Selected for initial simplicity and compatibility. |
| Per-agent Apple container / VM | OCI packaging and a separate guest kernel/filesystem per agent through a native Mac runtime. Deferred alternative. | Apple silicon/macOS prerequisite, guest memory and host service overhead. Fresh Python processes do not reset the VM or its session files. |
| Fresh container per call | Clear whole-container process reset and termination boundary. | Pays container lifecycle costs each call and needs separate handling for retained agent files. |
| Warm dedicated Linux VM | Common Linux policy and an explicit hypervisor boundary to the host. | Guest/VMM resident memory, provisioning and patching. Fresh processes can run inside it without booting per call. |
| Fresh VM per call | Stronger reset between executions, including guest-kernel state. | Boot/restore, memory and device-management costs; a different assurance/performance tradeoff. |

Native OS isolation was considered before Docker. Windows AppContainer and Job
Objects address different responsibilities; Linux controls require usable
namespaces/controllers. macOS App Sandbox associates storage with an app identity,
not automatically with each invocation. Random directories in a shared writable
container do not establish access control. Docker provides a common Linux workload
environment across the host compatibility targets without building separate native
runtime adapters initially, at the cost of a shared-kernel boundary.
[Bubblewrap](https://github.com/containers/bubblewrap),
[Windows AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation),
[Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects),
[Apple App Sandbox files](https://developer.apple.com/documentation/security/accessing-files-from-the-macos-app-sandbox).

### Further sandbox research and disposition

These alternatives were reviewed after the original Docker decision. The final
2026-10-09 decision selects Docker/Bollard; none below is an additional backend to
implement in this phase.

| Alternative | Assessment for this scope |
| --- | --- |
| Apple `container` | Research covered release 1.5.0: Apple silicon/macOS 26, one VM and independent filesystem per container. Its CLI/XPC integration differs from Docker. Deferred in favor of broader initial compatibility. |
| Firecracker | Direct Linux/KVM microVM option; needs guest artifacts, an execution service and lifecycle/transport integration. A future Linux specialization, not a native Mac backend. |
| Kata Containers | VM-backed container runtime with documented Docker integration. Could retain a Docker/Bollard-facing interface on a configured Linux host; adds runtime/VMM provisioning. |
| Cloud Hypervisor | Linux VMM with broader VM functionality; no current requirement justifies another integration. |
| gVisor | Linux userspace-kernel boundary, not a microVM. Package/syscall compatibility and overhead require validation; deferred. |
| QEMU | Hardware-accelerated backends across host OS families; requires guest/control integration beyond the selected Docker adapter. Deferred. |
| libkrun | Potential small VM integration; requires separate host confinement and lifecycle ownership. Deferred. |
| Docker Sandboxes | Separate product from Docker Engine, with local microVMs on supported desktop platforms. Account/product requirements and host-access defaults need review; not selected. |
| LangSmith Sandboxes | Managed microVM service with self-hosted infrastructure options. Uploads and retention would change the local data flow; not selected. |
| Deep Agents sandbox backends | Useful examples of provider adapters, not an independent isolation mechanism or reason to replace the Rust agent loop. |
| Archived `langchain-sandbox` | Local Pyodide/Deno wrapper; maintainers advise against production use. Not selected. Pyodide itself remains a separate runtime alternative above. |

Primary sources: [Apple container](https://github.com/apple/container/blob/1.5.0/README.md),
[Apple architecture](https://github.com/apple/container/blob/1.5.0/docs/technical-overview.md),
[Firecracker](https://github.com/firecracker-microvm/firecracker),
[Kata installation](https://github.com/kata-containers/kata-containers/blob/main/docs/installation.md),
[Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor),
[gVisor architecture](https://gvisor.dev/docs/architecture_guide/intro/),
[QEMU](https://www.qemu.org/docs/master/system/introduction.html),
[libkrun security model](https://github.com/libkrun/libkrun/blob/v1.19.6/README.md#security-model),
[Docker Sandboxes](https://docs.docker.com/ai/sandboxes/install/),
[LangSmith Sandboxes](https://docs.langchain.com/langsmith/sandboxes),
[Deep Agents backends](https://docs.langchain.com/oss/python/deepagents/sandboxes),
[LangChain Sandbox maintenance status](https://github.com/langchain-ai/langchain-sandbox).

Maintain a versioned runtime inventory and patch the host OS, Docker stack, base
image, Python, native libraries and Rust client. Small images do not replace
patching or policy. Record licenses/notices for the actual shipped components.
Bollard is Apache-2.0; runtime distributions and bundled image dependencies have
their own terms. Alternatives also carry runtime/package licensing: QuickJS/NG
are MIT, CPython uses the PSF license, and Pyodide is MPL-2.0 with additional
bundled-package licenses. One license does not cover the entire runtime image.
[Bollard manifest](https://github.com/fussybeaver/bollard/blob/v0.21.1/Cargo.toml),
[QuickJS-NG license](https://github.com/quickjs-ng/quickjs/blob/master/LICENSE),
[Python license](https://docs.python.org/3/license.html),
[Pyodide license](https://github.com/pyodide/pyodide/blob/main/LICENSE).

## Performance evidence and intended measurements

There is no justified universal percentage for Docker, OS-sandbox or VM overhead
for this workload. Separate setup, imports, data transfer, computation and cleanup.
Hardware virtualization on the native CPU architecture is a different experiment
from cross-architecture emulation. Differences in NumPy/BLAS builds must not be
attributed automatically to isolation overhead.

```text
Agent activation: Docker readiness + container startup + Python startup + imports
First file use:  source traversal + transfer + importer completion
Ready code call: queue + source compile + compute + bounded output + worker exit
Replenishment:  fresh Python startup + imports + policy transition
Idle memory:    container + prepared worker; shared Docker/Desktop overhead
```

Prewarming moves work earlier and consumes idle memory; it does not eliminate
imports or their CPU cost. Warm OS caches can share immutable runtime pages without
sharing a used interpreter. Forkserver templates and pristine VM snapshots remain
deferred optimizations. Do not fork the personal-agent process with its credentials
and history, or substitute Python subinterpreters for an OS boundary.
[Python process-start tradeoffs](https://docs.python.org/3/library/multiprocessing.html#contexts-and-start-methods),
[subinterpreter isolation limits](https://docs.python.org/3/library/concurrent.interpreters.html#multiple-interpreters-and-isolation).

Start scheduling experiments with one numerical-library thread per executing job,
then measure larger allocations for workloads that benefit. Environment settings
guide approved libraries but do not replace enforced container budgets. Native
libraries may release the GIL and use multiple threads; multiple agents can
otherwise multiply thread pools and oversubscribe the device.
[NumPy threading controls](https://numpy.org/doc/stable/reference/global_state.html),
[SciPy parallel execution](https://docs.scipy.org/doc/scipy/tutorial/parallel_execution.html).

### Local baseline recorded during research

Measurements used an **Apple M5 Pro, 64 GiB RAM, 18 logical CPUs, macOS 27.0.1,
CPython 3.14.8**. A Python harness launched fresh subprocesses with `-I -B`, an
empty environment, no input, discarded stdout and captured stderr. Each case had
three warm-up runs followed by 30 measured runs using `time.perf_counter`.
The p95 values below use the harness's lower order-statistic estimate.

These processes inherited the research tool environment's restrictions; no
application-specific Docker or native sandbox was added. Page caches were warmed,
and power/background-load conditions were not controlled. NumPy/pandas/SciPy/
matplotlib were absent from this interpreter. The configured Docker socket was
unavailable; no Docker workload was measured. No Apple container workload was
measured in this research either.

| Fresh subprocess workload | Median | p95 estimate |
| --- | ---: | ---: |
| `pass` | 19.57 ms | 24.73 ms |
| `import csv, json, statistics, decimal, pathlib` | 31.51 ms | 32.36 ms |
| `sum(i*i for i in range(1000000))` | 41.14 ms | 41.81 ms |

A separate minimal process importing `resource` reported peak RSS of 15,171,584
bytes, approximately 14.5 MiB. This is not a scientific-worker memory estimate.

For 32 fresh processes executing the CPU case, the same harness recorded:

| Parallel jobs | Total batch time | Median job latency | Throughput |
| ---: | ---: | ---: | ---: |
| 1 | 1,305.24 ms | 40.32 ms | 24.52 jobs/s |
| 4 | 338.71 ms | 41.84 ms | 94.48 jobs/s |
| 8 | 197.18 ms | 47.06 ms | 162.29 jobs/s |
| 16 | 127.57 ms | 51.94 ms | 250.84 jobs/s |

These observations distinguish throughput from individual latency. They do not
set production concurrency, prove containment, or predict heavy dataframe workloads.

Later measurements must separate cold Docker/Desktop activation from cached-image
container startup; worker imports and readiness; first-copy transfer; prepared and
unprepared calls; file tools; numerical work; replenishment; and cleanup. Record
p50/p95 latency, throughput, cancellation latency and total resident/peak host
memory at controlled concurrency. Include shared daemon/Desktop VM memory and idle
workers; summing process RSS can double-count shared pages.

Measure retained host memory after large NumPy/pandas jobs and container removal;
do not infer total host reclamation from a worker's exit. Account for temporary
storage, Docker logs and leftover owned sessions. Never restart the shared daemon
or Desktop VM to reclaim one agent's resources. Container restart loses its tmpfs
workspace and must follow the explicit reset contract.

Use no-op, JSON/text, NumPy computation and pandas CSV/groupby/join cases, plus small
and larger input trees. Keep images, datasets, architecture, BLAS backend, thread
limits and resource budgets explicit. Broader scientific profiles need their own
compatibility/import measurements. No latency SLA or numeric production budget is
established by this research; configuration must supply budgets explicitly.

## Validation gates for later implementation

These are proposed confidence tests and experiments, **not tests implemented or
passed by this ADR**. Prefer integration checks that exercise actual containment
and lifecycle behavior over tests written merely for coverage.

| Gate | Required evidence |
| --- | --- |
| Docker/Bollard compatibility | Validate local Unix sockets/Windows named pipes, negotiated API and daemon capabilities, Linux-container mode, image digest/native architecture and effective policy. Reject remote or unsupported configurations. |
| Separate agent filesystems | Run two containers from the same image; write the same path with different contents. Verify neither sees the other's mutations, host originals or forwarded host services. Verify no shared writable volume or user host-directory mount. |
| Lazy creation and first use | Creating an agent does not read resource contents or recursively enumerate them. Concurrent first tool calls cause one complete import for that session before either operates. Empty lists need no transfer. |
| Resource semantics | Edits before first copying are observed; edits afterward do not refresh copies. Container mutations do not touch host originals. Reset discards changes and later imports current host contents. |
| Source and destination safety | Exercise nested folders, alias collisions, invalid destination names, missing files, links/reparse points, special files, replacement races and detected changes during transfer. No escape from authorized sources. |
| Transfer correctness and limits | Verify binary data, stdin EOF/half-close and simultaneous output backpressure through Bollard exec streams on supported transports. Confirm files appear in the live tmpfs. Partial transfer, timeout or quota failure cannot publish readiness. |
| Fresh workers and preparation | Mutate globals, modules and handlers, then verify the next worker is fresh. Plant workspace modules/configuration and confirm they cannot affect trusted preloads. Count container resources and idle workers in admission; include shared Docker/Desktop overhead. |
| Process policy | Attempt fork, subprocess launch, process-form clone, executable replacement and cross-process injection. Verify preexisting and late-created numerical threads on both approved architectures; restriction-installation failure must prevent source execution. |
| Containment and resource exhaustion | Deny host/other-agent/runtime-service/credential access and external networking. Exercise memory, PID/thread, file-byte/inode, open/deleted-file and simultaneous stdout/stderr exhaustion. Verify limits on actual exec workers, not only container init; account for container and host memory. |
| Completion and cancellation | Cover Python exceptions, abrupt exit, native crash, timeout, dropped tool future, dropped attach stream, daemon disconnection, unexpected container restart and cleanup failure. Never reuse a quarantined session or release its accounting before container termination and removal are confirmed. Verify restart reconciliation does not touch unrelated containers. |
| File tools | Reject workspace escapes, unsafe link behavior, shell interpolation, arbitrary flags and `find` execution/deletion actions. Bound traversal and output. |
| Privacy and reporting | Default broker/runtime diagnostics omit source, file contents and outputs; errors omit host internals. Returned content and explicit DEBUG source logging are documented disclosures, not described as local-only data. |
| Mixed-workload performance | Measure activation, lazy copying, prepared execution, imports/replenishment, contention and total memory. Maintain safety gates under every optimization. |

## Consequences and deferred work

The selected direction retains conventional Python-package compatibility and one
Linux execution environment across Docker hosts. Bollard provides a small Rust
integration with the Engine API. This introduces a local Docker prerequisite,
image/runtime maintenance and a shared-kernel security boundary. Separate agent
filesystems do not provide separate guest kernels.

Preloading improves ready-call latency while increasing idle memory. Lazy inputs
avoid copying unused resources but charge first use for the complete transfer.
Private editable temporary copies provide a workspace without host write-back.
Session files persist across calls only while that container session remains
alive; close, replacement or an unexpected stop discards them. Removal and runtime
cleanup are explicit, and deletion is not a secure-erasure guarantee.

Fresh workers do not reset container state or restore edited workspace files.
Whole-container replacement is the recovery boundary when execution is uncertain.
Strict process restrictions, race-safe host copying, temporary-mount limits,
reliable cancellation/cleanup and measured aggregate resource use remain release
gates. Choosing a simple Docker integration does not mean these gates have passed.

Apple container/Swift/XPC integration, direct Firecracker/Kata/QEMU backends,
VM snapshots, managed remote execution, Wasmtime/JavaScript implementation, GPU
support, generated subprocesses, runtime package installation, durable artifact
storage, inter-agent file/message-board communication and deployment automation
are deferred. Future changes to these capabilities require their own explicit
architectural decision. The basic image/startup implementation above does not
introduce these deferred execution capabilities.
