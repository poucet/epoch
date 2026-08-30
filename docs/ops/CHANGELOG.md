# Changelog

What got added, per landing — dated with a clock and the jj change id. The entry
format is stated once in SYSTEM.md §2.

## Unreleased

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
