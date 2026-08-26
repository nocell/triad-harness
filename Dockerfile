# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.88

FROM rust:${RUST_VERSION}-bookworm AS builder
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && \
    install -Dm0755 target/release/triad /out/triad

FROM node:24-bookworm-slim AS runtime

ARG TARGETARCH
ARG CLAUDE_CODE_VERSION=2.1.246
ARG CODEX_CLI_VERSION=0.149.1
ARG KIMI_CODE_VERSION=0.38.0
ARG CURSOR_AGENT_BUILD=2026.08.11-e8db854
ARG CURSOR_AGENT_SHA256_AMD64=bfff4bf6f4e9dd30c1d0ef0a70b6077b074015dd2948e4c50685d53afdcfce5a
ARG CURSOR_AGENT_SHA256_ARM64=ea13f92e295f523a99ce8d8f57d6894d21e5d1e2d030ffad718ccd5955ca2eed
ARG VERSION=dev
ARG VCS_REF=unknown
ARG CREATED=unknown

LABEL org.opencontainers.image.title="Triad" \
      org.opencontainers.image.description="Subscription-backed frontier-model MapReduce code review harness" \
      org.opencontainers.image.source="https://github.com/nocell/triad-harness" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$VCS_REF" \
      org.opencontainers.image.created="$CREATED"

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get update && \
    apt-get install --yes --no-install-recommends \
      bash \
      build-essential \
      ca-certificates \
      curl \
      git \
      gzip \
      jq \
      libnss-wrapper \
      pkg-config \
      python3 \
      python3-pip \
      python3-venv \
      ripgrep \
      tar && \
    rm -rf /var/lib/apt/lists/* && \
    ln -s "$(find /usr/lib -name libnss_wrapper.so -print -quit)" /usr/local/lib/libnss_wrapper.so

RUN --mount=type=cache,target=/root/.npm \
    npm install --global --no-audit --no-fund \
      --allow-scripts=@anthropic-ai/claude-code,@moonshot-ai/kimi-code,node-pty \
      "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
      "@openai/codex@${CODEX_CLI_VERSION}" \
      "@moonshot-ai/kimi-code@${KIMI_CODE_VERSION}"

RUN case "$TARGETARCH" in \
      amd64) cursor_arch=x64; cursor_sha="$CURSOR_AGENT_SHA256_AMD64" ;; \
      arm64) cursor_arch=arm64; cursor_sha="$CURSOR_AGENT_SHA256_ARM64" ;; \
      *) echo "unsupported Docker architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac && \
    mkdir -p /opt/cursor-agent && \
    curl --fail --location --silent --show-error \
      "https://downloads.cursor.com/lab/${CURSOR_AGENT_BUILD}/linux/${cursor_arch}/agent-cli-package.tar.gz" \
      --output /tmp/cursor-agent.tar.gz && \
    echo "${cursor_sha}  /tmp/cursor-agent.tar.gz" | sha256sum --check --strict && \
    tar --extract --gzip --file /tmp/cursor-agent.tar.gz --directory /opt/cursor-agent --strip-components=1 && \
    test -x /opt/cursor-agent/cursor-agent && \
    ln -s /opt/cursor-agent/cursor-agent /usr/local/bin/cursor-agent && \
    ln -s /opt/cursor-agent/cursor-agent /usr/local/bin/agent && \
    rm /tmp/cursor-agent.tar.gz

COPY --from=builder /usr/local/cargo /usr/local/cargo
COPY --from=builder /usr/local/rustup /usr/local/rustup
COPY --from=builder /out/triad /usr/local/bin/triad
COPY docker/entrypoint.sh /usr/local/bin/triad-entrypoint

ENV HOME=/home/triad \
    USER=triad \
    CODEX_HOME=/home/triad/.codex \
    CLAUDE_CONFIG_DIR=/home/triad/.claude \
    KIMI_CODE_HOME=/home/triad/.kimi-code \
    KIMI_DISABLE_TELEMETRY=1 \
    TRIAD_CONFIG_HOME=/home/triad/.config/triad \
    TRIAD_DATA_HOME=/home/triad/.local/share/triad \
    CARGO_HOME=/home/triad/.cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN usermod --login triad --home /home/triad --move-home node && \
    groupmod --new-name triad node && \
    mkdir -p \
      /home/triad/.cache \
      /home/triad/.cargo \
      /home/triad/.claude \
      /home/triad/.codex \
      /home/triad/.cursor \
      /home/triad/.kimi-code \
      /home/triad/.config/triad \
      /home/triad/.local/share/triad \
      /workspace && \
    chown -R triad:triad /home/triad /workspace && \
    chmod 0755 /usr/local/bin/triad-entrypoint && \
    triad --version && \
    claude --version && \
    codex --version && \
    kimi --version && \
    cursor-agent --version && \
    rustc --version && \
    python3 --version && \
    node --version

USER triad:triad
WORKDIR /workspace
STOPSIGNAL SIGTERM

ENTRYPOINT ["/usr/local/bin/triad-entrypoint"]
CMD ["--help"]
