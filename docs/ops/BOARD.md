# Board

Line-items only; what belongs here is stated once in SYSTEM.md §2. Live state
(what is running right now) lives in `docs/live/` — gitignored, invisible to
lanes; a lane gets its context from its brief, not from here.

Legend: 🚧 in flight · ✅ landed · ⏸ deferred.

## In flight

- 🚧 `export-audit` — `epoch diff`/`export`/`audit` verbs + event attribution (EPOCH-FIXES §1.3, §3.2)
- 🚧 `bench` — `epoch-bench` suite: micro/macro/realworld, CPU+RSS costs (EPOCH-FIXES §2.4)
- 🚧 `times-persistence` — atime/mtime in tree-format v2 (EPOCH-FIXES §1.5, pulled forward)

## Milestones

## Queued

## Deferred by decision

## Landed index
- ✅ `fix-rename` — rename(2) on macFUSE 5.x (fuser `macfuse-4-compat` wire layout) + lower-dots `rmdir` fix (EPOCH-FIXES §1.1)
- ✅ `vfs-semantics` — symlink/readlink overlay objects, setattr mode persistence, statfs (EPOCH-FIXES §1.2/§1.5/§1.6)

| Landed | What | Change |
|---|---|---|
| 🚧 2026-08-30 02:15 | export/landing verbs (`epoch diff\|export\|audit`) + mount-attributed audit surface | nttzrqvn |
