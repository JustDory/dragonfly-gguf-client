# Dragonfly Client

[![GitHub release](https://img.shields.io/github/release/dragonflyoss/client.svg)](https://github.com/dragonflyoss/client/releases)
[![CI](https://github.com/dragonflyoss/client/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/dragonflyoss/client/actions/workflows/ci.yml)
[![Coverage](https://codecov.io/gh/dragonflyoss/client/branch/main/graph/badge.svg)](https://codecov.io/gh/dragonflyoss/dfdaemon)
[![Open Source Helpers](https://www.codetriage.com/dragonflyoss/client/badges/users.svg)](https://www.codetriage.com/dragonflyoss/client)
[![Discussions](https://img.shields.io/badge/discussions-on%20github-blue?style=flat-square)](https://github.com/dragonflyoss/dragonfly/discussions)
[![Twitter](https://img.shields.io/twitter/url?style=social&url=https%3A%2F%2Ftwitter.com%2Fdragonfly_oss)](https://twitter.com/dragonfly_oss)
[![LICENSE](https://img.shields.io/github/license/dragonflyoss/dragonfly.svg?style=flat-square)](https://github.com/dragonflyoss/dragonfly/blob/main/LICENSE)
[![FOSSA Status](https://app.fossa.com/api/projects/git%2Bgithub.com%2Fdragonflyoss%2Fclient.svg?type=shield)](https://app.fossa.com/projects/git%2Bgithub.com%2Fdragonflyoss%2Fclient?ref=badge_shield)

Dragonfly client written in Rust. It can serve as both a peer and a seed peer.

This fork adds a native `gguf://` backend for downloading GGUF models from Hugging Face
through Dragonfly's P2P distribution system.

## Installation

### Prerequisites

- A Linux environment (native Linux or WSL2). The workspace depends on Linux-only crates
  (unix sockets, `fuse`), so it does **not** build on native Windows.
- [Rust](https://rustup.rs/) (stable toolchain).
- Build dependencies. On Debian/Ubuntu:

  ```shell
  sudo apt-get update
  sudo apt-get install -y git curl build-essential pkg-config \
      libssl-dev libclang-dev protobuf-compiler
  ```

  > **No `sudo`?** You can install user-local equivalents without root: a prebuilt
  > `protoc` into `~/.local/bin` (set `PROTOC`), the `libclang` Python wheel
  > (`pip install --user libclang`, set `LIBCLANG_PATH`), and point bindgen at your
  > GCC headers via `BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/<ver>/include"`.

### Build from source

```shell
# Replace <your-username> with your GitHub fork.
git clone https://github.com/<your-username>/client.git dragonfly-gguf-client
cd dragonfly-gguf-client
git checkout feature/gguf-backend

# Install the Rust toolchain if you don't have it.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# Build the client binaries.
cargo build --release --bin dfget --bin dfdaemon

# Binaries are produced in target/release/ .
./target/release/dfget --help
```

To run the backend unit tests:

```shell
cargo test -p dragonfly-client-backend gguf
```

## Usage

### Download a GGUF model from Hugging Face

Use the `gguf://` scheme to download a `.gguf` model file. Only `.gguf` files are
accepted. The file is P2P-distributed like any other Dragonfly download, internally
resolving via the Hugging Face backend, so the `--hf-token`, `--hf-revision`, and
`--hf-base-url` options apply.

```shell
dfget gguf://owner/repo/model.gguf -O ./model.gguf
```

`dfget` forwards the request to a running `dfdaemon`, which downloads the model and
distributes it over the P2P network. A standalone `dfget` therefore needs a `dfdaemon`
(plus a Dragonfly scheduler and manager) to talk to — see below.

## Testing peer-to-peer locally

A full P2P run needs a Dragonfly **manager**, **scheduler**, and at least one **peer**.
The easiest way to stand these up is the official compose stack in the
[dragonflyoss/dragonfly](https://github.com/dragonflyoss/dragonfly/tree/main/deploy/docker-compose)
repo, with this fork's client image substituted for the peers.

1. **Build this fork's client image:**

   ```shell
   docker build -f ci/Dockerfile -t dragonfly-gguf-client:latest .
   ```

2. **Get the deploy compose** and point the `client` / `seed-client` services at your image:

   ```shell
   git clone https://github.com/dragonflyoss/dragonfly.git
   cd dragonfly/deploy/docker-compose
   # In docker-compose.yaml, set the image of the `client` and `seed-client`
   # services to: dragonfly-gguf-client:latest
   ```

3. **Two gotchas to be aware of** (learned the hard way):
   - The manager/scheduler/dfdaemon validate `advertiseIP` and `host.ip` as **real IP
     addresses** — service-name hostnames are rejected. Assign each container a static IP
     on a custom bridge network (e.g. `172.30.0.0/24`) and use those IPs for the advertise
     fields. Connection strings (mysql/redis/manager `addr`) may use service names.
   - The peers come up `Restarting` until the manager and scheduler are healthy; this is
     expected and self-heals once the control plane is ready.

4. **Bring it up and download:**

   ```shell
   ./run.sh                       # or: docker compose up -d
   docker exec client dfget \
     gguf://bartowski/Qwen2-0.5B-Instruct-GGUF/Qwen2-0.5B-Instruct-Q4_K_M.gguf \
     -O /tmp/model.gguf
   ```

   The daemon log line `load [gguf] builtin backend` confirms the backend is registered,
   and `finished piece ... from parent ...-seed using protocol tcp` confirms pieces were
   served peer-to-peer.

## Documentation

You can find the full documentation on the [d7y.io](https://d7y.io).

## Community

Join the conversation and help the community grow. Here are the ways to get involved:

- **Slack Channel**: [#dragonfly](https://cloud-native.slack.com/messages/dragonfly/) on [CNCF Slack](https://slack.cncf.io/)
- **Github Discussions**: [Dragonfly Discussion Forum](https://github.com/dragonflyoss/dragonfly/discussions)
- **Developer Group**: <dragonfly-developers@googlegroups.com>
- **Mailing Lists**:
  - **Developers**: <dragonfly-developers@googlegroups.com>
  - **Maintainers**: <dragonfly-maintainers@googlegroups.com>
- **Twitter**: [@dragonfly_oss](https://twitter.com/dragonfly_oss)
- **DingTalk Group**: `22880028764`

## Contributing

You should check out our
[CONTRIBUTING](./CONTRIBUTING.md) and develop the project together.
