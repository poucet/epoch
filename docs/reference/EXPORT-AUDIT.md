# `epoch diff` / `epoch export` / `epoch audit` — the store-side verbs

**Status:** implemented (2026-08-30, lane `export-audit`)
**Binary:** `epoch` (in `epoch-fuse`, no FUSE feature needed) · logic:
`epoch_history::inspect` · attribution schema: `epoch_timeline::Attribution`

The lane lifecycle's landing verbs and the audit surface. All three are
**store-side**: they read `<state-dir>/epoch.redb` directly and need no live
mount. Only **checkpointed** state is visible — content still in a running
mount's staging tier is, by design, not there yet.

```text
epoch diff   <state-dir> [--head <name>] [--base <event-hex>] [--lower <dir>]
epoch export <state-dir> [--head <name>] [--lower <dir>] <dest-dir>
epoch audit  <state-dir> [--head <name>] [--lower <dir>]
```

## The single-writer caveat (read this first)

redb is single-writer. A state dir held by a **running** mount refuses to
open (`DatabaseAlreadyOpen`); the verbs say so and suggest the remedy: unmount
first, or copy the state dir (`cp -Rc state state-copy`) and point the verb at
the copy. A copied state dir is a consistent snapshot (redb is a single
crash-safe file); checkpoints taken after the copy are simply absent from it.

## The lower

Checkpoint roots of an overlay mount are **sparse deltas**; names absent from
the delta fall through to the lower directory. The verbs resolve the lower
from (in order): `--lower`, or the `lower` field of the head event's
attribution meta (every mount records it since attribution schema v1), or
nowhere — in which case the base is treated as empty and three
classifications degrade: renames across the lower boundary cannot be
reconstructed, a dropped upper entry over an existing lower path reports as
`delete` (not `restore`), and a lower directory that merely gained overlay
entries reports as `mkdir`.

## `epoch diff` — semantic diff, machine-readable

Diff of the head's checkpoint **against the lower base** (default) or against
an explicit `--base <event-hex>` (pair diff of two roots). JSON:

```json
{"schema":1,"type":"diff","state":"…","head":"main","head_event":"<hex>",
 "base":{"kind":"lower","lower":"/abs/path"} | {"kind":"event","event":"<hex>"},
 "entries":[
   {"op":"write","path":"a.txt","content_hash":"<blob-hex>","size":5,"mode":33188},
   {"op":"delete","path":"old.txt","previous_hash":"<blob-hex>"|null,"dir":false},
   {"op":"mkdir","path":"d"},
   {"op":"rename","from":"a","to":"b","content_hash":"<blob-hex>"|null,"dir":false},
   {"op":"restore","path":"reverted.txt","dir":false}
 ]}
```

- Ops: `write` (created **or** overwritten — without per-version lower
  comparison these are one op), `delete` (upper entry dropped, or a tombstone
  hides the lower entry; `previous_hash` is set only when the deleted content
  was overlay-owned), `mkdir`, `rename`, `restore` (overlay entry lifted, the
  lower path shows through again — distinguishable only with a known lower).
- **Renames are reconstructed**: content that vanished at A and appears
  byte-identical at B is one `rename A→B` entry, not delete+create. Overlay
  files pair by blob id; directories pair by identical subtree id; a
  tombstoned *lower* file pairs with its re-created content by hashing bytes
  on both sides (only possible with a known lower).
- `content_hash` is the blob's content address — resolvable in-mount under
  `.vfs/cas/<hash>`. `mode` is the raw unix mode (`0o120000` = symlink, blob
  bytes are the target).
- Paths are raw unix bytes rendered **lossily** into JSON strings.

## `epoch export` — materialize the merged snapshot

Writes the head's merged snapshot (delta over lower, tombstones honored) into
`<dest-dir>`, which is created and must be empty. Overlay files come from the
CAS with their recorded permission bits; lower fall-through entries are copied
(`fs::copy`, permissions preserved); symlinks are recreated as symlinks. A
lower `.vfs` entry at the root is skipped, mirroring the in-mount view (the
control plane shadows it). The result is byte-identical to what the mount
showed (same code path interprets the same trees; lower drift after the last
checkpoint applies equally to both until `OpaqueTree` lands). Stdout is a JSON
summary: `{"schema":1,"type":"export",…,"files":N,"dirs":N,"symlinks":N,"bytes":N}`.

Export materializes a snapshot rather than applying a diff onto a working
copy: the snapshot is the artifact every consumer wants (a landable tree), it
cannot conflict, and it is one walk instead of diff-then-apply round-tripping
through path mutations. Consumers that want patch semantics can diff first
and apply `entries` themselves.

## `epoch audit` — the derived write log

For each consecutive first-parent event pair on `--head` (chronological), the
pair diff, each entry stamped with the event's checkpoint timestamp and mount
attribution:

```json
{"schema":1,"type":"audit","state":"…","head":"main","claims":"…",
 "entries":[
   {"ts":1700000000000,"event":"<hex>","mount":"m-9f2c…","user":"chris",
    "host":"mac","op":"write","path":"a.txt","content_hash":"…","size":5,"mode":33188}
 ]}
```

`mount`/`user`/`host` are `null` for events without attribution (anything
checkpointed before schema v1, or test/in-memory mounts).

### Attribution (event `meta`, schema v1)

Every checkpoint a mount commits carries
`{"v":1,"mount":"m-<16hex>","head":"main","user":"…","host":"…","lower":"…"}`
in `TimelineEvent.meta` (`epoch_timeline::Attribution`; `user`/`host`/`lower`
optional). The mount id is generated once per mount; a parent orchestrating
lanes maps mount id → lane at spawn time. Old events carry no meta and decode
fine.

### The claims, precisely

- **Complete for writes and deletes of paths under the mountpoint** that were
  captured into checkpoints.
- **Timestamp resolution is the checkpoint debounce** (5 ms–500 ms), not
  per-op. Ops share an event's timestamp; an op made and undone within one
  debounce window leaves no trace.
- **Reads are not recorded at all.** Fall-through reads never touch the
  overlay; nothing here says anything about them.
- **The overlay is not a sandbox, and this audit surface is scoped to
  accidents and review.** A child inside a mount can `exec` anything, write
  the host directly, read outside the mountpoint, or unmount and write the
  lower — none of which leaves a trace here. Detection of malice stays with
  the syscall layer and role authority; this log is the instrument for
  catching *mistakes* and for parent-side review.
