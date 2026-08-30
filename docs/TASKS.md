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

Store-side (no mount needed; reads `<state-dir>/epoch.redb` directly — unmount
or copy the state dir first, redb is single-writer): `epoch diff|export|audit
<state-dir> [--head <name>]` — the landing verbs and audit log. See
`docs/reference/EXPORT-AUDIT.md`.

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
  - **macOS wire-decode bug, fixed 2026-08-30:** on macFUSE 4.x/5.x every rename arrived with empty name buffers (100% failure — `mv`, editor saves, `jj git init` all broken). Root cause was not the overlay (its semantics were already correct) but fuser parsing the 8-byte Linux `fuse_rename_in` where macFUSE sends 16 bytes; fixed by enabling fuser's `macfuse-4-compat` feature on macOS only (`epoch-fuse/Cargo.toml`). See `docs/MOUNTING.md`.
  - **Follow-on found by the acceptance test, same fix chain:** `rmdir` of an emptied fall-through directory returned ENOTEMPTY because `WorkingTree` counted the lower `readdir`'s synthesized `.`/`..` entries as children (and `readdir` cached them as real nodes). Dot entries are now ignored at both merge points; regression test in `epoch-worktree/tests/integration.rs`.
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
   seal-on-close in `release`. (`symlink`/`readlink`, `setattr` **mode**
   persistence, `statfs`, and atime/mtime semantics landed — see
   `docs/ops/CHANGELOG.md`.)

### 3.6 VfsService semantics survey (per EPOCH-FIXES §1.6)

Full operation surface, what landed, and what is deliberately refused.
Verdicts are from `docs/design/EPOCH-FIXES.md` §1.2/§1.5/§1.6.

| Semantic | Status | Verdict |
|---|---|---|
| `symlink` / `readlink` | ✅ landed (overlay objects; `SYMLINK_MODE` blob entries; resolve inside history snapshots and the `.vfs/cas` portal) | P0 — closed (§1.2) |
| `setattr` **mode** persistence | ✅ landed (persisted in tree entries; survives seal/remount) | P1 — closed (§1.5) |
| `statfs` | ✅ landed (lower's real `statvfs` numbers) | P3 — closed (§1.6) |
| atime/mtime | ✅ landed **in-memory, session-scoped**: explicit `setattr` exact, writes bump mtime, copy-up carries lower times, reads never touch atime (`noatime`-for-reads). Never in CAS — dedup is structural. History snapshots stamp every entry with the checkpoint instant. Resets on remount (documented). | P1 — closed (§1.5) |
| xattr storage | `ENOTSUP` stubs (silently degrading is the right interim posture) | P2 — real storage only when a signing toolchain must run in-mount (§1.6) |
| `link(2)` (hardlink) | no op — adapter answers `EPERM` | P3 — accept `EPERM`; document (§1.6) |
| flock / POSIX locks | no handler; kernel emulates | P3 — document cross-path incoherence (locks in a mount are not coherent with locks on the host path for the same file) (§1.6) |
| mmap coherence | unmeasured; no `writeback`/`direct_io` mount opts | P3 — the bench suite measures it; no implementation demanded (§1.6, §2.4) |
| fallocate / punch-hole (sparse) | no op | P3 — acceptable; chunked CAS represents holes-as-absence at agent sizes (§1.6) |
| seal-on-close in `release` | not yet (writers seal at checkpoint time) | roadmap §3.5 |

Refused / never (per EPOCH-FIXES §3.3): **read-deny of host paths outside the
mountpoint** — a mount can only see paths that traverse it, so no VFS-layer
decorator can deny `~/Documents`; that is permanently the syscall layer's job.

### 3.7 Timestamp semantics (the three-layer model)

Where a time comes from, per view — the full story after the atime/mtime work:

| View | mtime/atime source | Notes |
|---|---|---|
| **live mount** | the in-memory inode | explicit `utimes`/`FATTR_MTIME` sets it exactly; a write bumps it to now; a fall-through file reports the lower's real stat until copied up (and copy-up carries those times). Reads never update atime. |
| **history snapshot** (`.vfs/history/*`, `.vfs/cas/<tree>`) | the snapshot's checkpoint event `timestamp_ms` | every entry stamped with the checkpoint instant — a point-in-time view has one honest time. Applied in `SubMounts::remap_attr`, not stored. |
| **CAS and redb** | nothing — time-free, always | identical content ⇒ identical blob/tree ids regardless of mtime; a time-only change between seals yields the *same* root hash. |

Consequences (accepted, documented): times do **not** survive unmount →
remount (a remounted overlay reports the epoch until re-stamped), and
`make`-style freshness works within a mount session but does not resume
across one (a build does not span remounts).

### 3.6 VfsService semantics survey (per EPOCH-FIXES §1.6)

Full operation surface, what landed, and what is deliberately refused.
Verdicts are from `docs/design/EPOCH-FIXES.md` §1.2/§1.5/§1.6.

| Semantic | Status | Verdict |
|---|---|---|
| `symlink` / `readlink` | ✅ landed (overlay objects; `SYMLINK_MODE` blob entries; resolve inside history snapshots and the `.vfs/cas` portal) | P0 — closed (§1.2) |
| `setattr` **mode** persistence | ✅ landed (persisted in tree entries; survives seal/remount) | P1 — closed (§1.5) |
| `statfs` | ✅ landed (lower's real `statvfs` numbers) | P3 — closed (§1.6) |
| atime/mtime persistence | accepted-and-ignored | P1 — documented gap; `make`-style mtime reasoning broken in-mount until a tree-format v2 (§1.5) |
| xattr storage | `ENOTSUP` stubs (silently degrading is the right interim posture) | P2 — real storage only when a signing toolchain must run in-mount (§1.6) |
| `link(2)` (hardlink) | no op — adapter answers `EPERM` | P3 — accept `EPERM`; document (§1.6) |
| flock / POSIX locks | no handler; kernel emulates | P3 — document cross-path incoherence (locks in a mount are not coherent with locks on the host path for the same file) (§1.6) |
| mmap coherence | unmeasured; no `writeback`/`direct_io` mount opts | P3 — the bench suite measures it; no implementation demanded (§1.6, §2.4) |
| fallocate / punch-hole (sparse) | no op | P3 — acceptable; chunked CAS represents holes-as-absence at agent sizes (§1.6) |
| seal-on-close in `release` | not yet (writers seal at checkpoint time) | roadmap §3.5 |

Refused / never (per EPOCH-FIXES §3.3): **read-deny of host paths outside the
mountpoint** — a mount can only see paths that traverse it, so no VFS-layer
decorator can deny `~/Documents`; that is permanently the syscall layer's job.

## 4. Deferred (agreed, not started)

- Evictable / persistent inodes (FUSE-`forget`; ids stable across remount) and
  sub-mount/minted-id cache eviction — same problem class.
- Spill staging tier for working sets larger than RAM.
- `.epochignore` live reload (rules load once at mount).
- Control plane (REST/gRPC) + `epoch-cli`; non-FUSE adapters; merge events.
  The **store-side landing verbs landed ahead of the daemon** (per
  `docs/design/EPOCH-FIXES.md` §1.3/§5.1: export is the lane lifecycle's
  correctness, the daemon is ergonomics): `epoch diff|export|audit
  <state-dir>` — semantic JSON diff with reconstructed rename pairs, merged-
  snapshot materialization, and the mount-attributed write log. Schema and
  claims: `docs/reference/EXPORT-AUDIT.md`. Still deferred from the original
  item: the control plane itself and merge events (`epoch` deliberately has
  no merge verb).
- Housekeeping: spent `.worktrees/{vfs-debug,route-ignore}` workspaces can be
  `jj workspace forget` + removed.

## 5. Resuming

- Trunk is linear; new work stacks on `main`. Each feature above also has its
  own bookmark for review.
- `docs/TIMELINE.md` holds the checkpoint/timeline contracts and accepted
  trade-offs; `docs/ARCHITECTURE.md` the as-built crate map (note: its `.vfs`
  section predates the feature-registry refactor and the typed portal —
  refresh alongside the daemon work).
