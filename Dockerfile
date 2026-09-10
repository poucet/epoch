# syntax=docker/dockerfile:1
# Minimal Linux dev box for the epoch FUSE spike.
# Linux ships an in-kernel FUSE driver (/dev/fuse), so fuser's pure-Rust
# backend mounts with no macFUSE, no kernel extension, and no approvals.
FROM rust:slim

# rustc needs a C linker; curl/ca-certificates fetch tools; zsh is the
# interactive shell; fuse3/libfuse3-dev for FUSE; git for VCS operations.
RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc libc6-dev curl ca-certificates git fuse3 libfuse3-dev zsh \
 && rm -rf /var/lib/apt/lists/*

# Zellij — a Rust terminal multiplexer. Grab the prebuilt musl binary matching
# this image's architecture (works on Apple Silicon arm64 and amd64 alike).
RUN arch="$(uname -m)" \
 && curl -fsSL "https://github.com/zellij-org/zellij/releases/latest/download/zellij-${arch}-unknown-linux-musl.tar.gz" \
    | tar -xz -C /usr/local/bin zellij \
 && chmod +x /usr/local/bin/zellij

# Jujutsu (jj) VCS for agent workflows
RUN arch="$(uname -m)" \
 && JJ_VER="v0.28.0" \
 && curl -fsSL "https://github.com/jj-vcs/jj/releases/download/${JJ_VER}/jj-${JJ_VER}-${arch}-unknown-linux-musl.tar.gz" \
    | tar -xz -C /usr/local/bin ./jj \
 && chmod +x /usr/local/bin/jj

# Build the spike once and expose it as `epoch` on PATH. The cache mounts keep
# rebuilds fast when you re-run the helper script after editing code.
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p epoch-fuse --features fuser-mount \
 && cp target/release/epoch-fuse /usr/local/bin/epoch

# zsh as the interactive shell; a tiny rc keeps zsh from running its first-run
# new-user wizard inside the pane.
ENV SHELL=/usr/bin/zsh
COPY <<'ZSHRC' /root/.zshrc
autoload -Uz compinit && compinit -u
PROMPT='%F{cyan}epoch%f:%~ %# '
ZSHRC

# Split-screen layout: interactive shell on the left, the live epoch CoW
# mount on the right. The mount is the durable overlay (`cas` verb) with the
# store on the /state host bind — writes are captured and checkpointed;
# the lower (/work, read-only) is never modified. Re-running docker-run.sh
# with the same head resumes from the last checkpoint.
COPY <<'KDL' /root/.config/zellij/layouts/epoch.kdl
layout {
    pane split_direction="vertical" {
        pane name="shell" focus=true {
            cwd "/mnt/epoch"
        }
        pane name="epoch" command="epoch" {
            args "cas" "/mnt/epoch" "/work" "--state" "/state" "--head" "dev"
        }
    }
}
KDL

RUN mkdir -p /mnt/epoch
ENV TERM=xterm-256color
WORKDIR /mnt/epoch
CMD ["zellij", "--layout", "/root/.config/zellij/layouts/epoch.kdl"]
