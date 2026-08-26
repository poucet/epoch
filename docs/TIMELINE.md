# Timeline, checkpointing, and time travel

**Status:** Design note (implemented; session of 2026-08-26)
**Supersedes:** parts of `TASKS.md` §2 (commit-object-in-CAS, redb-table ref
store) and `ARCHITECTURE.md` §8 (sqlite timeline). The D3 orthogonality rule
from `ARCHITECTURE.md` — *CAS family ⊥ timeline* — is reinstated.

---

## 1. What this layer is

On flush/fsync/close (debounced) or a dirty-bytes backstop, the working tree is
sealed and its root is made durable and time-addressable:

```
seal (under WorkingTree lock)
  → pending_closure(root)          post-order, pruned where far.has(id)
  → promote_many batches (drain)   one durable txn per batch
  → timeline.append(event)         event = {parents, root, timestamp_ms, meta}
  → timeline.set_head(head, id)
```

Reading history back: `/.vfs/history/<unix-millis>` (nearest checkpoint ≤ t on
this mount's lineage), `/.vfs/history/<N><unit>-ago` (relative, never
dentry-cached), `/.vfs/history/id/<event-hex>` (any event, unlistable dir).
All read-only (EROFS on every mutation path).

## 2. Decisions

### Timeline ⊥ CAS storage, behind a trait

The timeline is **not** stored in the CAS and shares no storage contract with
it. `epoch-timeline` defines the whole interface:

- `TimelineEvent { parents: Vec<EventId>, root: TreeId, timestamp_ms, meta }`
  — an immutable record; the DAG lives in the records, not the engine.
- `EventId` = blake3 of the canonical event encoding. Content-derived ids make
  events engine-independent, verifiable, and mergeable across stores. (This
  reuses the *hash utility*, not CAS storage.)
- `TimelineStore` — `append`/`get` (events), `set_head`/`get_head`/`list_heads`
  (the only mutable state), with `at(head, ts)`/`log(head, n)` as default
  first-parent chain walks an engine may override with an index.

Engines are pluggable: `epoch-timeline-redb` is the first
(`RedbTimelineStore`, two tables, shareable `Arc<Database>`); sqlite or a flat
log would slot in without touching callers. The v1 `at()` is O(chain); a
per-head time index is a documented engine-side optimization.

### Lineage and forking

Multiple agents forking from one base is the core scenario: a fork is nothing
more than `set_head("agent-b", existing_event)` — both heads then append
events whose `parents` link back through the shared base. Each mount owns one
head (`--head <name>`, default `main`); time queries resolve along that head's
first-parent chain, because a global timestamp→event map is ambiguous once
lineages fork. Timestamps are clamped strictly increasing per chain
(`max(now, parent+1)`) to survive clock steps.

### Ordering instead of atomicity

There is no cross-store transaction. The ordering rule gives crash safety:

1. the root's closure is durable (post-order batches: children before
   referrer, so the far tier is closed under reachability at every point);
2. then the event is appended;
3. then the head advances.

A crash in between leaves durable-but-unreachable objects — a temporary
superset of the storage invariant, harmless, self-healing (promotion prunes on
`far.has`), and eventually GC-by-retention's job. It can never leave a
reachable event or tree pointing at missing data.

### Staging drains on promotion

`promote_many(keys, drain=true)` deletes just-promoted (and already-durable)
keys from the memory staging tier after the durable batch commit. Without
this, staging grows monotonically. Safe: reads fall through to far, and
content addressing makes a racing identical re-put lossless. Un-promoted
staged data is never touched.

### Overlay checkpointing (the default deployment)

`epoch-fuse cas <mnt> --state <dir> [<lower-dir>]` — with a lower, the live
tree is a CoW overlay and every checkpoint records the **sparse delta**
(copied-up blobs/trees and tombstones — fully CAS-owned; lower content is
never promoted). Snapshots rehydrate a checkpoint's delta **over the current
lower** (`CasVfs::from_overlay`), and resume-after-remount does the same with
the head's delta.

Caveat, accepted deliberately: a historical view is exact for everything the
overlay ever touched, but untouched fall-through paths show the lower *as it
is now*. Making checkpoints fully self-contained against lower drift is what
the deferred `OpaqueTree` ingestion (CAS-to-CAS squash, `TASKS.md` §4) buys.

### Trigger

`CheckpointPolicy` (pure state machine) + `Scheduler` (thread + mpsc):
debounce 5ms, max-wait cap 500ms since first-dirty, dirty-bytes backstop
64 MiB. `flush`/soft-`fsync`/`release` are debounce signals; writes only feed
the backstop; signals with nothing dirty are ignored. `--hard-fsync` makes
`fsync(2)` a synchronous barrier (`SchedulerHandle::checkpoint_now`). A clean
unmount runs a final checkpoint (scheduler drop).

### Batched durability

`SyncByteBackend::put_many` (default = loop) lets `RedbByteBackend` land a
whole promotion batch in one write txn — one fsync per ~256 objects instead of
one per object. Objects and timeline share one `epoch.redb` (separate tables),
but nothing depends on that: the timeline could point at its own file/engine.

## 3. Multi-agent topology (designed, not built)

The end state is many mounts — one per agent — sharing state, locally and
across machines. Two process-boundary **cuts** are supported, at different
layers, so they compose:

```
fuse shim ──VfsService/UDS──▶ machine daemon ──ByteBackend+TimelineStore──▶ cloud core
(per mount)                   (worktrees, staging,        (network)        (far objects
                               moka, scheduler)                             + timeline)
```

- **VfsService cut** (local, multiple FUSE mounts on one machine): a daemon
  owns the tier, the timeline, and one `WorkingTree`/`History` per head; each
  `epoch-fuse` process is a thin FUSE ↔ `VfsService`-over-unix-socket shim.
  `VfsService` is already the universal internal boundary (object-safe, small,
  every decorator composes against it — the 9p/NFS-style cut). Buys shared
  staging (cross-agent dedup of un-checkpointed data), one cache, one
  scheduler, and `fork <head> from <event>` as a daemon control verb. This is
  also why N mounts can't just share one `--state` dir today: redb is
  single-writer-per-process.
- **Byte-layer cut** (remote, many machines against a cloud store): the far
  tier and timeline become network clients — `SyncByteBackend`/`AsyncByteBackend`
  impls (the async twin + `Blocking`/`Asyncify` bridges exist for exactly
  this) plus a remote `TimelineStore`. Content-hashed `EventId`s make timeline
  sync/merge across machines renumbering-free. The local redb slots in as a
  mid-tier: `Tiered<Mem, Caching<Moka, Tiered<Redb, Cloud>>>` — the tiered
  combinator was documented to compose this way from the start.
- **Single-process mode stays first-class**: it is simply the in-process
  composition `mount_durable` does today — neither cut applied.

Trait extensions the byte cut will want: batched `has_many` (per-key `far.has`
pruning is too chatty over a network) and compare-and-swap head advance
(`set_head_if(old, new)`) — with several machines advancing heads through one
timeline, plain `set_head` loses updates; locally the single scheduler per
head makes this a non-issue.

## 4. Deferred

- **`OpaqueTree` ingestion** — self-contained overlay checkpoints (above).
- **Per-head time index** in timeline engines (v1 `at()` walks the chain).
- **Snapshot/mapping eviction** — `.vfs/history` snapshot instances and
  synthetic inode maps are never dropped; same problem class as FUSE-`forget`
  inode eviction (`TASKS.md` §4).
- **GC-by-retention** — orphaned objects from crashed checkpoints and
  unreferenced history.
- **Merge events** — the event format carries `parents: Vec<EventId>` already;
  nothing writes a merge yet.
