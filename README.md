# Murmeli

Murmeli is your AI sidekick for software engineering: helping review code, take notes, and keep decisions organised. A little Swiss marmot with an eye for bugs and a notebook full of context.

The first milestone is a standalone Rust process serving a local **Hello world** page. Future milestones will add Murmeli's application runtime and a TypeScript Visual Studio Code extension that connects to core.

## Prerequisites

- Install Rust through [rustup](https://rustup.rs/), including the native build tools required for your platform. On macOS, install the Xcode Command Line Tools with `xcode-select --install` if needed.
- Install [Node.js 24 LTS](https://nodejs.org/en/download), which includes npm. npm runs the root commands; the server itself is a Rust executable.

From the repository root, run:

```sh
rustup show
```

The checked-in `rust-toolchain.toml` selects Rust **1.98.1** and installs its minimal toolchain plus `rustfmt` and `clippy` when needed. The first build also downloads the Rust dependencies recorded in `Cargo.lock`. These initial downloads require internet access. No `npm install` is needed: there are no npm dependencies.

## Run locally

From the repository root:

```sh
npm run dev
```

Open **http://127.0.0.1:3000/**. The server prints its URL after binding successfully. Stop it with **Ctrl+C**.

Use a different port on macOS or Linux:

```sh
MURMELI_PORT=4000 npm run dev
```

Then open **http://127.0.0.1:4000/**. Core binds exclusively to `127.0.0.1`; it is accessible from your own machine.

`MURMELI_PORT` defaults to `3000` only when unset. If supplied, it must be an integer from `1` through `65535`. Empty, malformed, zero, out-of-range, and non-Unicode values fail with an error and a nonzero exit status. A port already in use also causes startup to fail; choose another port or stop the process occupying it.

To inspect the response from another terminal:

```sh
curl -i http://127.0.0.1:3000/
```

Expect HTTP `200`, `Content-Type: text/html; charset=utf-8`, and a page containing `<h1>Hello world</h1>`.

To build and launch the standalone executable:

```sh
npm run build
./target/release/murmeli-core
```

The HTML is embedded in the executable at compile time, so the binary can run from any working directory without a separate assets folder. After editing `core/web/index.html`, stop and restart `npm run dev`, or rebuild the release executable, to include the changes. There is no automatic reload yet.

## Project organization

```text
murmeli/
|-- Cargo.toml                  Rust workspace; core is its only member
|-- Cargo.lock                  Resolved Rust dependencies, tracked in Git
|-- rust-toolchain.toml         Compiler and tooling versions
|-- package.json                Root commands; no npm dependencies
|-- README.md                   Setup, behavior, and structure
|-- .github/workflows/ci.yml    Formatting, linting, and tests
|-- core/
|   |-- Cargo.toml              Binary package: murmeli-core
|   |-- src/
|   |   |-- common/
|   |   |   |-- mod.rs          Exposes the config module
|   |   |   `-- config.rs       Configuration, port parsing, parser test
|   |   |-- main.rs             Startup and server lifecycle
|   |   `-- web.rs              Router, HTML handler, route test
|   `-- web/
|       `-- index.html          HTML embedded by include_str!
`-- murmeli-ext/
    `-- README.md              Future TypeScript VS Code extension
```

The root is a virtual Cargo workspace using resolver `3`. It owns the shared lockfile and `target/` build directory. `core` is a single unpublished Rust binary package using edition **2024**, **Axum 0.8.9** for HTTP routing, and **Tokio 1.53.1** for asynchronous execution. Axum enables only `http1` and `tokio`; Tokio enables `macros`, `net`, and `rt-multi-thread`. Tower `0.5` with `util` is used only in tests.

The root build currently builds core. `murmeli-ext` reserves the location for the future extension; it has no package or build step yet. Tokio is the asynchronous runtime for the server; Murmeli's future application runtime is still to be implemented.

```text
Terminal                         core process
   |                                  |
   | npm run dev                      |
   +--------------------------------->|
   |                        Config::from_env() -> validated settings
   |                        config.bind_addr() -> local address
   |                        bind 127.0.0.1:port
   |<----------------------- print listening URL
                                      |
Browser                               |
   | GET /                            |
   +--------------------------------->|
   |                        router -> index()
   |<----------------------- 200 + embedded HTML
```

## Models and functions

`common::config::Config` is the single settings struct. It stores a private `port: u16` validated during construction. `main` loads and owns the configuration once before binding the listener. There are no domain models yet.

| Type | Responsibility |
| --- | --- |
| `common::config::Config` | Server settings, visible within core; keeps the validated port private |
| `u16` | Port number, validated to exclude zero |
| `std::net::SocketAddr` | IPv4 loopback address and configured port returned by `Config::bind_addr` |
| `tokio::net::TcpListener` | Bound local network listener |
| `axum::Router` | Registered HTTP routes |
| `axum::response::Html<&'static str>` | Embedded HTML response |
| `std::io::Result<T>` | Configuration and server success/error result |

| Function | Responsibility |
| --- | --- |
| `async main() -> io::Result<()>` | Load configuration once, bind the listener, print the URL, and serve |
| `Config::from_env() -> io::Result<Self>` | Read `MURMELI_PORT`, validate it, and construct the settings |
| `Config::bind_addr(&self) -> SocketAddr` | Combine IPv4 loopback (`127.0.0.1`) with the configured port |
| `parse_port(value: Option<&str>) -> io::Result<u16>` | Apply the default or validate an explicit port; private to the config module and independent of the environment |
| `pub(crate) build_router() -> Router` | Register `GET /` for startup and route testing |
| `async index() -> Html<&'static str>` | Return embedded HTML; private to the web module |

Ctrl+C uses the operating system's normal process termination behavior. There is no custom shutdown handler.

## Commands and checks

Run these commands from the repository root:

| Command | Underlying command |
| --- | --- |
| `npm run dev` | `cargo run --locked -p murmeli-core` |
| `npm run build` | `cargo build --workspace --release --locked` |
| `npm run check` | `cargo check --workspace --all-targets --locked` |
| `npm run lint` | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| `npm run fmt` | `cargo fmt --all` |
| `npm run fmt:check` | `cargo fmt --all -- --check` |
| `npm test` | `cargo test --workspace --locked` |

`--locked` requires Cargo to use the committed dependency resolution without changing it. `npm run fmt` reformats the Rust source; `npm run fmt:check` checks formatting without editing it.

The focused tests provide confidence in two observable contracts:

- The route test invokes the router without opening a TCP socket and checks status `200`, the HTML content type, and the Hello world heading.
- The table-driven port test in `common/config.rs` checks the default, accepted values including both bounds, and invalid inputs. It does not change global environment variables.

Tests live beside the corresponding functions under `#[cfg(test)]`. Run them with `npm test`. To perform the same checks as CI:

```sh
npm run fmt:check
npm run lint
npm test
```

GitHub Actions runs these checks in one Ubuntu job on pushes and pull requests, with read-only repository permissions. It uses Node 24 and the Rust toolchain selected by `rust-toolchain.toml`; npm caching is disabled because there are no npm dependencies.
