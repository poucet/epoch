#!/usr/bin/env bash
# bench/run-mount.sh — run the epoch-bench suite against a REAL macFUSE mount.
#
# The suite's default mode measures an in-process CasVfs overlay (no kernel
# round-trip). This script mounts a scratch lower with the epoch-fuse binary,
# points the suite at the mount and the daemon pid, and tears down — so the
# numbers include the FUSE path, which is what EPOCH-FIXES §2.2's ceilings
# are about.
#
# Two phases, because `.epochignore` rules load once at mount time:
#   A) mount WITHOUT an ignore file → full suite incl. cargo-build-overlay
#   B) fresh mount WITH `/target/` ignored → cargo-build-ignore only
#      (demonstrates the §2.3 "build outputs belong in ignored subtrees"
#      contract numerically)
#
# Usage:
#   bench/run-mount.sh [--quick] [--groups "micro macro ..."] [--keep]
#
# Ceilings failing today is EXPECTED (the suite exists to regress against);
# the script exits 0 unless epoch-bench itself crashed.

set -uo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

QUICK=""
SUITES="micro macro spinup mmap realworld"
KEEP=0
MARKDOWN_ARG=""
NO_FAIL_ARG=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --quick) QUICK="--quick"; shift ;;
    --groups) SUITES="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --markdown) MARKDOWN_ARG="--markdown $2"; shift 2 ;;
    --no-fail-exit) NO_FAIL_ARG="--no-fail-exit"; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

SCRATCH="${EPOCH_BENCH_SCRATCH:-$(mktemp -d "${TMPDIR:-/tmp}/epoch-bench-mount.XXXXXX")}"
LOWER="$SCRATCH/lower"
MNT="$SCRATCH/mnt"
BARE_A="$SCRATCH/bare-a"
BARE_B="$SCRATCH/bare-b"
STATE_A="$SCRATCH/state-a"
STATE_B="$SCRATCH/state-b"

# ── preflight & OS detection ──────────────────────────────────────────────────
OS="$(uname -s)"
if [[ "$OS" == "Darwin" ]]; then
  if [[ ! -d /Library/Filesystems/macfuse.fs ]]; then
    echo "macFUSE not installed — use \`cargo run -p epoch-bench -- ...\` (in-process mode) or ./docker-run.sh" >&2
    exit 1
  fi
  FUSE_FEATURE="fuser-adapter"
  unmount_fs() {
    umount "$1" 2>/dev/null || diskutil unmount force "$1" >/dev/null 2>&1 || true
  }
elif [[ "$OS" == "Linux" ]]; then
  if [[ ! -e /dev/fuse ]]; then
    echo "/dev/fuse not found — ensure FUSE is enabled" >&2
    exit 1
  fi
  FUSE_FEATURE="fuser-mount"
  unmount_fs() {
    fusermount -u "$1" 2>/dev/null || fusermount3 -u "$1" 2>/dev/null || umount "$1" 2>/dev/null || true
  }
else
  echo "Unsupported OS for kernel mount benchmark: $OS" >&2
  exit 1
fi

echo "▸ building epoch-bench + epoch-fuse ($FUSE_FEATURE, release)…" >&2
cargo build --release -p epoch-bench || exit 1
cargo build --release -p epoch-fuse --features "$FUSE_FEATURE" || exit 1
BENCH="$REPO_ROOT/target/release/epoch-bench"
FUSE="$REPO_ROOT/target/release/epoch-fuse"

# ── scratch ──────────────────────────────────────────────────────────────────
if [[ -d "$SCRATCH" ]]; then
  unmount_fs "$MNT"
  rm -rf "$SCRATCH"
fi
mkdir -p "$LOWER" "$MNT" "$BARE_A" "$BARE_B" "$STATE_A" "$STATE_B"

DAEMON_PID=""
cleanup() {
  if [[ -n "$DAEMON_PID" ]]; then
    kill "$DAEMON_PID" 2>/dev/null
    wait "$DAEMON_PID" 2>/dev/null
  fi
  unmount_fs "$MNT"
  [[ "$KEEP" == 0 ]] && rm -rf "$SCRATCH"
}
trap cleanup EXIT

mount_epoch() {  # mount_epoch <state-dir>
  "$FUSE" cas "$MNT" --state "$1" --head bench "$LOWER" \
      >"$SCRATCH/daemon.log" 2>&1 &
  DAEMON_PID=$!
  # Wait for the mount to answer (readiness poll — not a measured sleep).
  for _ in $(seq 1 300); do
    [[ -d "$MNT/.vfs" ]] && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || { echo "epoch-fuse died; log:" >&2; cat "$SCRATCH/daemon.log" >&2; return 1; }
    sleep 0.1
  done
  echo "mount at $MNT never came up" >&2
  return 1
}

unmount_epoch() {
  unmount_fs "$MNT"
  wait "$DAEMON_PID" 2>/dev/null
  DAEMON_PID=""
}

# ── phase A: no .epochignore — full suite ────────────────────────────────────
echo "▸ phase A: mount (no .epochignore) — $SUITES, cargo overlay mode" >&2
mount_epoch "$STATE_A" || exit 1
"$BENCH" $SUITES $QUICK $NO_FAIL_ARG $MARKDOWN_ARG \
  --bare "$BARE_A" \
  --mount "$MNT" --lower "$LOWER" --daemon-pid "$DAEMON_PID" \
  --fuse-bin "$FUSE" \
  --scratch "$SCRATCH/suite" \
  --cargo-mode overlay
A_CODE=$?
unmount_epoch

# ── phase B: /target/ routed via .epochignore — cargo ignore variant ────────
if [[ " $SUITES " == *" realworld "* ]]; then
  echo "▸ phase B: fresh mount with /target/ in .epochignore — cargo-build-ignore" >&2
  printf '/target/\n' > "$LOWER/.epochignore"
  mount_epoch "$STATE_B" || exit 1
  "$BENCH" realworld $QUICK $NO_FAIL_ARG $MARKDOWN_ARG \
    --bare "$BARE_B" \
    --mount "$MNT" --lower "$LOWER" --daemon-pid "$DAEMON_PID" \
    --fuse-bin "$FUSE" \
    --scratch "$SCRATCH/suite-b" \
    --cargo-mode ignore
  B_CODE=$?
  unmount_epoch
else
  B_CODE=0
fi

echo "" >&2
if [[ $A_CODE -gt 2 || $B_CODE -gt 2 ]]; then
  echo "▸ epoch-bench crashed (exit $A_CODE/$B_CODE)" >&2
  exit 1
fi
echo "▸ done (ceiling FAILs are measurements, not script errors; exit $A_CODE/$B_CODE)" >&2
exit 0
