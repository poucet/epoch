# epoch — Tasks & Handoff

**Status:** Working notes / handoff (for review)
**As of:** 2026-07-13
**Scope:** Where the storage + checkpointing work stands, the decisions locked in, and what to pick up next.

---

## 0. TL;DR

The mutable runtime layer (CoW overlay, mountable via FUSE) is done and lazy. The **composable storage tiers** for durable, checkpoint-scoped persistence are now built and tested (staging / cache / durable). What remains is the **History/checkpoint layer** that ties them together, plus the **checkpoint trigger**. Everything is delivered as a **linear stack of jj bookmarks** off the common base, for you to rebase.

---

## 1. Current bookmark stack (newest → oldest)

Linear stack; rebase the whole thing with `jj rebase -s cas-typed-id -d <base>`.

| Bookmark | What it does |
|---|---|
| `epoch-cas-redb` | New crate: `RedbByteBackend`, the durable storage tier over embedded redb (`create(path)` / `in_memory()`). moka-free. |
| `epoch-cas-moka` | New crate: `MokaByteBackend`, a byte-weighted fixed-size LRU cache backend. Keeps moka out of the kernel. |
| `caching-byte-backend` | `CachingByteBackend<Cache, Inner>` — read-through cache, write-through, safe eviction (inner is source of truth). |
| `tiered-byte-backend` | `TieredByteBackend<Near, Far>` — staging: `get`/`has` fall through, `put` is near-only, `promote(key)` is the sole path to durable. |
| `stores-as-fields` | `WorkingTree` always-overlay (one code path), stores held as fields built once, split tree/file backends `WorkingTree<TBk, FBk = TBk>`. |
| `lazy-hydrate` | `DirChildren = Sealed(TreeId) | Open(map)`; spine faults in one level per access; `seal` short-circuits untouched subtrees. |
| `inode-id-rename` | `NodeId → InodeId` (also `ROOT_INODE_ID`, `inode_id`). |
| `cas-truncate` | `WorkingTree::truncate` / honor `O_TRUNC`. |
| `tree-typing` | `EntryId` vs `Overlay<EntryId>` — tombstone-in-complete-tree unrepresentable. |
| `cas-vfs` | `CasVfs` + `epoch-fuse cas <mnt> [<lower>]` mount. |
| `tree-store` | Folded `epoch-tree-cas` into a `TreeStore` alias over the typed engine. |
| `cas-typed-id` | Typed `Id` param on `SyncGenericCAS`. |

All green (workspace tests + clippy) at the tip.

---

## 2. Design decisions locked in

### Storage model (git-like, three layers)
- **Objects** — content-addressed, immutable: blobs + trees (the CAS). *Built.*
- **Checkpoint = a commit object** `{ root: TreeId, parents: [CheckpointId], timestamp, meta? }`, itself content-addressed, stored in the CAS. *Not built.*
- **Refs** — a small mutable `name → CheckpointId` map (HEAD, branches). *Not built.*

### The storage invariant (the whole point)
> The durable CAS holds **exactly the transitive closure of checkpointed roots** — nothing else, ever.

No garbage from intermediate mutations; GC only ever means retention. Write cost ∝ checkpoint cadence, not mutation rate.

### The tiers (built)
```
TieredByteBackend< MemoryByteBackend,               ← staging: writable, non-evictable, promote-only
                   CachingByteBackend< MokaByteBackend,   ← read-through LRU, safe eviction
                                       RedbByteBackend > >  ← durable, survives restart
```
- **Staging (`near`)** holds un-promoted writes — the *only* copy, so it must **never** evict. `put` lands here; nothing is durable until `promote`.
- **Cache** sits on the *durable* side of the promotion boundary, where eviction is safe (inner is source of truth). The moka LRU is one drop-in; the combinator is policy-agnostic.
- **`promote(key)`** is dumb/per-object (raw bytes). Deciding *which* keys form a checkpoint (the closure walk) is a higher layer's job.

### Checkpoint semantics
- **v1 staging is RAM-only** (no spill tier yet). Working-set bounded; crash before checkpoint loses un-checkpointed work — that *is* the intended semantics.
- **seal-on-close**: a dirty `FileWriter` becomes immutable staging-CAS objects when the file is closed (`release`), not only at checkpoint.
- **`checkpoint()` = seal → promote-closure → write commit object → advance HEAD.**
  - The **closure walk** starts at the checkpoint root and **prunes wherever `far.has(id)`** — an unchanged subtree is already durable (from a prior checkpoint), so it stops there. Naturally incremental; leans on the `lazy-hydrate` seal short-circuit.
  - It must decode structure: tree → child `TreeId`s + `BlobId`s, and a blob's `FileObject` → chunk ids. So it needs both `TreeStore` and `FileCas` — lives in the History layer (or `epoch-worktree`).
- **Concurrency**: hold the `WorkingTree` lock only for the in-memory seal; run `promote` + fsync off-lock on the captured immutable object set. New mutations during promotion belong to the next checkpoint.

### Trigger policy
- Separate **policy** (pure state machine: events + clock → `Idle | At(deadline) | Now`) from **scheduler** (background thread + timer + channel) from the **`checkpoint()` op**.
- Default: **debounced-flush** with a **`max_wait` cap** (avoid starvation under continuous flushing) + a **dirty-bytes backstop** (bound RAM when a workload never flushes).
- **fsync contract**: soft-by-default (flush/fsync are debounce signals; durability best-effort within `max_wait`) with an optional **hard-fsync** flag (synchronous checkpoint barrier). This deliberately relaxes strict `fsync` durability in favor of coalescing — like ext4 `commit=N`.

### WorkingTree shape (built)
- Always overlay: `lower = None` means an *empty* lower (no fall-through), byte-identical to a complete tree.
- `InodeId` is a POSIX inode / FUSE `ino`; owned by the overlay, stable across copy-up (body mutated in place). Session-scoped monotonic counter for now.

---

## 3. What's next (ordered)

1. **History / Repo layer** (the meat):
   - Commit object type `{ root: TreeId, parents, timestamp, meta }`, content-addressed → `CheckpointId`.
   - A small durable **ref store** (HEAD; branches later) — a redb table or a file.
   - The structure-aware **closure walk** driving `promote` (needs `TreeStore` + `FileCas`).
   - API: `checkpoint(meta) -> CheckpointId`, `head()`, `at(time) -> TreeId`, `log()`.
2. **Wire `WorkingTree`/`CasVfs` onto the tiered stack** — `Bk = Tiered<Memory, Caching<Moka, Redb>>` for both tree and file backends (split backends already support this); implement **seal-on-close** in `CasVfs::release`.
3. **Checkpoint trigger** — policy state machine (debounced-flush + max_wait + backstop) + scheduler thread; wire `CasVfs::flush`/`fsync` (currently no-ops) to send events; `fsync`-hard flag.
4. **Design note** — consolidate §2 into a reviewable `docs/` design doc (still owed; not yet written).

---

## 4. Deferred (agreed, not started)

- **Layer 3 — evictable / persistent inodes.** `WorkingTree.inodes` only grows; bounded-memory eviction needs FUSE-`forget` refcounting **and** a derivable/persistent `InodeId` (not the session counter) — same requirement as inode-stability across remount. Hardlinks: broken on copy-up (overlayfs default) for now.
- **Spill staging tier** — for when the working set exceeds RAM: spill un-promoted writes to an *ephemeral* scratch store (not permanent), promote its reachable closure at checkpoint. Big blobs: spill to a content-addressed scratch file, promote by move/link (avoid double-write).
- **Compaction / `OpaqueTree`** — CAS-to-CAS squash (flatten overlay+lower into a standalone tree). Where the "provably complete, no fall-through" guarantee lives. Also what checkpointing an overlay-over-an-*external*-filesystem would need (checkpointing is CAS-native only until then).
- **Housekeeping** — spent `.worktrees/` from earlier sessions can be `jj workspace forget` + removed.

---

## 5. Resuming

- The stack is linear jj bookmarks off the common base; rebase with `jj rebase -s cas-typed-id -d <target>`.
- Backend crates are cleanly independent: `epoch-cas` (kernel, no external backend deps), `epoch-cas-moka` (moka), `epoch-cas-redb` (redb). Compose them as the tier stack above.
- Start with the **History layer** (§3.1) — the tiers it needs are all built and tested.
