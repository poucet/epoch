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
