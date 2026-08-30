#!/usr/bin/env bash
# Build the dev image and open a shell where you can drive the epoch FUSE spike.
# Linux's in-kernel FUSE means no macFUSE, no kext, no approvals on your Mac —
# the mount happens inside the container's Linux kernel.
#
# The mount is the DURABLE copy-on-write overlay (`epoch cas`), not the
# read-only probe: writes inside the container are captured, checkpointed,
# and attributed to a named head, with the store on a host bind so it
# survives `--rm`. Reads fall through to /work; the real repo is never
# modified. Re-running with the same head resumes from the last checkpoint.
set -euo pipefail
cd "$(dirname "$0")"

IMAGE=epoch-fuse
STATE=${EPOCH_LANE_STATE:-$PWD/.epoch-state/dev}
docker build -t "$IMAGE" .
mkdir -p "$STATE" "$PWD/.epoch-mnt"

cat <<EOF

── epoch CoW dev container (Zellij split, Linux /dev/fuse) ──────────
Zellij opens with two panes:
  • left  — an interactive zsh shell (cwd /mnt/epoch, the live mount)
  • right — epoch running the durable overlay (mount + logs)

In the left pane, work on the mount:
  ls -la /mnt/epoch
  echo scratch > /mnt/epoch/scratch.txt             # captured, checkpointed
  cat /mnt/epoch/.vfs/history/30s-ago/scratch.txt   # ...before it existed

Everything you write lands in the overlay store (host bind at
$STATE — survives --rm; same head "dev" across runs resumes from the last
checkpoint). /work is the lower, bind-mounted read-only; never modified.

Zellij keys: Alt+←/→ move between panes · Ctrl-q quits (unmounts + exits).
Want a plain shell instead?  ./docker-run.sh zsh
────────────────────────────────────────────────────────────────────
EOF

exec docker run --rm -it \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  --security-opt apparmor=unconfined \
  -v "$PWD:/work:ro" \
  -v "$STATE:/state" \
  -v "$PWD/.epoch-mnt:/mnt/epoch" \
  "$IMAGE" "$@"
