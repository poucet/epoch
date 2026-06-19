#!/usr/bin/env bash
# Build the dev image and open a shell where you can drive the epoch FUSE spike.
# Linux's in-kernel FUSE means no macFUSE, no kext, no approvals on your Mac —
# the mount happens inside the container's Linux kernel.
set -euo pipefail
cd "$(dirname "$0")"

IMAGE=epoch-fuse
docker build -t "$IMAGE" .

cat <<'EOF'

── epoch FUSE dev container (Zellij split, Linux /dev/fuse) ──────────
Zellij opens with two panes:
  • left  — an interactive zsh shell (cwd /work)
  • right — `epoch /work /mnt/epoch` running live (the FUSE mount + logs)

In the left pane, poke the mount:
  ls -la /mnt/epoch
  cat /mnt/epoch/Cargo.toml

Zellij keys: Alt+←/→ move between panes · Ctrl-q quits (unmounts + exits).
/work is your repo, mounted read-only; edit on the host, re-run this
script to rebuild `epoch`. Want a plain shell instead?  ./docker-run.sh zsh
─────────────────────────────────────────────────────────────────────
EOF

exec docker run --rm -it \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  --security-opt apparmor=unconfined \
  -v "$PWD:/work:ro" \
  "$IMAGE" "$@"
