FROM rust:1.85.1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release --bin jail-guest

FROM node:22-bookworm-slim
ARG CLAUDE_VERSION=2.1.284
ARG CODEX_VERSION=0.159.2
ARG JAIL_UID=501
ARG JAIL_GID=20
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git iptables procps ripgrep tini util-linux \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL https://claude.ai/install.sh -o /tmp/claude-install.sh \
    && bash /tmp/claude-install.sh "$CLAUDE_VERSION" \
    && cp -L /root/.local/bin/claude /usr/local/bin/claude \
    && rm /tmp/claude-install.sh \
    && npm install --global "@openai/codex@$CODEX_VERSION" \
    && npm cache clean --force
RUN printf '%s\n' "$JAIL_UID" > /etc/jail-uid \
    && printf '%s\n' "$JAIL_GID" > /etc/jail-gid \
    && mkdir -p /etc/jail /etc/claude-code /etc/codex /home/jail \
    && (getent group "$JAIL_GID" || groupadd -g "$JAIL_GID" jail) \
    && useradd -o -u "$JAIL_UID" -g "$JAIL_GID" -d /home/jail -s /bin/bash jail
COPY --from=builder /build/target/release/jail-guest /usr/local/bin/jail-guest
ENV HOME=/home/jail TERM=xterm-256color
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/jail-guest", "init"]
