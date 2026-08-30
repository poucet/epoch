# Issues

A parking lot, not a queue — defects and rough edges noticed in passing; nothing
here is scheduled until promoted onto the board. What belongs here is stated once
in SYSTEM.md §2.

| Status | Found | What | Verdict |
|---|---|---|---|
| open | 2026-08-30 · qnxoqvlu | `Dockerfile` can bake a stale `epoch` binary: `RUN --mount=type=cache,target=/src/target cargo build … && cp target/release/epoch-fuse /usr/local/bin/epoch` ships the cached artifact unchanged when context source mtimes predate the cache-mount artifacts (observed: a fix present in `/src` was absent from `/usr/local/bin/epoch`; only `docker build --no-cache` produced a correct binary). Affects `docker-run.sh` rebuilds after worktree checkouts with old mtimes. | unscheduled |
| open | 2026-08-30 · nttzrqvn | `PassthroughVfs::readdir` returns `.` and `..` entries — nothing else in the tree does that; a consumer merging listings must filter them itself | open |
| open | 2026-08-30 · nttzrqvn | Sealed entry modes are inconsistent: `create` stores the caller mode verbatim (e.g. `0o644`, no type bits) while copy-up preserves the lower's full `0o100644` — diffs show `mode:420` next to `mode:33188` for the same kind of file | belongs to the vfs-semantics lane's setattr/mode persistence work |
