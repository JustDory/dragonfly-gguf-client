# Dragonfly GGUF Client — Roadmap

This document tracks feature ideas, bug fixes, and promotional plans for the project.

---

## Recently Completed

- [x] **EC2 tracker server deployed** — t4g.nano `i-010775ec77305759a`, us-east-1, Elastic IP `35.169.49.134`, Ubuntu 24.04 ARM64. Cloudflare proxies `tracker.dragonfly-gguf.dev` → origin over HTTP (Flexible SSL).
- [x] **nginx landing page + reverse proxy** — Port 80 serves `/` (model registry), `/about` (original landing page), proxies `/peers|announce|leave` → `127.0.0.1:8080`. Config deployed via `deploy/ec2/setup-nginx.sh`.
- [x] **`/about` 404 fixed** — nginx config on the live server was missing the `location = /about` block; patched and reloaded 2026-06-21.
- [x] **Dynamic GGUF model registry** — `index.html` replaced the hardcoded 13-model bartowski list with a live two-phase HF Hub API fetch: top-50 text-generation GGUF models by downloads (`filter=gguf&pipeline_tag=text-generation&sort=downloads&direction=-1&limit=50`), then per-model `?blobs=true` for real file sizes and availability status. Deployed commit `997c97a`.

---

## Immediate (before / alongside public launch)

### Bug Fixes

- [ ] **Fix pre-existing stable-clippy drift** — `gc/mod.rs` ×3 (`sort_by` → `sort_by_key` etc.) and one in `dfget/main.rs:1048`. None are from the GGUF work but CI on `stable` is red until cleaned up.
- [ ] **`cargo fmt` pass across all crates** — Iroh + tracker crates landed without a fmt pass; formatting drift flags CI.
- [ ] **`--seed-only` daemon mode** — Pure-Iroh home users can't run `dfdaemon` without a manager/scheduler config (init `?`s out on `scheduler_announcer`). A `--seed-only` flag would let individuals seed without the full Dragonfly stack. Deferred but important for the individual-user story.
- [ ] **Range-request fallback** — If a proxy/mirror strips `Range` headers, GGUF header parsing silently fails. Add a graceful fallback that downloads the full file instead of erroring out.
- [ ] **HuggingFace SHA256 API error message** — If HF changes their metadata API or returns an unexpected response, the integrity check currently may fail silently or confusingly. Surface a clear, actionable error message rather than falling back to a potentially corrupt download.

### Infrastructure

- [x] **Deploy `dragonfly-tracker` binary to EC2** — Cross-compiled aarch64 musl binary via `cargo-zigbuild` in WSL, uploaded to GitHub Release `v0.1.0-tracker`, downloaded to `/usr/local/bin/dragonfly-tracker` on EC2. Running as systemd service `dragonfly-tracker.service`, enabled at boot. `tracker.dragonfly-gguf.dev/peers` returns `{"providers":[]}` — all P2P endpoints live (2026-06-21).
- [x] **EC2 disk** — Was 38% used (not critical; the 99.5% reading was a failed Rust install that rolled back cleanly).
- [ ] **Pre-built binaries in GitHub Releases** — Currently requires Linux + Rust 1.91+ toolchain to build. Publish a statically-linked `x86_64-unknown-linux-musl` release binary (and `dragonfly-tracker`) so users can try it without installing Rust.
- [ ] **Make the community tracker URL the default** — `https://tracker.dragonfly-gguf.dev` should be the hardcoded default in `dfget`. New users currently get no P2P unless they specify `--p2p-tracker`. Make P2P opt-out, not opt-in.
- [ ] **`notice.json` first broadcast** — Now that the repo is public and the update-notice channel is live, edit `notice.json` to announce the project launch / key features to any early adopters.

---

## Short-term (next 1–4 weeks)

### Features

- [ ] **Bandwidth savings counter** — After each download, print how much data came from P2P peers vs. HuggingFace origin (e.g., `387 MB via peers, 10 MB from origin — saved 97%`). Users will share these numbers; it's the clearest proof the project works.
- [ ] **Tracker status / health page** — A lightweight web UI (or even a static page that polls the tracker's existing `/peers` endpoint) showing live peer count, models currently being seeded, and cumulative bytes served. Makes the network feel real.
- [ ] **Model discovery endpoint** — Add `GET /models` to `dragonfly-tracker` returning a list of unique content keys being seeded (with peer counts). Foundation for the status page and future discoverability.
- [ ] **`docker compose` quickstart** — A single `docker-compose.yml` in `deploy/` that spins up tracker + seeded daemon + an example `dfget` download. Enables a one-command demo for anyone without Rust or WSL.
- [ ] **Paged model discovery / sorting / search** — Listed in README roadmap already. Tracker API: `GET /models?sort=peers&q=qwen&page=2`. Useful once the network has content.
- [ ] **Recursive repo download** — `gguf://owner/repo/` (trailing slash) downloads all `.gguf` files in the repo. Useful for model families with multiple quant levels.
- [ ] **Sharded / multi-part GGUF support** — Large models (70B+) are often split into multiple `.gguf` shards. Support `gguf://owner/repo/model-00001-of-00004.gguf` discovery and parallel shard download.
- [ ] **`dfctl gguf` subcommand** — Expose GGUF-specific commands: `dfctl gguf list` (show seeded models from local registry), `dfctl gguf status` (peer counts from tracker), `dfctl gguf remove <key>` (un-seed a model).

### Promotion

- [ ] **Seed a demo model** — Fine-tune or re-quantize a small model (Qwen2.5-0.5B or SmolLM2-360M → Q4_K_M, ~200–400 MB). Distribute it via `gguf://` as the primary download path. The model being *exclusive to the P2P URL on launch day* gives people a concrete reason to install the client.
- [ ] **r/LocalLLaMA post** — "I built P2P model distribution that cuts HuggingFace bandwidth — here's the benchmark." Include numbers (MB/s from P2P vs. direct HF), the demo model URL, and a one-liner install. This community cares deeply about download speed and bandwidth costs.
- [ ] **Hacker News Show HN** — "Show HN: P2P GGUF distribution with QUIC/NAT traversal, built on Iroh + Dragonfly (CNCF)." Technical depth, Rust, and CNCF credentials land well here.
- [ ] **Open a PR to `dragonflyoss/client` upstream** — Submit the GGUF backend as a feature PR to the official Dragonfly repo. Even if not merged immediately, it surfaces the project to the CNCF community and shows it's a serious contribution. Reference the fork in the PR description.
- [ ] **HuggingFace Hub model cards** — For every model you seed, add a note to its HF model card: "Download faster with P2P: `dfget gguf://...`" and link the project. Tag models with `gguf_p2p`.
- [ ] **Add GitHub topics** — Add `gguf`, `llm`, `p2p`, `peer-to-peer`, `huggingface`, `model-distribution`, `iroh`, `quic`, `dragonfly` to the repo for discoverability.

---

## Long-term (1–3 months)

### Features

- [ ] **Windows native support** — Currently Linux/WSL only. A native Windows build (or at minimum a well-documented WSL setup script) would dramatically expand the potential user base. Most AI hobbyists are on Windows.
- [ ] **LM Studio / llama.cpp transparent proxy shim** — A small local HTTP proxy that intercepts `huggingface.co` model download requests and routes them through `dfget`. Zero config change for end users of existing tools.
- [ ] **Ollama integration** — Either a PR to Ollama or a wrapper that makes `ollama pull` route through the P2P layer. Ollama has millions of users; even a prototype would generate significant attention.
- [ ] **GitHub Actions model-release workflow** — A reusable GHA action model creators can add to their repos: on release, automatically seed the new `.gguf` files to the tracker. Embeds the project in model creators' pipelines.
- [ ] **Persistent public seed nodes** — Run one or more always-on seed peers that pre-seed popular models (top-100 GGUF downloads on HF). This makes the network useful from day one for users who download popular models, before any organic peer density builds up.
- [ ] **Seed preheat via `dfctl`** — Wire `dfctl task preheat` to accept `gguf://` URLs and kick off Iroh seeding, complementing the existing Dragonfly preheat mechanism.
- [ ] **HF metadata wired into daemon download path** — Currently GGUF header metadata (architecture, quantization, etc.) is parsed but not surfaced through the daemon. Wire it into the task/piece metadata so it can be exposed via `dfctl`.
- [ ] **GUI / desktop app** — A minimal Tauri or Electron app for non-CLI users: paste a HuggingFace model URL, watch P2P download progress, manage seeded models. Lowers the barrier for the non-developer audience.
- [ ] **Bandwidth savings metrics dashboard** — Aggregate tracker-side: total bytes served via P2P vs. estimated origin bytes. Show a live counter on the tracker status page. Makes the project's impact legible.

### Promotion

- [ ] **Blog post / writeup** — A technical deep-dive on how the Iroh P2P layer works, the NAT traversal approach, and the GGUF header parsing. Cross-post to dev.to, Medium, and the Dragonfly blog if possible.
- [ ] **Dragonfly community engagement** — Post in the Dragonfly GitHub Discussions and any Slack/Discord channels about the fork. The maintainers may be interested in the GGUF backend and Iroh integration.
- [ ] **CNCF blog / case study** — Dragonfly is a CNCF incubating project. A use-case writeup submitted to the CNCF blog ("distributing AI models P2P with Dragonfly") would reach the cloud-native audience.

---

## Known Limitations / Caveats

- **No NAT traversal in Dragonfly mode** — The Dragonfly cluster path has no NAT traversal; peers must be able to dial each other's advertised IP directly. The Iroh path handles this via hole-punching and relay fallback.
- **Dragonfly mode requires manager + scheduler** — Not viable for individual home users without the full cluster. The `--seed-only` daemon mode (listed above) would close this gap.
- **Public tracker is a single point of failure** — Document how to self-host the tracker, and consider a hardcoded fallback list.
- **Repo was private during initial development** — The update-notice channel (`notice.json`) and any tracker-default URLs only work now that the repo is public (confirmed public 2026-06-18).
