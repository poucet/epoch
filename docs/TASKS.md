# epoch — Tasks & Handoff

**Status:** Working notes / handoff (for review)
**As of:** 2026-08-26
**Scope:** Where the storage + checkpointing + timeline work stands, and what to pick up next. Architecture: `docs/ARCHITECTURE.md` (as-built); checkpoint/timeline design: `docs/TIMELINE.md`.

---

## 0. TL;DR

The full vertical is **built and green**: CoW overlay runtime → tiered storage → checkpoint layer (closure walk, batched promotion, debounced trigger) → engine-agnostic timeline with lineage/forking → `.vfs/history` time travel → durable FUSE mount (`epoch-fuse cas <mnt> --state <dir> [<lower>]`, resumes from the head's last checkpoint). Overlay mounts checkpoint their sparse delta. What remains is the **multi-agent daemon** (both cuts), **GC**, and **OpaqueTree** ingestion.

## 1. Current trunk

Linear line ending at bookmark `main`:

`… → cas-typed-id → … → epoch-cas-redb` (previous handoff) `→ filecas-manifest → cas-put-many → tiered-promote-many → timeline-api → timeline-redb → history-closure → history-commit → vfs-fsync → history-vfs → checkpoint-trigger → mount-durable → overlay-checkpoint → macos-helper-mount → main (mount-mac.sh mode switch)`

All green (workspace tests + clippy) at `main`.

## 2. Running it

```sh
EPOCH_STATE=~/.epoch ./mount-mac.sh ~/some/project    # durable checkpointed overlay at ~/epoch
./mount-mac.sh                                        # old read-only probe (unchanged)
```

Knobs: `EPOCH_HEAD` (timeline head, default `main`), `EPOCH_HARD_FSYNC=1`
(synchronous fsync barrier), `EPOCH_MOUNT`. Time travel:
`ls <mount>/.vfs/history/`, `cat <mount>/.vfs/history/5s-ago/<file>`.
Remount with the same state dir resumes from the last checkpoint.

Known machine issue: after many mount attempts macFUSE can start denying all
access (EPERM) until reboot — unrelated to the code.

## 3. What's next (ordered)

1. **Machine daemon (VfsService cut)** — one daemon owns worktrees, staging,
   cache, scheduler, timeline; each `epoch-fuse` mount becomes a
   FUSE ↔ `VfsService`-over-UDS shim. Enables N concurrent mounts sharing one
   state (redb is single-writer-per-process). Fork = daemon control verb.
   Single-process mode remains the no-daemon composition. (TIMELINE.md §3.)
2. **Remote byte/timeline cut** — network `ByteBackend` + `TimelineStore`
   clients for a cloud far tier. Prereq trait extensions: batched `has_many`,
   compare-and-swap `set_head_if(old, new)`.
3. **`OpaqueTree` / compaction** — flatten overlay+lower into a standalone
   tree at checkpoint, making history immune to lower drift.
4. **GC by retention** (`epoch-gc`) — mark from heads (+ live volatile roots),
   sweep unreachable CAS objects and timeline events; also cleans checkpoint
   crash orphans.
5. **VfsService gaps** — `rename`, `symlink`, `setattr`; seal-on-close in
   `release` (needs the inode id in the release signature).

## 4. Deferred (agreed, not started)

- Evictable / persistent inodes (FUSE-`forget` refcounting; ids stable across
  remount) and `.vfs/history` snapshot-cache eviction — same problem class.
- Spill staging tier for working sets larger than RAM.
- `.gitignore` scratchpad routing (`VfsRouter`).
- Control plane (REST/gRPC) + `epoch-cli`; non-FUSE adapters.
- Merge events (the event format already carries multiple parents).

## 5. Resuming

- Trunk is linear; new work stacks on `main`.
- `docs/TIMELINE.md` holds the checkpoint/timeline contracts and the accepted
  trade-offs (ordering-not-atomicity, overlay lower-drift caveat, trigger
  defaults). `docs/ARCHITECTURE.md` is the as-built crate map and layering.
