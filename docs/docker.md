# Docker

The tool is distributed as the `ghcr.io/cecil-the-coder/autoanneal` Docker image.

> **Kubernetes users:** For scheduled, multi-repo deployments, see [kubernetes.md](kubernetes.md) instead of running Docker directly. The Helm chart manages CronJobs, secrets, and per-repo configuration.

## Image structure

Multi-stage build:

1. **Builder stage** (`rust:1.94-bookworm`) — Compiles the Rust binary with `cargo build --release`.
2. **Runtime stage** (`debian:bookworm-slim`) — Minimal Debian with only the necessary tools installed.

Final image size is approximately 2.4 GB due to pre-installed language toolchains (Rust, Go, Node.js, Python, build-essential). This ensures most projects can build without additional setup.

## What's in the image

| Component | Purpose |
|-----------|---------|
| `git` | Clone repos, manage branches, commit/push |
| `gh` | GitHub CLI for PR creation, repo metadata, auth |
| `claude` | Claude Code CLI (native binary via installer) |
| `autoanneal` | The Rust orchestrator binary (from builder stage) |
| `gcc`, `g++`, `make`, `pkg-config` | Build essentials for compiled languages |
| `rustc`, `cargo` | Rust toolchain (via rustup) |
| `node`, `npm` | Node.js 24.x (via NodeSource) |
| `python3`, `pip` | Python 3 with venv support |
| `go` | Go 1.26.1 |
| `ca-certificates`, `curl` | HTTPS support |

The image runs as a non-root `worker` user with `/work` as the working directory.

## Recommended docker run flags

```bash
docker run --rm \
  -e ANTHROPIC_API_KEY \
  -e GH_TOKEN \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  --memory=4g --cpus=2 \
  autoanneal owner/repo [OPTIONS]
```

Flag rationale:

| Flag | Why |
|------|-----|
| `--rm` | Container is ephemeral; clean up after exit |
| `--cap-drop=ALL` | Drop all Linux capabilities. The tool needs no special privileges. |
| `--security-opt=no-new-privileges` | Prevent privilege escalation inside the container |
| `--memory=4g` | Reasonable cap. Adjust up for large repos. |
| `--cpus=2` | Prevents runaway CPU usage. Increase if build commands are CPU-intensive. |

## Environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `ANTHROPIC_API_KEY` | Yes | Anthropic API key for Claude |
| `GH_TOKEN` | Yes | GitHub token with `repo` scope. `GITHUB_TOKEN` also accepted. Prefer a fine-grained PAT scoped to the target repo. |

Pass them with `-e VAR` (reads from host environment) or `-e VAR=value`.

Do not bake tokens into derived images.

## Dry run example

Run analysis without creating a PR:

```bash
docker run --rm \
  -e ANTHROPIC_API_KEY \
  -e GH_TOKEN \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  autoanneal owner/repo --dry-run --output json
```

## Pre-installed toolchains

The image ships with Rust, Go, Node.js, Python, and C/C++ build tools. For most projects, no additional setup is needed. Before the implementation phase, autoanneal runs a Claude-driven toolchain verification step that installs any missing project dependencies (e.g., `npm install`, `cargo fetch`).

If the target repo needs additional tools, use `--setup-command`:

```bash
docker run --rm \
  -e ANTHROPIC_API_KEY \
  -e GH_TOKEN \
  autoanneal owner/repo --setup-command "pip install -r requirements.txt"
```

For toolchains not in the image (e.g., Java, Ruby), extend it:

```dockerfile
FROM ghcr.io/cecil-the-coder/autoanneal:latest

USER root
RUN apt-get update && apt-get install -y --no-install-recommends \
    openjdk-21-jdk \
    && rm -rf /var/lib/apt/lists/*
USER worker
```

## Dockerfile reference

See the [Dockerfile](../Dockerfile) for the full source. Key layers:

1. **Builder**: `rust:1.94-bookworm` compiles the Rust binary
2. **System packages**: git, curl, build-essential, pkg-config, libssl-dev, python3
3. **gh CLI**: GitHub CLI from the official apt repo
4. **Node.js 24.x**: via NodeSource
5. **Go 1.26.1**: direct binary download
6. **Rust**: via rustup (installed as non-root `worker` user)
7. **Claude Code**: native installer to `~/.local/bin`
