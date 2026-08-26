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
# Two modes:
#
#   (default)      durable, checkpointed copy-on-write overlay: writes/deletes
#                  land in the CAS, are checkpointed on flush/close (debounced),
#                  persist in $EPOCH_STATE/epoch.redb (default ~/.epoch), and
#                  resume on remount. Time travel: ls <mount>/.vfs/history/
#   EPOCH_PROBE=1  legacy read-only overlay probe: reads through to the lower
#                  dir, EROFS on any write — no CAS, no checkpoints, and .vfs
#                  holds only the cas stub (no history/ or debug/).
set -euo pipefail
cd "$(dirname "$0")"

# ── config (override via positional args or env) ─────────────────────────────
# LOWER  pristine tree the overlay reads through / copies up from (defaults to
#        this repo, like docker-run.sh's `-v $PWD:/work:ro`).
# MOUNT  where it appears (~/epoch).
# EPOCH_STATE       state dir for the durable mode (default: ~/.epoch).
# EPOCH_HEAD        timeline head this mount advances (default: main).
# EPOCH_HARD_FSYNC  set to 1 to make fsync(2) a synchronous checkpoint barrier.
# EPOCH_PROBE       set to 1 for the legacy read-only probe (no state dir used).
LOWER="${EPOCH_LOWER:-${1:-$PWD}}"
MOUNT="${EPOCH_MOUNT:-${2:-$HOME/epoch}}"
STATE="${EPOCH_STATE:-$HOME/.epoch}"
HEAD="${EPOCH_HEAD:-main}"
[[ "${EPOCH_PROBE:-0}" == 1 ]] && STATE=""

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

# ── build ────────────────────────────────────────────────────────────────────
# The `fuser-adapter` feature selects fuser's `macos-no-mount` mode: fuser's own
# macOS mount does an in-process mount() that macOS 26 rejects with EPERM, so we
# mount through macFUSE's `mount_macfuse` helper ourselves (see
# crates/epoch-fuse/src/macfuse.rs) and drive the fd via Session::from_fd. No
# libfuse linkage, so no pkg-config / FUSE-T entanglement.
echo "Building epoch-fuse (release, macFUSE helper mount)…" >&2
cargo build --release -p epoch-fuse --features fuser-adapter
BIN="${PWD}/target/release/epoch-fuse"

# ── mount ────────────────────────────────────────────────────────────────────
is_mounted() { mount | grep -q " ${MOUNT} "; }
unmount() { umount "${MOUNT}" 2>/dev/null || diskutil unmount force "${MOUNT}" >/dev/null 2>&1 || true; }

if is_mounted; then echo "Clearing stale mount at ${MOUNT}…" >&2; unmount; fi
mkdir -p "${MOUNT}"

# Unmount on exit OR Ctrl-C — fuser installs no signal handler, so a bare SIGINT
# would otherwise leave the mount dangling.
trap unmount EXIT
trap 'exit 130' INT TERM

if [[ -n "${STATE}" ]]; then
  mkdir -p "${STATE}"
  cat <<EOF

── epoch FUSE mount (durable checkpointed overlay) ─────────────────
  lower (read-only)  ${LOWER}
  mountpoint         ${MOUNT}
  state              ${STATE}/epoch.redb   (head: ${HEAD})

Poke it from another terminal:
  echo hi > ${MOUNT}/note.txt          # copy-up into the overlay
  ls ${MOUNT}/.vfs/history/            # checkpoints, by unix-millis
  cat ${MOUNT}/.vfs/history/5s-ago/note.txt
Remounting with the same EPOCH_STATE resumes from the last checkpoint.

Ctrl-C here unmounts (running a final checkpoint) — or \`umount ${MOUNT}\`.
────────────────────────────────────────────────────────────────────
EOF
  ARGS=(cas "${MOUNT}" --state "${STATE}" --head "${HEAD}")
  [[ "${EPOCH_HARD_FSYNC:-0}" == 1 ]] && ARGS+=(--hard-fsync)
  "${BIN}" "${ARGS[@]}" "${LOWER}"
else
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
fi
