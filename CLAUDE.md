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

A standalone crate (`src/{node,downloader,seeder,scanner,tracker}.rs`) built on `iroh` + `iroh-blobs`.
Public API in `lib.rs`: `content_key()`, `try_p2p_download()`, `register_seed()`, `run_seed_service()`,
`discover_seeds()`, `run_scan_service()`, `ScanConfig`, `DiscoveredSeed`, `TrackerClient`,
`SeedManifest`.

- **Content key** = `sha256_hex("hf://owner/repo/path:revision[:base_url]")` — stable and identical
  across peers for the same source object. Path is **not** lowercased (HF blobs are case-sensitive);
  `base_url` is folded in so mirrors don't collide. See `content_key()` tests for the invariants.
- **`dfget` download path** (`dragonfly-client/src/bin/dfget/main.rs`): for a `gguf://` **or `hf://`**
  URL with P2P enabled it computes the content key, queries the tracker, and downloads over Iroh
  (`try_p2p_download`) — so every Hugging Face file format shares the P2P path, not just GGUF.
  On any successful download it calls `register_p2p_seed()` to write a **seed manifest** into the
  registry dir — `dfget` is short-lived and cannot hold an Iroh endpoint open.
- **Seeding** (`dragonfly-client/src/bin/dfdaemon/main.rs`): a long-running `dfdaemon` runs
  `run_seed_service()` over the registry dir (`$XDG_DATA_HOME/dragonfly/gguf-seeds`, override
  `DRAGONFLY_GGUF_SEED_REGISTRY`), hosting one persistent Iroh endpoint that serves every unexpired
  manifest and announces it to the tracker. On by default; disable with `--no-gguf-seed` or
  `DRAGONFLY_GGUF_SEED=0`. Manifests are JSON on disk, so seeding survives daemon restarts.

Unlike the upstream crates (which forbid `anyhow` in library code and use `DFError`), this crate uses
`anyhow::Result` throughout — follow the existing style within each crate.

### Local engine scanning — `dragonfly-client-p2p/src/scanner.rs`

Most users already have gigabytes of models on disk that they downloaded with *other* tools. The scan
service (`run_scan_service()`, hosted by `dfdaemon` alongside the seed service) walks the well-known
model directories of popular local inference engines, reconstructs each pre-downloaded model's Hugging
Face content key, and writes a **seed manifest** per key into the same registry dir — so the seed
service picks them up with no extra plumbing. On by default whenever seeding is enabled; disable with
`--no-gguf-scan` / `DRAGONFLY_GGUF_SCAN=0`. Add extra roots with `--gguf-scan-dir` (repeatable).

A seed only helps if its key equals what a downloader computes, so a file is only ever registered once
its **exact** HF coordinates are known. The engines fall into two mapping strategies:

- **Offline, exact** (`discover_seeds()`, network-free) — the on-disk layout preserves or records the
  HF coordinate:
  - **HF hub cache** `~/.cache/huggingface/hub/models--<org>--<repo>/snapshots/<commit>/<path>` (env
    `HF_HUB_CACHE` / `HF_HOME` / `LLAMA_CACHE`) — shared by transformers, vLLM, TGI,
    sentence-transformers, `huggingface-cli` and modern `llama.cpp -hf`. `refs/*` map each snapshot to
    the revisions it answers (`main` **and** the commit hash).
  - **LM Studio** `~/.lmstudio/models/<owner>/<repo>/<file>`.
  - **Legacy llama.cpp cache** `~/.cache/llama.cpp/…` — recovered from the `.json` url sidecar.
  - **Jan** `~/jan/models/**/model.json` — recovered from `sources[].url`.
- **Network, hash-verified** (`resolver`, `--no-gguf-scan-resolve` to disable) — for content-addressed
  or loose files with no HF coordinate in the path. It looks the model up **by name** (forward: never a
  reverse hash search — HF has no such endpoint), then confirms by comparing the repo file's LFS
  `sha256` (`GET /api/models/<repo>/tree/main?expand=true` → `lfs.oid`) to the local file. Seeds only on
  an exact match:
  - **Ollama** `~/.ollama/models` (env `OLLAMA_MODELS`): for `hf.co/…` pulls the manifest names the
    repo (a direct lookup); the blob's `sha256-<hex>` name is its hash for free. Native
    `registry.ollama.ai` models are Ollama's own requantizations that don't exist on the Hub, so they
    are skipped.
  - **Loose files** (GPT4All, oobabooga, `--gguf-scan-dir`): filename → HF search → hash confirm.

Only `.gguf` + `.safetensors` above `DEFAULT_MIN_SIZE` are considered. **ModelScope** support (its
exact on-disk cache + enabling the `ms://` P2P path) is a planned follow-up — the resolver is HF-only
today, which is where Ollama pulls and virtually all GGUF models live.

### Discovery tracker — `dragonfly-tracker/`

Lightweight HTTP peer-discovery service (`src/{routes,store}.rs`). Endpoints: `POST /announce`,
`GET /peers?content_key=<hex>`, `DELETE /leave`, and `GET /contents?format=&q=&limit=` (lists every
content key with live providers, most-seeded first — powers registry search/categories). `/announce`
accepts optional `filename`/`format`/`size` metadata: content keys are one-way hashes, so this is the
only way the tracker can know what a key is. The seed service announces it automatically (derived
from the manifest's file path); all fields are optional in both directions, so old clients and old
trackers interoperate. In-memory store with per-peer TTL eviction (metadata is dropped when a key's
last provider expires) and per-IP announce rate limiting. Default community instance:
`https://tracker.dragonfly-gguf.dev` (`DEFAULT_TRACKER_URL` in the p2p crate).

### Download fallback order

1. **Iroh P2P** — when `--p2p-tracker` is set (default) and peers are found.
2. **Dragonfly scheduler** — when `--no-p2p`/`--prefer-dragonfly`, or no Iroh peers.
3. **Hugging Face direct** — upstream fallback when the scheduler is unreachable.

After a `gguf://` or `hf://` download, `dfget` verifies the file against the source `sha256` from the
Hugging Face LFS `X-Linked-Etag` (best-effort: skipped if none advertised — e.g. small non-LFS config
files — fails only on a confirmed mismatch), deleting the file on mismatch.

## Deploy

`deploy/tracker/` has a `docker compose` stack (tracker + Dragonfly seed daemon + manager/scheduler/
Redis/MySQL). Tracker image builds from `deploy/tracker/Dockerfile`; the fork's client image builds
from `ci/Dockerfile`. Both use the **repo root** as build context.
