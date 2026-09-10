# Changelog

What got added, per landing — dated with a clock and the jj change id. The entry
format is stated once in SYSTEM.md §2.

## Unreleased

### GitHub Actions CI & raw vs mount benchmark publishing · 2026-09-10 08:06 · pzqtvusm

- **Multi-platform CI workflow** (`.github/workflows/ci.yml`): Linux CI runs full `cargo check`, `cargo test --workspace --all-targets`, in-process benchmarks, and live kernel FUSE mount benchmarks over `/dev/fuse`. macOS CI kept minimal as a fast compilation check (`cargo check --workspace --all-targets`).
- **Benchmark reporting**: `epoch-bench` gains `--markdown <file>` and `--no-fail-exit` flags, generating GitHub Flavored Markdown comparison tables (raw disk vs epoch-fs) with resource metrics (CPU, RSS, ratios). CI automatically publishes the table to `$GITHUB_STEP_SUMMARY` and uploads downloadable artifacts.
- **Universal mount runner** (`bench/run-mount.sh`): Auto-detects Linux vs macOS, building with `--features fuser-mount` on Linux and `fuser-adapter` on macOS, handling unmounts appropriately.
- **High-file-count real-world workloads**: Continuous regression testing of procedural git clone (200 files), jujutsu (jj) workflows, and full cargo crate builds from scratch inside the mount.
- **Design proposal** (`docs/design/DAEMON-AND-FSKIT.md`): Architecture roadmap for the Shared Machine Daemon (Option A) to share state between Docker and host macOS, preserving single-binary mode and laying out the FSKit path without requiring a paid Apple Developer account.

### atime/mtime: in-memory session times, checkpoint-stamped history · 2026-08-30 02:44 · ynvtzmkokpkm

Lane `times-persistence`, per `docs/design/EPOCH-FIXES.md` §1.5 — pulled
forward from "best-effort / acceptable to leave" by Chris, with a design
constraint that replaced the briefed tree-format v2: **times never live in
content-addressed storage** (a time in the tree would make identical content
hash differently per mtime and collapse CAS dedup across checkpoints and
forks). The tree codec therefore stays v1 and time-free; what landed is the
three-layer model (TASKS.md §3.7):

- **Live mount — in-memory session times** on the `WorkingTree` inode:
  explicit `setattr` (`utimes(2)`/`FATTR_ATIME`/`FATTR_MTIME`, `UTIME_OMIT`
  per-field) sets them exactly; `write_at`/`truncate`/creates bump `mtime`
  (and `atime`) to now; copy-up of a fall-through file carries the lower's
  own times until written; **reads never touch atime** (effective `noatime`
  for implicit updates — also what keeps a read from arming the checkpoint
  trigger). `ctime` is the metadata-change instant (chmod/utimes/rename).
- **History/portal views — checkpoint-instant stamps**: `.vfs/history/*` and
  `.vfs/cas/<tree>` entries all report `mtime = atime =` the snapshot event's
  `timestamp_ms`, applied in `SubMounts::remap_attr` via a per-root stamp
  registry (`TreeMounts::stamp_snapshot`), never stored. A point-in-time view
  has one honest time.
- **CAS + redb — time-free, always**: dedup is structural. Test
  `times_never_change_content_hashes` proves two same-bytes files with
  *different* mtimes share one blob id and one root tree id, and a time-only
  change between seals yields the *same* root (no phantom checkpoint).

Verified through a real macFUSE mount: `touch -d` exact; `touch -a`/`-m`
independent; write bumps mtime; read leaves atime; `cp -p` preserves mtime
(lower *and* overlay); a `make` demo builds/skips/rebuilds correctly; two
checkpoints stamp the same unchanged file at their own instants; remount
shows mode persisted (in CAS) and times reset to epoch — the documented
consequence, with the brief's remount-persistence acceptance criterion
**dropped** (session-scoped by design; a build does not span remounts).

Cost: `SetAttrRequest`'s atime/mtime are now honored (was accept-and-ignore);
`PassthroughVfs::setattr` applies times to the host via a new `utimensat`
helper in `epoch-vfs-api` (`epoch-vfs` is `forbid(unsafe_code)`). The
`.vfs/history` attr-synthesis layer grew a stamp registry — the only
threading the event timestamp needed.

### vfs-semantics batch: symlink + setattr-mode + statfs · 2026-08-30 01:49 · pynyrqopqtvt

Lane `vfs-semantics`, per `docs/design/EPOCH-FIXES.md` §1.2/§1.5/§1.6. Three
`VfsService` semantics, verified through a real macFUSE mount on this machine.

- **symlink/readlink as overlay objects** (§1.2): `VfsService` gains
  `symlink`/`readlink`. Upper symlinks seal git-style — a blob entry whose
  mode is `SYMLINK_MODE` (`0o120000`), blob bytes = target path — so
  checkpoints, `.vfs/history/latest`, and the `.vfs/cas` portal reproduce
  them exactly; lower symlinks fall through and delegate `readlink`;
  rename/chmod of a fall-through link copies its target up; `.vfs/debug/overlay`
  marks them `(symlink)`. **No new CAS kind and no format change**: the tree
  codec already carried per-entry modes and the moded-blob design
  (`epoch-tree`'s `SYMLINK_MODE`), so the storage-format batch the brief
  budgeted for came out free.
- **setattr mode persistence** (§1.5): `VfsService::setattr` +
  `WorkingTree::set_mode`. chmod lands in the tree entry's mode and survives
  seal → rehydrate → remount (verified: `chmod 755` still `755` after a
  remount). Fall-through files/symlinks copy up on chmod; a chmod'd
  fall-through *directory* force-emits its own entry (`mode_override`) so the
  mode lands in the delta. atime/mtime accepted, not persisted — documented
  known gap for `make`-style tooling.
- **statfs** (§1.6): `VfsService::statfs` → `StatFs`, answered with the
  lower's `statvfs(2)` numbers (in-memory trees answer a synthetic unbounded
  shape). `df` inside a mount now prints the host's exact 1K-block count
  (block counts are `f_frsize` units — using `f_bsize` made `df` 256×
  optimistic on macOS).
- Threaded through every decorator: `CasVfs`, `PassthroughVfs` (host
  `symlink(2)`/`readlink(2)`/`chmod`, writable mode), `RouterVfs`
  (rule-matching symlinks land on the host), `SignalVfs` (symlink/setattr arm
  the checkpoint trigger), `LoggingVfs`, `ControlPlaneVfs`, the
  `ReadOnlyFeature` blanket, `SubMounts`/`TreeBrowseFeature` (readlink
  resolves inside history snapshots and portal-mounted trees), and the fuser
  adapter (`symlink`, `readlink`, `statfs` handlers; `setattr` mode path).

Cost: `VfsService` grew four defaulted methods (`Unsupported` unless
overridden — existing implementors keep compiling); the `Body::Dir` variants
carry a `mode_override` flag. atime/mtime persistence was deliberately left
out (§1.5's best-effort allowance).

**rename(2) works on macFUSE 5.x; rmdir of emptied lower dirs fixed** · 2026-08-30 01:40 · qnxoqvlu
 - Root cause: fuser 0.17 parses `FUSE_RENAME` as the 8-byte Linux `fuse_rename_in` unless its `macfuse-4-compat` feature is on; macFUSE 4.x/5.x send 16 bytes (`newdir`+`flags`+`padding`), so both name strings were read out of zero padding and arrived empty — 100% rename failure (`mv`, editor atomic saves, `jj git init`). Fix: enable `macfuse-4-compat` for `cfg(target_os = "macos")` only, in `crates/epoch-fuse/Cargo.toml` (per-target feature union; Linux untouched).
 - Follow-on found by the live acceptance test: `rmdir` of an emptied fall-through directory failed ENOTEMPTY because `WorkingTree` counted the lower `readdir`'s synthesized `.`/`..` entries as children (and `readdir` cached them as real nodes). Both lower-merge points now skip dot entries; regression test `overlay::rmdir_emptied_fall_through_dir_ignores_lower_dot_entries`; the test lower now emits dot entries like the real `PassthroughVfs`.
 - Verified live on macFUSE 5.3.1: `mv` of lower file / upper file / lower directory (EXDEV → copy+delete), editor atomic-save cycle, `jj git init` + commit inside a mount; same suite green on Linux (Docker, `fuser-mount`).
 - Cost: none user-visible; macOS and Linux now parse different rename wire widths, per-target. Found-not-fixed: Dockerfile cargo cache-mount can bake a stale binary (see ISSUES).

### 2026-08-30 02:15 · nttzrqvn — export/landing verbs + audit surface

`epoch diff|export|audit <state-dir>` (new `epoch` binary in `epoch-fuse`,
no FUSE feature needed; logic in `epoch_history::inspect`): schema-versioned
JSON diff with reconstructed rename pairs (blob-id, subtree-id, and
cross-lower content-hash matching), merged-snapshot materialization honoring
tombstones, and the derived write log with mount attribution. Mounts now
stamp every checkpoint's `meta` with schema-v1 attribution
(`epoch_timeline::Attribution`: mount id, head, user, host, lower);
`CheckpointedFs::with_meta` plumbs it through. Cost: the first committed
schema on the free-form `meta` field; store-side reads still obey redb's
single-writer rule (live state dirs must be unmounted or copied — the verbs
say so). Spec: `docs/reference/EXPORT-AUDIT.md`.

- **epoch-bench: the benchmark suite EPOCH-FIXES §2.4 mandates** · 2026-08-30 02:13 · qupstnsnnzwy
  New crate `crates/epoch-bench` (+ `bench/run-mount.sh`, `bench/README.md`):
  micro op-classes (stat-walk, read, write+fsync, copy-up, rename, unlink),
  macro envelopes (spawn-heavy churn with writes forced in-dir, 10⁴-entry
  stat-storm, editor atomic-save, bulk-write), spin-up grid {10³,10⁴,10⁵} ×
  {10 MB,1 GB,10 GB} (sparse-by-default, warm-cache caveat printed), sqlite
  mmap smoke, and realworld pairs (git/jj flows, sed -i loop, cargo build
  ×bare (same machine, same run), workload CPU (rusage), daemon CPU + peak
  RSS (ps sampling), §2.2 ceilings pass/fail. Known breakage (macFUSE 5.x
  rename wire bug, symlink ENOSYS) degrades to KNOWN-FAIL, never crashes.
  First live-mount numbers: stat-walk 25× (≤3× FAIL), reads 10.3× (≤2× FAIL),
  stat-storm 20× at 10⁴-scale (FAIL), copy-up 10 ms ✓, write+fsync 0.6 ms ✓,
  mount→first-read 247–250 ms ✓ at ≤10⁴ files, bulk-write RSS growth
  +882 MiB per GiB written (the §2.3 staging-tier hazard, now measured). Cost: one workspace crate with deps on
  epoch-cas/epoch-vfs/epoch-vfs-api + libc; the report-only rows carry no
  ceilings by design (§2.3).
