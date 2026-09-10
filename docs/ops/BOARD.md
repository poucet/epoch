# Board

Line-items only; what belongs here is stated once in SYSTEM.md §2. Live state
(what is running right now) lives in `docs/live/` — gitignored, invisible to
lanes; a lane gets its context from its brief, not from here.

Legend: 🚧 in flight · ✅ landed · ⏸ deferred.

## In flight

## Milestones

## Queued

## Deferred by decision

## Landed index
- ✅ `fix-rename` — rename(2) on macFUSE 5.x (fuser `macfuse-4-compat` wire layout) + lower-dots `rmdir` fix (EPOCH-FIXES §1.1)
- ✅ `vfs-semantics` — symlink/readlink overlay objects, setattr mode persistence, statfs (EPOCH-FIXES §1.2/§1.5/§1.6)
- ✅ `export-audit` — `epoch diff`/`export`/`audit` verbs + event attribution (EPOCH-FIXES §1.3, §3.2)
- ✅ `times-persistence` — in-memory session times (never in CAS), checkpoint-stamped history views (EPOCH-FIXES §1.5)
- ✅ `bench` — `epoch-bench` suite: micro/macro/realworld, CPU+RSS costs (EPOCH-FIXES §2.4)
- ✅ `ci-bench` — GitHub Actions CI (Linux live FUSE + high-file realworld benchmarks, minimal macOS build check, step summary publishing)
- ✅ `vfs-throughput-opt` — readdirplus wire & VFS integration, FUSE kernel readahead/write/splice tuning, snapshot TTL cache, multi-chunk read/write parallelism, CasVfs RwLock, RouterVfs fast-paths
- ✅ `ci-bare-docker` — CI step verifying bare Epoch-FS overlay semantics in Docker (`jj git clone` from GitHub, `cargo build`, binary execution, file modification, and jj commit)

| Landed | What | Change |
|---|---|---|
| 2026-08-30 02:15 | export/landing verbs (`epoch diff\|export\|audit`) + mount-attributed audit surface | nttzrqvn |
| 2026-08-30 02:44 | in-memory session times (never in CAS), checkpoint-stamped history views | ynvtzmko |
| 2026-08-30 03:02 | `epoch-bench` suite: micro/macro/realworld, CPU+RSS costs | qupstnsn |
| 2026-09-10 08:06 | GitHub Actions CI with live Linux FUSE benchmarks + step summary publishing | pzqtvusm |
| 2026-09-10 08:50 | VFS & FUSE throughput optimizations (readdirplus, kernel tuning, chunk parallelism) | rrkyuost |
| 2026-09-10 09:03 | CI bare Epoch-FS verification in Docker (jj git clone & cargo build) | npurszno |
