#!/usr/bin/env bash
# bench/verify-bare-overlay.sh — verify that a bare Epoch-FS overlayed over nothing
# supports full coding agent workflows: jj git clone from GitHub, cargo build,
# binary execution, file modifications, and jj commit semantics.
#
# Can be run directly on Linux (with /dev/fuse) or inside Docker.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." 2>/dev/null && pwd || echo "/src")"
EPOCH_BIN="${EPOCH_BIN:-$(which epoch 2>/dev/null || which epoch-fuse 2>/dev/null || echo "$REPO_ROOT/target/release/epoch-fuse")}"

if [[ ! -x "$EPOCH_BIN" ]]; then
  echo "ERROR: epoch binary not found or not executable at '$EPOCH_BIN'" >&2
  exit 1
fi

REPO_URL="${1:-https://github.com/${GITHUB_REPOSITORY:-poucet/epoch}.git}"

echo "========================================================================"
echo "==> Bare Epoch-FS Verification"
echo "==> Binary:      $EPOCH_BIN"
echo "==> Target Repo: $REPO_URL"
echo "========================================================================"

# ── Scratch & mount paths ───────────────────────────────────────────────────
SCRATCH="$(mktemp -d /tmp/epoch-verify-bare.XXXXXX)"
LOWER="$SCRATCH/empty-lower"
MOUNT="$SCRATCH/mnt"
STATE="$SCRATCH/state"
mkdir -p "$LOWER" "$MOUNT" "$STATE"

DAEMON_PID=""
cleanup() {
  local exit_code=$?
  set +e
  echo "==> Cleaning up mount and scratch directories..."
  if [[ -n "$DAEMON_PID" ]]; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  fusermount3 -u "$MOUNT" 2>/dev/null || fusermount -u "$MOUNT" 2>/dev/null || umount "$MOUNT" 2>/dev/null || true
  rm -rf "$SCRATCH" 2>/dev/null || true
  if [[ $exit_code -eq 0 ]]; then
    echo "==> Bare Epoch-FS verification PASSED."
  else
    echo "==> Bare Epoch-FS verification FAILED (exit $exit_code)."
  fi
  exit "$exit_code"
}
trap cleanup EXIT INT TERM

# ── Mount bare Epoch-FS overlayed over nothing (empty lower) ────────────────
echo "▸ Mounting bare Epoch-FS over empty lower..."
"$EPOCH_BIN" cas "$MOUNT" --state "$STATE" --head ci-bare "$LOWER" > "$SCRATCH/daemon.log" 2>&1 &
DAEMON_PID=$!

# Readiness poll
READY=0
for _ in $(seq 1 150); do
  if [[ -d "$MOUNT/.vfs" ]]; then
    READY=1
    break
  fi
  if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "ERROR: epoch daemon crashed during startup. Daemon log:" >&2
    cat "$SCRATCH/daemon.log" >&2
    exit 1
  fi
  sleep 0.1
done

if [[ $READY -ne 1 ]]; then
  echo "ERROR: mount at $MOUNT timed out waiting for readiness. Daemon log:" >&2
  cat "$SCRATCH/daemon.log" >&2
  exit 1
fi
echo "▸ Bare Epoch-FS mount ready at $MOUNT."

# ── Hermetic Git / JJ configuration ──────────────────────────────────────────
export GIT_AUTHOR_NAME="Epoch CI"
export GIT_AUTHOR_EMAIL="ci@epoch.dev"
export GIT_COMMITTER_NAME="Epoch CI"
export GIT_COMMITTER_EMAIL="ci@epoch.dev"
export JJ_USER="Epoch CI"
export JJ_EMAIL="ci@epoch.dev"

git config --global user.name "Epoch CI"
git config --global user.email "ci@epoch.dev"
git config --global init.defaultBranch main

if [[ -n "${GITHUB_TOKEN:-}" ]]; then
  echo "▸ Configuring GitHub token credential for clone..."
  git config --global url."https://x-access-token:${GITHUB_TOKEN}@github.com/".insteadOf "https://github.com/"
fi

# ── JJ Git Clone from GitHub inside bare mount ──────────────────────────────
cd "$MOUNT"
echo "▸ Running 'jj git clone $REPO_URL epoch-clone' inside bare Epoch-FS..."
jj git clone "$REPO_URL" epoch-clone

cd epoch-clone
echo "▸ Cloned successfully! Inspecting jj status & log..."
jj status
jj log -n 5

# ── Cargo Build inside cloned repo ──────────────────────────────────────────
echo "▸ Running 'cargo build' inside bare Epoch-FS..."
cargo build

echo "▸ Executing compiled binary to verify mmap, linking, and execve semantics..."
./target/debug/epoch

# ── Verify coding agent file mutation & commit semantics ─────────────────────
echo "▸ Testing file mutation (append/save) and jj commit..."
echo "// Verified by bare Epoch-FS CI" >> src/main.rs
jj status
jj commit -m "verify coding agent semantics in bare epoch-fs"
jj log -n 2

# ── Control Plane Telemetry ──────────────────────────────────────────────────
echo "▸ Verifying control plane debug stats (.vfs/debug)..."
cat "$MOUNT/.vfs/debug"
echo ""

echo "==> All bare Epoch-FS assertions satisfied!"
