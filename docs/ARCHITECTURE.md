# epoch — Architecture (as built)

**Status:** As-built reference, refreshed 2026-08-26
**Supersedes:** the original architecture draft (see git history for the full
21-crate plan) and `docs/INITIAL.md` (the founding system design, retained for
reference). Checkpointing, the timeline, and time travel are specified in
`docs/TIMELINE.md`; current status and next steps in `docs/TASKS.md`.

---

## 1. Overview

`epoch` is a time-traveling **copy-on-write filesystem** for autonomous AI
agents and developer workflows. It decouples **immutable file state**
(content-addressed storage: blobs, chunks, Merkle trees) from **temporal
lineage** (an append-only event DAG with named heads). Mounted over FUSE, it
overlays a pristine lower directory: reads fall through, writes and deletes
are copied up into the CAS, and the overlay state is checkpointed
automatically (debounced on flush/close) into durable storage — giving
consequence-free scrubbing, cheap forking of agent lineages, and a read-only
time-travel view at `.vfs/history/`, without ever touching the underlying
project.

## 2. Crate map (all built unless marked)

Layered bottom-up; every crate depends only on crates above it in this list.

| Crate | Role |
|---|---|
| `epoch-cas` | The generic, **id-agnostic** CAS kernel; depends on nothing internal. `SyncCAS<V>`/`AsyncCAS<V>` with a **typed `Id` parameter**, `SyncByteBackend`/`AsyncByteBackend` (opaque content-keyed KV, incl. batched `put_many`), the `SyncGenericCAS` engine (encode → BLAKE3 → zstd → backend, integrity-verified on read), `TieredByteBackend` (staging / promote-only), `CachingByteBackend` (read-through cache), `BatchingCAS`, `Blocking`/`Asyncify` bridges, `MemoryByteBackend`, `CASHash`, `CASError`. |
| `epoch-cas-moka` | `MokaByteBackend` — fixed-size, byte-weighted LRU cache backend (keeps `moka` out of the kernel). |
| `epoch-cas-redb` | `RedbByteBackend` — durable backend over embedded redb; single-txn `put_many`; `with_database(Arc<Database>)` so one `.redb` file can host sibling stores. |
| `epoch-file-cas` | `FileCas` — file content as chunked blobs: chunk store + `FileObject` manifest store (`Inline` ≤ 64 KiB, else 1 MiB chunks — the single swappable chunking seam); offset-aware `read_at`, `edit`/`commit` writer path, public `manifest()` for structural walks. `BlobId`. |
| `epoch-tree` | `Tree<L>` Merkle tree objects with the canonical versioned codec, `TreeId`, and the link split that makes corruption unrepresentable: `EntryId` (complete tree, no whiteout possible) vs `Overlay<EntryId>` (sparse layer with tombstones). `TreeStore<Bk, L>` is just the kernel engine specialized by a type alias. |
| `epoch-timeline` | The **engine-agnostic timeline contract**: `TimelineEvent {parents, root, timestamp_ms, meta}`, content-hashed `EventId`, the `TimelineStore` trait (events + named heads; chain-walk defaults for `at`/`log`), memory impl. Deliberately ⊥ CAS *storage* (see TIMELINE.md). |
| `epoch-timeline-redb` | `RedbTimelineStore` — first timeline engine (events + heads tables; shareable `Arc<Database>`). |
| `epoch-worktree` | `WorkingTree<TBk, FBk>` — the mutable inode layer: always-overlay CoW (absent lower = empty), lazy spine hydration (`DirChildren::Sealed`), copy-up, tombstones, truncate, and `seal() → TreeId` with structural-sharing short-circuits. |
| `epoch-history` | The checkpoint layer: `pending_closure` (post-order not-yet-durable walk, pruned where `far.has`), `History::commit_root` (batched promote + drain → append event → advance head), `CheckpointPolicy` (debounce/max-wait/dirty-backstop state machine) + `Scheduler` thread. |
| `epoch-vfs-api` | The one **synchronous, object-safe `VfsService` trait** (lookup/getattr/readdir/open/read/write/create/mkdir/unlink/rmdir/flush/fsync/release) + `InodeId`, `NodeAttr`, `VfsError::to_errno`-style mapping. Every layer above composes against it. |
| `epoch-vfs` | The `VfsService` implementations and decorators: `CasVfs` (WorkingTree behind a mutex), `PassthroughVfs` (read-only host dir), `ControlPlaneVfs` (`.vfs` synthetic subtree, id-space partitioned at `CP_BASE`), `HistoryDir` (`.vfs/history` time travel), `SignalVfs` (checkpoint trigger events), `CheckpointedFs` (seal-under-lock + commit-off-lock glue), `LoggingVfs`. |
| `epoch-fuse` | The fuser adapter (1:1 `InodeId` ↔ ino), the macOS `mount_macfuse` helper mount, and the `epoch-fuse` binary/CLI. |

**Planned, not built:** `epoch-gc` (mark/sweep retention), the machine daemon
and its `VfsService`-over-UDS shim, remote byte/timeline backends, control
plane (REST/gRPC), `epoch-cli`. See §6 and TIMELINE.md §3.

## 3. Storage layering

```
                      WorkingTree (inodes, CoW overlay, lazy hydrate)
                        │ TreeStore / FileCas (typed, shared backend clones)
                        ▼
      TieredByteBackend< MemoryByteBackend,              ← staging: writable, non-evictable,
                         CachingByteBackend<               promote-only; drained on checkpoint
                             MokaByteBackend,            ← read cache: safe eviction
                             RedbByteBackend > >         ← durable, survives restart
```

- **The storage invariant:** the durable (far) tier holds exactly the
  transitive closure of checkpointed roots — plus, transiently, orphans from a
  crashed checkpoint (harmless; future GC). Nothing else, ever: `put` lands in
  staging only, and `promote`/`promote_many` is the sole path to durable.
- Write cost is proportional to **checkpoint cadence, not mutation rate**:
  seal's structural sharing skips untouched subtrees, and the closure walk
  prunes wherever the durable tier already holds an id.
- Backends are `Arc`-backed clones, so the working tree, the checkpoint layer,
  and read-only snapshots all share one physical store with no accessor
  plumbing.

Decisions that held from the original design: id-agnostic kernel first
(D15), opaque semantics-free byte backends with the codec owned by the typed
layer (D4), `bytes::Bytes` everywhere (D5), one `thiserror` enum per crate
with an opaque backend escape hatch (D6), CAS ⊥ timeline (D3 — reinstated,
see TIMELINE.md). Revised: the caching/batching *CAS decorators* (old D8)
became **byte-backend combinators** (tiered/caching) below the typed layer;
the sqlite dedicated-thread timeline actor (old D7) became the redb engine
behind the `TimelineStore` trait; the async-first store surface (old D1/D2)
became **sync-first** — the async twins and bridges exist and become
load-bearing only at the remote cut (§6).

## 4. The VFS stack

One synchronous, object-safe trait — `VfsService` — is the universal internal
boundary; everything is either an implementation or a decorator of it. A
durable overlay mount composes:

```
ControlPlaneVfs            .vfs synthetic subtree; ids ≥ CP_BASE (0x8000…)
 ├─ .vfs/cas/<id>          hash-addressed read-only portal (readdir-blocked)
 ├─ .vfs/history/…         HistoryDir: time travel; ids ≥ HIST_BASE (0xC000…)
 │                           per-checkpoint read-only CasVfs snapshots, every
 │                           snapshot-local inode id remapped, EROFS on writes
 └─ SignalVfs              emits Wrote/Flush/Fsync/Release to the scheduler
     └─ CasVfs             Mutex<WorkingTree> over the tiered backend
         └─ PassthroughVfs read-only lower host directory (overlay mode)
```

- **Id-space partitioning** instead of capability traits: synthetic layers own
  high inode/handle ranges and delegate everything below them, so decorators
  stack without coordination. Read-only enforcement is done by the mediating
  decorator (every mutation on a synthetic or snapshot id answers `EROFS`),
  which replaced the old compile-time `VfsMetadata + VfsRead` split (old D12)
  with something that works for a single object-safe trait.
- The fuser adapter maps `InodeId` ↔ kernel ino 1:1 and stays a dumb
  translator. On macOS it mounts through the privileged `mount_macfuse`
  helper + `Session::from_fd` (fuser's in-process `mount()` is rejected by
  macOS 26); elsewhere fuser mounts natively.
- `.gitignore` scratchpad routing (`VfsRouter`, old §5.3) remains future work.

## 5. Checkpointing & time travel

Specified in `docs/TIMELINE.md`. In one paragraph: filesystem activity
debounces (5ms, capped at 500ms, 64 MiB dirty backstop) into `seal → promote
closure → append event → advance head`; events form a DAG with named heads
(forking = pointing a new head at an existing event); overlay mounts
checkpoint their **sparse delta** (fully CAS-owned, tombstones included) and
snapshots rehydrate deltas over the current lower; `.vfs/history/` resolves
unix-millis, relative (`5s-ago`), and exact event-id names to read-only
mounts of any checkpoint. Crash safety comes from write *ordering*, not
cross-store atomicity.

## 6. Multi-agent topology (designed, not built)

Two composable process-boundary cuts (details in TIMELINE.md §3):

```
fuse shim ──VfsService/UDS──▶ machine daemon ──ByteBackend+TimelineStore──▶ cloud core
```

- **VfsService cut** — many local mounts against one machine daemon that owns
  the worktrees, staging, cache, scheduler, and timeline.
- **Byte-layer cut** — many machines against a remote far tier + timeline
  (`AsyncByteBackend` + the `Blocking`/`Asyncify` bridges exist for this;
  needs batched `has_many` and compare-and-swap `set_head_if`).
- **Single-process mode stays first-class**: today's `epoch-fuse cas --state`
  binary is the no-cut composition.

## 7. Deferred

- `OpaqueTree` / compaction — CAS-to-CAS squash; makes overlay checkpoints
  self-contained against lower drift, and is the "provably complete tree"
  home.
- GC by retention (`epoch-gc`): mark from live heads (+ volatile roots) →
  sweep unreachable objects and events.
- Evictable / persistent inodes (FUSE-`forget` refcounting, ids stable across
  remount); ditto `.vfs/history` snapshot-cache eviction.
- Spill staging tier (working set > RAM), scratchpad `.gitignore` routing,
  `rename`/`symlink`/`setattr` in `VfsService`, drift detection of the lower,
  control plane + CLI, non-FUSE adapters.

## Appendix — Technology stack (as used)

| Domain | Library | Notes |
|---|---|---|
| Language | Rust (edition 2024) | Workspace of 13 crates. |
| FUSE | `fuser` 0.17 | macOS via `mount_macfuse` helper + `Session::from_fd`; macFUSE kext required (FUSE-T is wire-incompatible — cberner/fuser#273). |
| Storage | `redb` | One `epoch.redb` hosts object + timeline tables (shareable `Arc<Database>`). |
| Hashing | `blake3` | Content addressing for CAS objects *and* timeline events. |
| Compression | `zstd` | Applied by the CAS engine; ids hash the uncompressed canonical bytes. |
| Caching | `moka` | Byte-weighted LRU backend on the durable side of the promotion boundary. |
| Bytes / errors | `bytes`, `thiserror` | Refcounted buffers; typed error enums per crate. |
| Async (dormant) | `async-trait` | The `AsyncByteBackend`/`AsyncCAS` twins + bridges, load-bearing at the remote cut. |
