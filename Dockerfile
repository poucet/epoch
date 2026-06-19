# syntax=docker/dockerfile:1
# Minimal Linux dev box for the epoch FUSE spike.
# Linux ships an in-kernel FUSE driver (/dev/fuse), so fuser's pure-Rust
# backend mounts with no macFUSE, no kernel extension, and no approvals.
FROM rust:slim

# rustc needs a C linker; curl/ca-certificates fetch zellij; mount/umount ship
# with debian-slim (util-linux).
RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc libc6-dev curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Zellij — a Rust terminal multiplexer. Grab the prebuilt musl binary matching
# this image's architecture (works on Apple Silicon arm64 and amd64 alike).
RUN arch="$(uname -m)" \
 && curl -fsSL "https://github.com/zellij-org/zellij/releases/latest/download/zellij-${arch}-unknown-linux-musl.tar.gz" \
    | tar -xz -C /usr/local/bin zellij \
 && chmod +x /usr/local/bin/zellij

# Build the spike once and expose it as `epoch` on PATH. The cache mounts keep
# rebuilds fast when you re-run the helper script after editing code.
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p epoch-fuse --features fuser-mount \
 && cp target/release/epoch-fuse /usr/local/bin/epoch

# Split-screen layout: interactive shell on the left, the live epoch mount on
# the right.
COPY <<'KDL' /root/.config/zellij/layouts/epoch.kdl
layout {
    pane split_direction="vertical" {
        pane name="shell" focus=true
        pane name="epoch" command="epoch" {
            args "/work" "/mnt/epoch"
        }
    }
}
KDL

RUN mkdir -p /mnt/epoch
ENV TERM=xterm-256color
WORKDIR /work
CMD ["zellij", "--layout", "/root/.config/zellij/layouts/epoch.kdl"]
