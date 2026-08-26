# epoch

A time-traveling copy-on-write filesystem for AI agents and developer
workflows.

`epoch` mounts (via FUSE) a writable overlay on top of any directory: reads
fall through to your real files, while writes and deletes are captured into a
content-addressed store and **checkpointed automatically** — debounced on
flush/close, like an editor that never forgets. Every checkpoint is an event
in an append-only timeline DAG with named heads, so parallel agents can fork
from a shared base, work destructively, and be scrubbed backward — without
your real project ever being touched.

```sh
./mount-mac.sh ~/my/project        # durable checkpointed overlay at ~/epoch
cd ~/epoch
echo "reckless agent edit" > src/main.rs        # captured, checkpointed ~5ms later
cat .vfs/history/30s-ago/src/main.rs            # ...the file as it was
```

**Status: experimental.** Single-machine, macOS-first (Linux via Docker for
parity testing). Storage formats still change without migration.

## Time travel & introspection — the `.vfs` control plane

Every mount grafts a synthetic `.vfs/` directory serving pluggable features:

| Path | What you get |
|---|---|
| `.vfs/history/<unix-millis>/…` | read-only snapshot at the newest checkpoint ≤ that time |
| `.vfs/history/<N><unit>-ago/…` | relative time travel: `500ms-ago`, `30s-ago`, `5m-ago`, `2h-ago`, `1d-ago` |
| `.vfs/history/head` | the head checkpoint's root tree hash (feed it to `.vfs/cas/`) |
| `.vfs/history/latest/…` | the head checkpoint's merged snapshot (delta over the lower) |
| `.vfs/history/id/<event-hex>/…` | any timeline event by id |
| `.vfs/cas/<hash>` | typed object portal: a tree hash browses as a read-only directory, a file hash `cat`s its content |
| `.vfs/debug/…` | live stats: staged object count, dirty files, the overlay delta |

```sh
ls .vfs/cas/$(cat .vfs/history/head)     # browse the head checkpoint by hash
```

Drop a gitignore-syntax **`.epochignore`** in the lower directory to route
subtrees (`node_modules/`, `.git/`, `.jj/`, caches…) straight through to the
host — reads *and* writes bypass the overlay and never enter history.

## How it works (short version)

- **Objects** — an id-agnostic content-addressed kernel (BLAKE3 + zstd) stores
  chunks, file manifests, and Merkle trees, each behind a hashed git-style
  kind tag. Identical content dedups everywhere.
- **Tiers** — writes land in a non-evictable in-memory staging tier; a
  checkpoint promotes the root's not-yet-durable closure (post-order, batched)
  into a moka-cached, redb-durable tier. The durable store holds exactly the
  closure of checkpointed roots.
- **Timeline** — checkpoints are content-addressed events
  `{parents, root, timestamp}` behind an engine-agnostic `TimelineStore`
  trait; lineage lives in the records, forking is pointing a new head at an
  existing event. Crash safety comes from write ordering, not cross-store
  atomicity.
- **VFS** — one small synchronous `VfsService` trait; everything (the CoW
  worktree, pass-through routing, the control plane and each `.vfs` feature)
  is an implementation or decorator of it.

Deep dives: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) (crate map and
layering), [`docs/TIMELINE.md`](docs/TIMELINE.md) (checkpoint/timeline
design), [`docs/TASKS.md`](docs/TASKS.md) (status and roadmap — next up: a
machine daemon so many mounts share one state, then a remote object tier).

## Requirements & setup (macOS)

1. `brew install --cask macfuse`, then enable its kernel extension
   (one-time; see [`docs/MOUNTING.md`](docs/MOUNTING.md)).
2. `./mount-mac.sh [lower-dir] [mountpoint]` — builds and mounts. State
   persists in `~/.epoch` (override `EPOCH_STATE`); remounting resumes from
   the last checkpoint. `EPOCH_HEAD` names the timeline head,
   `EPOCH_HARD_FSYNC=1` makes `fsync(2)` a synchronous checkpoint barrier.

Linux parity testing: `./docker-run.sh`.

## License

MIT — see [LICENSE](LICENSE).
