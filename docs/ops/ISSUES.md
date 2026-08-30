# Issues

A parking lot, not a queue — defects and rough edges noticed in passing; nothing
here is scheduled until promoted onto the board. What belongs here is stated once
in SYSTEM.md §2.

| Status | Found | What | Verdict |
|---|---|---|---|
| open | 2026-08-30 · qnxoqvlu | `Dockerfile` can bake a stale `epoch` binary: `RUN --mount=type=cache,target=/src/target cargo build … && cp target/release/epoch-fuse /usr/local/bin/epoch` ships the cached artifact unchanged when context source mtimes predate the cache-mount artifacts (observed: a fix present in `/src` was absent from `/usr/local/bin/epoch`; only `docker build --no-cache` produced a correct binary). Affects `docker-run.sh` rebuilds after worktree checkouts with old mtimes. | unscheduled |
