# Linux/FUSE benchmark image: git-lazy-mount + sgrep (from the local checkout)
# + the claude CLI, running as a non-root user so claude can run headlessly with
# a scoped allow-list. Build from the repo root:
#   docker build -t glm-bench -f benchmarks/Dockerfile .
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      git curl ca-certificates build-essential pkg-config libfuse3-dev fuse3 python3 jq \
 && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /opt/glm
COPY . /opt/glm
RUN cargo build --release -p glm-cli --features fuse -p sgrep \
 && for b in git-lazy-mount git-lazy-mount-fsmonitor sgrep; do install -m0755 "target/release/$b" /usr/local/bin/ 2>/dev/null || true; done \
 && cargo clean
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && npm install -g @anthropic-ai/claude-code \
 && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /work /out && chown -R ubuntu:ubuntu /work /out && echo 'user_allow_other' >> /etc/fuse.conf
USER ubuntu
ENV HOME=/home/ubuntu
WORKDIR /work
