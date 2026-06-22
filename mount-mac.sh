#!/usr/bin/env bash
# Mount the epoch FUSE overlay natively on macOS at ~/epoch via macFUSE.
#
# Counterpart to ./docker-run.sh, which runs the same binary inside Linux's
# in-kernel FUSE for quick parity testing — that mount stays inside the VM and
# macOS can't see it. This builds the binary for macOS and mounts it into your
# home dir so Finder, editors, LSPs and compilers see a real path.
#
# Requires macFUSE with its kernel extension ENABLED. On Apple Silicon that is a
# one-time approval (Recovery → Reduced Security + "allow kernel extensions" →
# reboot → Privacy & Security → Allow → reboot). See docs/MOUNTING.md.
# (FUSE-T, the kext-less alternative, does not work here: fuser 0.17 hard-codes
# the macFUSE wire protocol and can't talk to FUSE-T — cberner/fuser#273.)
#
# The overlay (OverlayProbeVfs) is strictly read-only: it reads through to the
# lower dir and returns EROFS on any write, so the lower dir is never touched.
set -euo pipefail
cd "$(dirname "$0")"

# ── config (override via positional args or env) ─────────────────────────────
# LOWER  pristine tree the overlay reads through (defaults to this repo, like
#        docker-run.sh's `-v $PWD:/work:ro`). MOUNT  where it appears (~/epoch).
LOWER="${EPOCH_LOWER:-${1:-$PWD}}"
MOUNT="${EPOCH_MOUNT:-${2:-$HOME/epoch}}"

# ── preflight ────────────────────────────────────────────────────────────────
if [[ ! -d /Library/Filesystems/macfuse.fs ]]; then
  cat >&2 <<'MSG'
macFUSE is not installed:
  brew install --cask macfuse
Then enable its kernel extension (Apple Silicon) — see docs/MOUNTING.md.
MSG
  exit 1
fi
LOWER="$(cd "${LOWER}" && pwd)"   # canonicalize; fail loudly if it doesn't exist

# ── build (links macFUSE via pkg-config `fuse`) ──────────────────────────────
echo "Building epoch-fuse (release, macFUSE)…" >&2
cargo build --release -p epoch-fuse --features fuser-mount
BIN="${PWD}/target/release/epoch-fuse"
# Guard against a stale FUSE-T link from an earlier experiment: force macFUSE.
if otool -L "${BIN}" 2>/dev/null | grep -q 'libfuse-t'; then
  echo "Relinking fuser against macFUSE…" >&2
  cargo clean -p fuser >/dev/null 2>&1 || true
  cargo build --release -p epoch-fuse --features fuser-mount
fi

# ── mount ────────────────────────────────────────────────────────────────────
is_mounted() { mount | grep -q " ${MOUNT} "; }
unmount() { umount "${MOUNT}" 2>/dev/null || diskutil unmount force "${MOUNT}" >/dev/null 2>&1 || true; }

if is_mounted; then echo "Clearing stale mount at ${MOUNT}…" >&2; unmount; fi
mkdir -p "${MOUNT}"

# Unmount on exit OR Ctrl-C — fuser installs no signal handler, so a bare SIGINT
# would otherwise leave the mount dangling.
trap unmount EXIT
trap 'exit 130' INT TERM

cat <<EOF

── epoch FUSE mount (macOS / macFUSE) ──────────────────────────────
  lower (read-only)  ${LOWER}
  mountpoint         ${MOUNT}

Poke it from another terminal:
  ls -la ${MOUNT}
  cat ${MOUNT}/Cargo.toml

Read-only overlay — writes return EROFS. Ctrl-C here unmounts and exits
(or run \`umount ${MOUNT}\` from another terminal).

If you get "Operation not permitted", macFUSE's kernel extension is not
enabled yet — see docs/MOUNTING.md.
────────────────────────────────────────────────────────────────────
EOF

"${BIN}" "${LOWER}" "${MOUNT}"
