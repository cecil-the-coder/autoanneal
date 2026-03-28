FROM rust:1.94-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release

FROM debian:bookworm-slim

# System packages + build essentials
RUN apt-get update && apt-get install -y --no-install-recommends \
    git ca-certificates curl \
    build-essential pkg-config libssl-dev \
    python3 python3-pip python3-venv \
    && rm -rf /var/lib/apt/lists/*

# gh CLI
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y gh \
    && rm -rf /var/lib/apt/lists/*

# Node.js (via NodeSource)
RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# Go
RUN ARCH=$(dpkg --print-architecture) \
    && curl -fsSL "https://go.dev/dl/go1.26.1.linux-${ARCH}.tar.gz" \
      | tar -C /usr/local -xzf - \
    && ln -s /usr/local/go/bin/go /usr/local/bin/go \
    && ln -s /usr/local/go/bin/gofmt /usr/local/bin/gofmt

# Non-root user (create before installing user-scoped tools)
RUN useradd -m -s /bin/bash worker
USER worker

# Rust (via rustup, as worker)
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

ENV PATH="/home/worker/.cargo/bin:${PATH}"

WORKDIR /work

COPY --from=builder /build/target/release/autoanneal /usr/local/bin/autoanneal

ENTRYPOINT ["autoanneal"]
