# epoch — Tasks & Handoff

**Status:** Working notes / handoff (for review)
**As of:** 2026-08-26 (late)
**Scope:** Current state and what to pick up next. Architecture: `docs/ARCHITECTURE.md` (as-built); checkpoint/timeline design: `docs/TIMELINE.md`.

---

## 0. TL;DR

The single-machine filesystem is **feature-complete for daily use**: durable checkpointed CoW overlay, time travel, typed object browsing, configurable pass-through subtrees, live debug stats — mounted natively on macOS (TCC issue fixed) and resumable across remounts. `.vfs` is a plugin registry. What remains is the multi-agent daemon (both cuts), GC, and OpaqueTree.

## 1. Running it

```sh
./mount-mac.sh ~/some/project        # durable checkpointed overlay at ~/epoch (state: ~/.epoch)
EPOCH_PROBE=1 ./mount-mac.sh         # legacy read-only probe (no CAS/timeline; .vfs has only the cas stub)
```

Knobs: `EPOCH_HEAD` (timeline head, default `main`), `EPOCH_HARD_FSYNC=1`, `EPOCH_MOUNT`.

Inside the mount, `.vfs/` is the control plane (a registry of pluggable features):

| Path | What |
|---|---|
| `.vfs/history/<unix-millis>` | read-only snapshot at the newest checkpoint ≤ that time |
| `.vfs/history/<N><unit>-ago` | relative form (`500ms-ago`, `30s-ago`, `5m-ago`, `2h-ago`, `1d-ago`) |
| `.vfs/history/head` | the head checkpoint's **root tree hash** (empty before the first) |
| `.vfs/history/latest/…` | the head checkpoint's **merged** snapshot (delta over lower) |
| `.vfs/history/id/<event-hex>` | any event by id (dir itself unlistable) |
| `.vfs/cas/<hash>` | typed object portal: tree hash ⇒ browsable read-only dir, file hash ⇒ `cat`-able file, chunks hidden (dir unlistable) |
| `.vfs/debug/{near-objects,dirty-files,overlay}` | live stats: staging object count, un-sealed writers, overlay delta listing |

Explore by hand: `ls .vfs/cas/$(cat .vfs/history/head)`

A gitignore-syntax **`.epochignore`** in the lower directory routes matching
subtrees (`node_modules/`, `.git/`, `.jj/`, caches, …) straight to the host —
reads AND writes bypass the overlay and are never checkpointed.

## 2. This session's additions (all on the linear trunk, `main` at the tip)

`fuse-mount-fix → vfs-debug → vfs-plugins → route-ignore → cas-kind-tags → cas-object-portal → history-head → …` (see `jj log`; later additions below)

- **macOS mount fixed**: `local`/`nobrowse`/`noappledouble` mount options (TCC was silently EPERM-ing every open on the un-flagged volume); `VfsService::truncate` + fuser `setattr` (kernels deliver `O_TRUNC`/`truncate(2)` as `setattr(size)` — was ENOSYS).
- **`.vfs` as a feature registry**: every entry is a plain `VfsService` registered via `ControlPlaneVfs::with_feature(name, |slot| …)` owning a `1<<56`-sized id slice; routing is arithmetic. `SubMounts<K>` is the shared "mount a read-only sub-filesystem, remap its ids" helper used by both history (per `EventId`) and the cas portal (per `TreeId`).
- **Object-kind domain tags** (breaking: all hashes changed — wipe old `--state` dirs): canonical encodings start with a hashed kind byte, `'T'` tree / `'F'` file manifest / `'C'` chunk. Type confusion structurally impossible; portal type-sniffs one byte.
- **`.epochignore` routing**: writable `PassthroughVfs` + `RouterVfs` (ids in `[0x4000…, 0x8000…)`); routed traffic emits no checkpoint signals.
- **`.vfs/history/head`** (the head checkpoint's bare root hash) and **`.vfs/history/latest/`** (its merged snapshot); the cas portal is **overlay-aware** (tree hashes rehydrate over the lower).
- **One shared `TreeMounts` table**: history *resolves* names to root `TreeId`s served by the cas portal — one instance and one inode space per root, however it is reached. `ReadOnlyFeature` (blanket EROFS/no-op boilerplate) and `TreeBrowseFeature` (own-nodes-then-mounts combinators) keep the features slim; the `CasEchoDir` stub is gone.
- **`rename(2)`** end to end (editors' atomic saves work): POSIX replace semantics, fall-through files copy up and move, fall-through directories answer `EXDEV` (callers fall back to copy+delete), routed↔overlay moves answer `EXDEV` likewise; renames arm the checkpoint trigger.
- **xattr stubs** (`ENOTSUP` + empty list — no more `ENOSYS` noise from macOS) and **synthesized attrs stamped with the mounting user's uid/gid** (files no longer appear root-owned).
- Trigger fixes: metadata-only mutations (`touch`, bare `mkdir`) checkpoint; `mount-mac.sh` defaults to the durable overlay (`EPOCH_PROBE=1` for the legacy probe, whose `.vfs` is now bare).

## 3. What's next (ordered)

1. **Machine daemon (VfsService cut)** — one daemon owns worktrees, staging,
   cache, scheduler, timeline; each `epoch-fuse` mount becomes a
   FUSE ↔ `VfsService`-over-UDS shim. Enables N concurrent mounts sharing one
   state (redb is single-writer-per-process). Fork = daemon control verb.
   (TIMELINE.md §3.)
2. **Remote byte/timeline cut** — network `ByteBackend` + `TimelineStore`
   clients for a cloud far tier. Prereqs: batched `has_many`, compare-and-swap
   `set_head_if(old, new)`.
3. **`OpaqueTree` / compaction** — flatten overlay+lower at checkpoint, making
   history immune to lower drift (also fixes the `.vfs/history` /
   `.epochignore` fall-through caveats).
4. **GC by retention** (`epoch-gc`) — mark from heads (+ live volatile roots),
   sweep unreachable objects/events; cleans checkpoint crash orphans.
5. **VfsService gaps** — xattr *storage* (currently `ENOTSUP` stubs);
   seal-on-close in `release`; atime/mtime persistence (`setattr` accepts
   them but does not persist them — `make`-style mtime reasoning inside an
   overlay mount is a known gap). (`symlink`/`readlink`, `setattr` **mode**
   persistence, and `statfs` landed — see `docs/ops/CHANGELOG.md`.)

## 4. Deferred (agreed, not started)

- Evictable / persistent inodes (FUSE-`forget`; ids stable across remount) and
  sub-mount/minted-id cache eviction — same problem class.
- Spill staging tier for working sets larger than RAM.
- `.epochignore` live reload (rules load once at mount).
- Control plane (REST/gRPC) + `epoch-cli`; non-FUSE adapters; merge events.
- Housekeeping: spent `.worktrees/{vfs-debug,route-ignore}` workspaces can be
  `jj workspace forget` + removed.

## 5. Resuming

- Trunk is linear; new work stacks on `main`. Each feature above also has its
  own bookmark for review.
- `docs/TIMELINE.md` holds the checkpoint/timeline contracts and accepted
  trade-offs; `docs/ARCHITECTURE.md` the as-built crate map (note: its `.vfs`
  section predates the feature-registry refactor and the typed portal —
  refresh alongside the daemon work).
