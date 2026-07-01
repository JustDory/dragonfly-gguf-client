# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repository is

This is a **fork of [`dragonflyoss/client`](https://github.com/dragonflyoss/client)** that adds native
`gguf://` model distribution for pulling GGUF models from Hugging Face over peer-to-peer networks. It
layers two things on top of the upstream Dragonfly P2P client:

1. A **`gguf://` backend** — a thin wrapper over the Hugging Face backend.
2. An **Iroh P2P path** — NAT-transparent QUIC-based sharing with a lightweight HTTP discovery tracker,
   so downloads work between arbitrary machines without a Dragonfly control plane.

Everything else (the `dfdaemon`/`dfget`/`dfcache`/`dfstore`/`dfctl` binaries, piece-based P2P, RocksDB
storage, scheduler/manager gRPC) is inherited from upstream Dragonfly.

`.github/copilot-instructions.md` documents the **upstream** conventions in depth (error handling, the
`DFError`/`OrErr` model, tracing, testing layout, the Apache license header required on every `.rs`
file). Read it for anything not covered here. Note it is partly stale relative to this fork — it lists
9 crates (this workspace has 12), version `1.2.11` (now `1.3.12`), and toolchain `1.88.0`.

## Build, test, lint

The workspace only builds on **Linux** (native or WSL2) — it depends on Linux-only crates (unix
sockets, fuse). `.cargo/config.toml` sets `rustflags = ["--cfg", "tokio_unstable"]`, so all cargo
commands compile with that cfg. Protobuf compilation requires `protoc` on `PATH`.

```bash
# Build everything (dfget, dfdaemon, dfcache, dfstore, dfctl, dragonfly-tracker)
cargo build --release
cargo build --release --bin dfget          # just the client

cargo check --all --all-targets            # what CI's `check` job runs

# Tests — CI runs the whole workspace under coverage:
cargo llvm-cov --all-features --workspace --lcov --output-path lcov.info
cargo test -p dragonfly-client-backend gguf         # gguf backend unit tests
cargo test -p dragonfly-client-p2p                  # P2P layer unit tests
cargo test -p dragonfly-client-p2p --test registry_e2e   # in-process tracker + seed + Iroh download
cargo test -p dragonfly-client-storage test_lru_cache    # single test by name

./test_integration.sh                       # black-box tracker HTTP test (announce/peers/leave/rate-limit)
```

**Lint** — CI's `lint.yml` pins clippy/fmt to the **1.88.0** toolchain even though `rust-toolchain.toml`
is `stable`. To reproduce CI lint exactly, run against 1.88.0; warnings are hard errors:

```bash
cargo fmt --all -- --check
cargo clippy --all --all-targets -- -D warnings
```

## Fork-specific architecture

### The `gguf://` backend — `dragonfly-client-backend/src/gguf.rs`

`Gguf` implements the `Backend` trait by delegating to an inner `HuggingFace`. On each request it (a)
validates the `.gguf` extension, (b) rewrites `gguf://owner/repo/x.gguf` → `hf://owner/repo/x.gguf`,
and (c) forwards `stat`/`get`/`put`/`exists`. Registered as a builtin scheme in
`dragonfly-client-backend/src/lib.rs` (look for `"load [gguf] builtin backend"`). The daemon derives
the task id from the **original `gguf://` URL before rewriting**, so all peers requesting the same URL
share one task and its pieces. This module also parses GGUF header metadata (architecture, name,
quantization, tensor/KV counts) via a 1 MiB range request that avoids downloading the whole file.

### Iroh P2P layer — `dragonfly-client-p2p/`

A standalone crate (`src/{node,downloader,seeder,tracker}.rs`) built on `iroh` + `iroh-blobs`. Public
API in `lib.rs`: `content_key()`, `try_p2p_download()`, `register_seed()`, `run_seed_service()`,
`TrackerClient`, `SeedManifest`.

- **Content key** = `sha256_hex("hf://owner/repo/path:revision[:base_url]")` — stable and identical
  across peers for the same source object. Path is **not** lowercased (HF blobs are case-sensitive);
  `base_url` is folded in so mirrors don't collide. See `content_key()` tests for the invariants.
- **`dfget` download path** (`dragonfly-client/src/bin/dfget/main.rs`): for a `gguf://` URL with P2P
  enabled it computes the content key, queries the tracker, and downloads over Iroh
  (`try_p2p_download`). On any successful download it calls `register_gguf_seed()` to write a
  **seed manifest** into the registry dir — `dfget` is short-lived and cannot hold an Iroh endpoint open.
- **Seeding** (`dragonfly-client/src/bin/dfdaemon/main.rs`): a long-running `dfdaemon` runs
  `run_seed_service()` over the registry dir (`$XDG_DATA_HOME/dragonfly/gguf-seeds`, override
  `DRAGONFLY_GGUF_SEED_REGISTRY`), hosting one persistent Iroh endpoint that serves every unexpired
  manifest and announces it to the tracker. On by default; disable with `--no-gguf-seed` or
  `DRAGONFLY_GGUF_SEED=0`. Manifests are JSON on disk, so seeding survives daemon restarts.

Unlike the upstream crates (which forbid `anyhow` in library code and use `DFError`), this crate uses
`anyhow::Result` throughout — follow the existing style within each crate.

### Discovery tracker — `dragonfly-tracker/`

Lightweight HTTP peer-discovery service (`src/{routes,store}.rs`). Endpoints: `POST /announce`,
`GET /peers?content_key=<hex>`, `DELETE /leave`. In-memory store with per-peer TTL eviction and
per-IP announce rate limiting. Default community instance: `https://tracker.dragonfly-gguf.dev`
(`DEFAULT_TRACKER_URL` in the p2p crate).

### Download fallback order

1. **Iroh P2P** — when `--p2p-tracker` is set (default) and peers are found.
2. **Dragonfly scheduler** — when `--no-p2p`/`--prefer-dragonfly`, or no Iroh peers.
3. **Hugging Face direct** — upstream fallback when the scheduler is unreachable.

After a `gguf://` download, `dfget` verifies the file against the source `sha256` from the Hugging Face
LFS `X-Linked-Etag` (best-effort: skipped if none advertised, fails only on a confirmed mismatch),
deleting the file on mismatch.

## Deploy

`deploy/tracker/` has a `docker compose` stack (tracker + Dragonfly seed daemon + manager/scheduler/
Redis/MySQL). Tracker image builds from `deploy/tracker/Dockerfile`; the fork's client image builds
from `ci/Dockerfile`. Both use the **repo root** as build context.
