# epoch-bench — the benchmark suite

The suite EPOCH-FIXES (`docs/design/EPOCH-FIXES.md`) §2.4 requires epoch to
ship: every workload runs **twice** — bare lower and epoch overlay, same
machine, same run — and is reported **as ×bare ratio AND absolute**, against
the §2.2 ceilings. Ceiling FAILs are measurements, not crashes; the suite
exists to regress against.

## Running

```sh
# headless, no macFUSE needed: mount side = in-process CasVfs overlay
cargo run -p epoch-bench -- all

# against a REAL macFUSE mount (the numbers §2.2's ceilings are about):
./bench/run-mount.sh                 # full suite, two-phase mount
./bench/run-mount.sh --quick         # reduced grid for smoke runs
./bench/run-mount.sh --groups "micro macro"

# pieces, against a mount you made yourself:
epoch-bench micro macro --mount ~/epoch --lower ~/project --daemon-pid <pid>
```

`run-mount.sh` builds both binaries, mounts a scratch lower with
`epoch-fuse cas … --state`, runs the suite, unmounts, then re-mounts with
`/target/` in `.epochignore` for the cargo-build-ignore variant (rules load
once at mount time — hence two phases). Scratch defaults to a fresh
`mktemp -d` (`EPOCH_BENCH_SCRATCH` overrides; do NOT point it inside the
repo — the fixture crate would be swallowed by the parent cargo workspace).

Exit code: 0 with ceiling FAILs present (they are data), non-zero only on
harness crash. Known pre-existing breakage (rename on macFUSE 5.x, §1.1;
symlink ENOSYS, §1.2) reports **KNOWN-FAIL**, scoped by workload — never a
crash, never conflated with a real FAIL.

## What it measures

| group | contents | ceilings (§2.2) |
|---|---|---|
| `micro` | stat-walk, full-file reads, write+fsync 4 KiB, copy-up (first write to ≤10 MB lower file), rename (upper file), unlink | ≤3× bare · ≤2× bare · ≤2 ms abs · ≤50 ms abs · ≤2× bare · report-only |
| `macro` | spawn-heavy FS churn (subprocess workers, writes forced in-dir), stat-storm over 10⁴ entries, editor atomic-save loop (tmp+fsync+rename), bulk-write | stat-storm ≤3×, atomic-save ≤2×; churn/bulk report-only (§2.3) |
| `spinup` | {10³, 10⁴, 10⁵} files × {10 MB, 1 GB, 10 GB} grid: mount→first-read, first readdir, first-write | first-read ≤0.5 s (at 10 GB/10⁵, applied to every cell) |
| `mmap` | sqlite3 (system CLI, `PRAGMA mmap_size=256MiB`) create+insert+aggregate | report-only (§1.6: one number) |
| `realworld` | git init/add/commit/status/clone, jj git init+commit, sed -i loop, cargo build --offline (target/ in-dir; overlay and .epochignore variants) | report-only — these are the envelope; the micro numbers explain, these decide |

Every row also carries resource cost: workload user+sys CPU (rusage), daemon
user+sys CPU (`ps` deltas), daemon peak RSS (50 ms sampler). Bulk writes
report **daemon RSS growth per GiB written** — the §2.3 hazard (staging tier
is in-memory and non-evictable) measured, not asserted.

## Rules the suite keeps (and you must too)

- **No sleep-bound suite may appear in a perf claim** (§2.4.5). The repo's
  23 s gate was ~85% sleeps and wrote to host /tmp — it measured no overlay
  I/O. This suite contains no sleeps except mount-readiness polling, and
  every workload's writes land inside the measured directory.
- **Warm-cache caveat** (§2.4.3): macOS has no unprivileged cache drop
  (`purge` needs sudo). Spin-up numbers are warm-cache; a privileged
  cold-cache profile is future work.
- **Sparse spin-up cells**: `--dense` off (default) fills the 1 GB/10 GB
  cells via `set_len` (zero pages). Spin-up latency is a *metadata* cost —
  lazy spine hydration exercises lookup/readdir, not page content — so zero
  pages measure the same paths without ten minutes of entropy writes.
  `--dense` fills with PRNG bytes when you want the content path too.
- **The §2.3 performance contract**: the overlay is for source-shaped
  writes; build outputs belong in `.epochignore`-routed subtrees. The
  `cargo-build-overlay` vs `cargo-build-ignore` pair prices that contract
  numerically. The corresponding hazard: staging is in-memory and
  non-evictable, so GB-scale writes inside the overlay pressure RAM before
  checkpoints drain it — `bulk-write` reports exactly that.
- **fsync caveat**: through the overlay, fsync is a checkpoint *signal*
  (debounced, async), not a disk barrier — so the write+fsync ceiling can
  pass while durability lags. Mount with `--hard-fsync`
  (`EPOCH_HARD_FSYNC=1`) for the durable variant of the number.

## Linux parity (rename already works there)

The rename/atomic-save/git/jj/sed rows are KNOWN-FAIL on macFUSE 5.x until
the §1.1 fix lands; on Linux's in-kernel FUSE they run for real. Recipe via
the dev container (untested glue — the image needs a writable scratch bind):

```sh
docker run --rm -it --device /dev/fuse --cap-add SYS_ADMIN \
  -v "$PWD:/work:ro" -v /tmp/eb-scratch:/scratch epoch-fuse zsh
# inside: build epoch-bench + epoch-fuse, mount at /scratch/mnt, run with
# --mount /scratch/mnt --lower /scratch/lower --daemon-pid …
```

## Interpreting the two mount modes

- **In-process** (no `--mount`): drives `CasVfs` through `VfsService`
  directly. VFS-layer cost only — no kernel round-trip. Immune to the
  macFUSE wire bug (that bug is in the wire decode, not the overlay), so
  rename/atomic-save report real numbers here. Workload CPU includes the
  mount threads (same process); labelled in the banner.
- **Kernel mount** (`--mount`/`run-mount.sh`): the numbers that decide.
  Daemon CPU/RSS columns are live. `.vfs/` control-plane entries are part
  of the measured tree — walks see them too.
