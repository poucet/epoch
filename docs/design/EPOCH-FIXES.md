# What epoch has to fix — requirements proposal

**Status:** Requirements proposal, 2026-08-30 · lane `epoch-fixes`
**Inputs:** `docs/research/epoch/EPOCH.md` (change `rmluqlnn`, on this chain —
all measurements cited below as "research §N" are from it); the epoch tree at
`~/projects/simply/epoch` (read-only, re-verified this session — §7 says what);
the fence design `docs/design/ROLE-WRITE-AUTHORITY.md` rev 2 (authority
workspace, cited as "fence §N").
**For:** Chris wearing the epoch-author hat. This doc is the steering brief:
three mandated areas (§1 missing semantics, §2 performance, §3 audit/security),
the open questions ranked (§4), contradictions stated (§5), costs stated (§6).

The one-sentence version: **epoch is one bug fix (rename on macFUSE 5.x), one
small feature (symlink), and one CLI verb (export) away from being load-bearing
for agent lanes — everything else on this list is either already on epoch's own
roadmap, or something epoch should deliberately refuse to build (§3.3).**

---

## 1. Missing semantics — the blocker list

Ordering principle: what unblocks real agent workloads first. Each entry says
what epoch's tree already claims (cited), the precise required semantic, and
the rank. Ranks: P0 = no real toolchain runs inside a mount without it;
P1 = a specific workflow breaks; P2 = degrades specific toolchains; P3 =
acceptable to leave.

### 1.1 P0 — rename(2) on macFUSE 5.x (bug fix, not a feature)

- **Status in epoch:** claimed done end-to-end — `docs/TASKS.md` §2 ("rename(2)
  end to end (editors' atomic saves work)"), evidently proven on the
  Linux/Docker parity path only. Measured broken on this machine: 100% failure
  rate; the kernel's rename arrives with *empty name buffers*
  (`rename parent=46 name="" newname=""` → epoch answers ENOENT, `mv` gets
  EINVAL; research §3, §4.2.1). Consequence: `jj git init` dies, `mv` fails,
  every editor's atomic save fails.
- **Root cause is identified, and it is not epoch's overlay code.** The overlay
  rename semantics already exist and are correct — `epoch-worktree/src/lib.rs:873-895`
  (verified this session): POSIX replace semantics; a fall-through *file* is
  copied up then moved; upper files/trees re-link for free (content-addressed);
  a fall-through *directory* answers EXDEV so callers fall back to copy+delete
  (correct); the vacated name is whited-out when it exists in the lower. What is
  broken is the wire decode: epoch's own `docs/MOUNTING.md:22-26` already flags
  that fuser 0.17 hard-compiles the macFUSE **4.x** wire layout
  (`macfuse-4-compat`) and is protocol-incompatible with macFUSE 5.x, with
  upstream [cberner/fuser#273] open and unresolved. Empty names are the
  signature of that layout mismatch.
- **Required semantic (already implemented — the requirement is the fix):**
  rename delivered intact on macFUSE 5.x, preserving the existing overlay
  semantics above. The fix lives in the adapter/protocol layer
  (`epoch-fuse/src/fuser_adapter.rs:315-326`, where epoch logs the names it was
  handed) or in a fuser fork/upgrade. **Requirement to epoch: make the fix
  explicit on the roadmap as a macOS-native bug** — today TASKS.md reads as if
  rename is done; nothing on their list owns this.
- **Acceptance:** `jj git init` + a commit + a bookmark move inside a mount on
  macFUSE 5.3.1; `mv` of a lower file, an upper file, and a lower *directory*
  (EXDEV → coreutils fallback must succeed); an editor atomic-save cycle
  (write `.tmp`, rename over target). All on this machine, not Docker.

### 1.2 P0 (for this repo) / P1 (generally) — symlink + readlink

- **Status in epoch:** on the roadmap, behind the daemon — `docs/TASKS.md` §3.5
  ("VfsService gaps — symlink"). `VfsService` has no symlink/readlink operation
  at all (verified: the trait at `epoch-vfs-api/src/lib.rs:250-299` has lookup,
  getattr, readdir, open, read, write, create, mkdir, unlink, rmdir, rename,
  truncate, flush, fsync, release — nothing else). Measured: ENOSYS everywhere,
  and `.epochignore` routing does not help because `PassthroughVfs` implements
  the same trait (research §3, §4.2.2).
- **Why it is P0 *here*:** this repo has exactly two symlinks and both are
  load-bearing — `SYSTEM.md` (every lane's brief mandates reading it) and
  `.pi/agents` (personas). `ls -la` of the repo root errors inside a mount.
  jj cannot snapshot a tracked symlink either.
- **Required semantic — decide and document:** symlink must be an **overlay
  object**, not passthrough-only. Rationale: (a) jj/git must snapshot symlinks
  through the mount (readlink must resolve for overlayed paths); (b) the audit
  surface must *record* symlink creation (a lane that replaces a symlink has
  made a write — see §3.1); (c) checkpoint trees must be able to seal a
  symlink as a CAS object, else `.vfs/history` snapshots diverge from live
  state. Passthrough readlink is the cheap half and lands first; overlay-object
  symlinks are the correct half. `mkdir -p`-style parent semantics unchanged.
- **Acceptance:** `cat SYSTEM.md` and `readlink .pi/agents` inside a mount;
  `jj commit` of a tree containing the tracked symlinks; a created symlink
  survives checkpoint and appears in `.vfs/history/latest`.

### 1.3 P1 — export/landing tooling (the lane lifecycle's missing verb)

- **Status in epoch:** deferred — `docs/TASKS.md` §4 (control plane +
  `epoch-cli`; merge events "nothing writes a merge yet", `docs/TIMELINE.md` §4).
  The binary has three verbs, none of them export (verified, `epoch-fuse/src/main.rs`
  usage block). The raw material exists: `.vfs/history/latest/` (merged
  snapshot) + `.vfs/debug/overlay` (changed-path list), readable from outside
  the mount while it runs (research §3, §4.2.4).
- **Required semantic:** an `epoch export` verb (or two: `epoch diff <head>
  <base>` and `epoch export <head> <dest>`) that produces, from the store,
  without the mount being live: (a) a machine-readable **diff** of a head vs
  the lower base — path, op ∈ {write, delete(tombstone), mkdir, rename-pair},
  content hash; (b) an **apply/export** mode that materializes the merged
  snapshot (honoring tombstones) to a destination directory, or applies the
  diff onto a working copy. Rename pairs should be reconstructed
  (same-content-hash move = rename, not delete+create) because review wants to
  read "renamed A→B", not two unrelated ops.
- **Why epoch should own it (not each consumer):** every consumer of overlays
  for agent lanes needs exactly this; a ~50-line exporter per consumer
  (research §4.2.4) duplicates and drifts. It does **not** require the daemon
  or the full control plane — it is store-side, and TIMELINE.md's designs
  already assume head/event querying. **Pull it forward ahead of the daemon**
  (contradicts research §8's ordering — §5.1).

### 1.4 P1 — concurrent mounts on a shared store (daemon)

- **Status in epoch:** already their next task — `docs/TASKS.md` §3.1,
  `docs/TIMELINE.md` §3 ("VfsService cut": one daemon owns worktrees/staging/
  cache/timeline; mounts become UDS shims; `fork <head> from <event>` as a
  control verb). Measured today: second mount on one `--state` dir dies with
  `DatabaseAlreadyOpen` (research §3) — redb is single-writer; N lanes = N
  state dirs, zero shared dedup; no CLI fork verb, only library-level
  `set_head` (research §5, verified `epoch-timeline/src/store.rs:50`).
- **Requirement:** no new semantics demanded beyond their design — but with two
  additions this workflow needs: (a) `fork` must be a *CLI/daemon verb*, not
  library-only (a lane spawn is a shell command, not a Rust program); (b) the
  daemon must expose the audit surface of §3 for **all live mounts at once**
  (per-lane attribution is the point — a parent with six lanes wants six audit
  streams, which today means six state dirs anyway, so the daemon is what makes
  attribution *shared and comparable*).
- **Interim requirement (pre-daemon):** document the N-state-dirs pattern as
  the supported concurrent story, including the poor-man's fork (copy
  `epoch.redb` per lane — research §5, inferred, untested; epoch should either
  bless it with a test or document it as unsupported).

### 1.5 P1 — setattr persistence (mode; times second)

- **Status in epoch:** on the roadmap — TASKS.md §3.5 ("full `setattr`
  persistence (mode/times are accepted-and-ignored)"). Verified: the adapter
  handles `setattr` (`fuser_adapter.rs:165-190`) but only `size` does anything
  (→ truncate); mode/times are swallowed. This is **not** on the research
  doc's blocker list — added here because it breaks a class the research
  didn't run: **build toolchains.** `cargo` sets the executable bit by chmod
  *after* writing the binary (reasoned from std's
  `fs::write`+`set_permissions` pattern, not measured — flagged); with mode
  accepted-and-ignored, a built binary inside a mount is not executable and
  `cargo run` / `./target/debug/x` fails EACCES. git/jj also record
  100755-vs-100644 and would snapshot wrong modes.
- **Required semantic:** mode persisted in the file manifest (a storage-format
  change — epoch's README says formats change without migration, so batch it
  with other manifest changes), applied on copy-up and honored in synthesized
  attrs; mtime/atime best-effort (few toolchains in this workflow check them;
  `make` does — accept `make` breakage inside mounts as a known gap until
  then, and say so in epoch's docs).

### 1.6 P2/P3 — the survey the research didn't run: xattrs, hardlinks, flock, mmap, sparse, statfs

Verified against the tree this session; each with its weight for the
agent-workspace use case:

| Semantic | State in epoch (cited) | Weight for agent workloads |
|---|---|---|
| **xattr** | ENOTSUP stubs for get/set/remove + empty list (`fuser_adapter.rs:343-371`); *storage* deferred TASKS §3.5 | **P2.** Codesigning (`codesign`, notarization) and anything touching Finder metadata breaks; npm/git/tsc/jj don't care. Silently degrading today (ENOTSUP, not ENOSYS) is the right interim posture — keep it, add real storage only when a signing toolchain must run in-mount. |
| **hardlink (`link(2)`)** | EPERM (research §3, measured) — no operation in `VfsService` (trait verified) | **P3.** Rarely load-bearing for agents; some build tools (ccache, some linkers) dedup via hardlinks and will fall back or lose dedup. Accept EPERM; document it. |
| **flock / POSIX locks** | No lock handler in the adapter (verified — `fuser_adapter.rs` has none); the research's probe measured flock "succeeds" (research §3), i.e. the kernel emulates it | **P3, with one honest caveat.** Locks taken inside a mount cannot be coherent with locks taken on the *host path* for the same file — two inode spaces, structurally. Cross-process exclusion *within* a mount is what agent tooling actually does (lockfile-as-flag doesn't use flock anyway). Document the caveat; implement FUSE forwarding only if a measured workload needs it. |
| **mmap coherence** | Unmeasured (research has no number). No `writeback` cache and no `direct_io` — mount options are only `FSName` + `DefaultPermissions` (`fuser_adapter.rs:43-45`, verified) | **P3 for agents, P2 for heavyweight builds.** Default FUSE mode means mmap'd writes traverse the write path page-by-page — coherent but slow. Node/tsc/esbuild don't mmap project files; sqlite does (with a fallback). Requirement: a **measurement** in the bench suite (§2.4), not an implementation. |
| **sparse files** | pwrite-at-offset and truncate work (research §3, measured); no fallocate/punch-hole | **P3.** Acceptable — chunked CAS naturally represents holes-as-absence for the sizes agents produce. |
| **statfs** | On the gap list (TASKS §3.5); no handler in the adapter (verified) | **P3.** `df` shows nonsense; installers that pre-check free space (npm) may bail. Cheap to add; do it with §1.5. |

**Not demanded:** `OpaqueTree`/compaction and GC (TASKS §3.3–3.4) stay on
epoch's own schedule — they matter to this workflow only as state-growth
hygiene (§2.5).

---

## 2. Performance goals — measurable, with a suite to prove them

### 2.1 The workload envelope (what "agent-fast" must mean)

Three shapes, because they stress different operations:

1. **An agent session's churn** — 10²–10³ small-file ops/hour: creates, appends,
   truncates, editor atomic saves (write-tmp + rename), occasional mkdir/rmdir.
   Latency-sensitive, throughput-irrelevant.
2. **A stat-storm (build/discovery)** — a tool walking 10³–10⁵ entries
   (`cargo metadata`, `tsc --build`, `rg --files`, `jj status`): lookup +
   getattr per entry, readdir per dir. This is where the 60× hurts.
3. **A test-suite / build run** — mixed reads + writes + spawned processes,
   with the sleep-bound character of the repo's own gate removed (research
   §3/§6.2: the 23 s gate is ~85% sleeps and its overlay-vs-bare wall clock was
   indistinguishable *because it cannot see I/O*; also its writes go to host
   /tmp, so the overlay captured zero writes — a benchmark suite must force
   writes inside the mount or it measures nothing).

### 2.2 Ceilings per operation class — and where today violates them

All ratios vs the bare lower on the same machine; baselines are the research's
measurements (research §3), cited not re-measured. Ceilings are *requirements
for agent usability*, chosen so a lane inside a mount never notices the mount.

| Class | Ceiling | Today (research §3) | Verdict |
|---|---|---|---|
| Full-tree stat walk | **≤ 3× bare** | ~60× (0.5 → 30–32 ms/round, 94 files) | **violates ×20** — the one that gates builds |
| Full-file reads | **≤ 2× bare** | ~9× (1.2 → 10.5–10.9 ms/round) | **violates ×4.5** |
| Small-file write+fsync | **≤ 2 ms absolute** (create/write/close of ≤4 KiB) | 0.65–0.85 ms | passes with margin |
| Copy-up on first write to a lower file | **≤ 50 ms for ≤10 MB** | unmeasured | spec, then measure |
| Rename (upper file) | ≤ 2× bare | unmeasurable (broken, §1.1) | gated on §1.1 |
| Mount → first successful read | ≤ 0.5 s at 10 GB / 10⁵ files | 0.25 s at 1.3 MB / 93 files | unmeasured at scale — the claim is inferred from lazy hydration (research §5); make it measured |
| Bulk build output inside the overlay | **no ceiling** — routed via `.epochignore` (see §2.5) | ~7–9× per-op, sub-ms absolute | explicitly out of scope |

Why 3×/2× are achievable-in-principle: a fall-through stat/read need only
answer "is this path overlaid?" before delegating — an O(1) overlay-membership
check (prefix set / hash of dirty paths) plus the host syscall. Today every
fall-through walks the full overlay/router machinery per component. If epoch
cannot reach the ceilings with that structure, the honest alternative is a
documented fast-path design decision, not a lowered ceiling.

### 2.3 What is acceptable to leave slow (and say so)

- **Checkpoint promotion** — debounced 5 ms/500 ms, sealed + promoted in the
  background (research §2, TIMELINE.md). Never on the child's critical path;
  no ceiling.
- **Unmount final checkpoint** — 0.02 s measured (research §3); fine.
- **Bulk writes inside the overlay** — the *intended* answer for build
  directories is `.epochignore` passthrough (bypasses overlay and history by
  design, TASKS.md §1). Epoch should state this as the performance contract:
  "the overlay is for source-shaped writes; build outputs belong in ignored
  subtrees." The corresponding hazard to document: the staging tier is
  in-memory and non-evictable (TASKS §4, spill deferred) — a build writing GBs
  *inside* the overlay pressures RAM before checkpoint drains it.
- **State-store growth** — no GC yet (TASKS §3.4); 6.3 MB after modest traffic
  (research §3). Hygiene, not usability; keep on epoch's schedule.

### 2.4 The benchmark suite epoch must ship

Seed it from this repo's harnesses (`docs/research/epoch/bench/`:
`bench_io.py`, `bench_write.py`, `bench_mount.sh` — committed on this chain)
and grow it into `epoch-bench` in the epoch repo, so the numbers are theirs to
regress against:

1. **Micro, per op class** (the table above): read, stat-walk, write+fsync,
   copy-up, rename, unlink — each reported as ×bare-lower *and* absolute.
2. **Macro, envelope-shaped**: (a) a sleep-free test-suite variant (spawn-heavy
   FS churn with writes forced inside the mount); (b) a synthetic stat-storm
   over 10⁴ entries; (c) an editor-atomic-save loop (write-tmp + rename).
3. **Spin-up at scale** — the valid measurement the brief asks for: synthetic
   lower trees at {10³, 10⁴, 10⁵} files × {10 MB, 1 GB, 10 GB}, reporting
   mount→first-read, first readdir of root, and first-write latency. Cold-cache
   caveat stated honestly: macOS has no unprivileged cache drop (`purge` needs
   sudo) — report warm-cache numbers with the caveat, or accept a privileged
   bench profile for cold runs. The lazy-hydration claim (research §5, inferred
   from `DirChildren::Sealed`) becomes measured or dies.
4. **mmap smoke** — sqlite-with-mmap open/query/write inside a mount vs bare
   (one number, per §1.6).
5. **A rule: no sleep-bound suite may appear in a perf claim** (the gate
   lesson, research §6.2).

### 2.5 GB-scale spin-up — what a valid measurement is

Covered by §2.4.3; stated separately because the *decision* it feeds is the
workspace-replacement question (research §5: epoch could wrap a workspace's
tree copy but never the jj landing contract). The measurement is a
prerequisite for that decision, not for the fence/audit use — those work at
this repo's scale today.

---

## 3. Audit / security layer — formalize what the research found

### 3.1 What exists, precisely (re-verified this session)

- **`.vfs/debug/overlay`** is a *live state* listing — every overlay path, one
  per line, tombstones included (`epoch-vfs/src/debug_dir.rs:27,71-74`;
  `DebugSnapshot.overlay_paths`). It is **not** an evented write log: no
  timestamps, no attribution, no history, and it is rendered fresh per read.
  Research §3/§4.1 measured it as a complete changed-path listing readable
  from outside the mount — accurate, but its shape is "current delta," and it
  is named `debug/`, i.e. explicitly not a contract.
- **The timeline** is the durable half: checkpoint events
  `{parents: Vec<EventId>, root: TreeId, timestamp_ms, meta: Option<String>}`
  (`epoch-timeline/src/event.rs:35-54`, verified), append-only, per-head.
  Timestamps are *checkpoint* times (5 ms–500 ms debounce, research §2), not
  per-op times. `meta` is a free-form annotation hook ("author, trigger,
  message") — **currently unused for attribution** and the natural seat for
  it.
- The only per-op, per-instant record is the debug FUSE op log (research §4.2
  mentions the 34 MB log from the rename session) — debug-grade, unbounded,
  not a surface.

### 3.2 The requirement: a stable, documented audit surface

Specify it as a *derived, versioned* view over the timeline, not a new store:

1. **Event attribution.** Every checkpoint event records the identity of the
   mount that produced it — mount id, head name, (optional) user/host — in
   `meta` (schema'd JSON) or a dedicated field. Per-lane attribution is the
   point: the parent maps mount-id → lane at spawn (simply-pi's side, one
   line of bookkeeping).
2. **A write log, derived.** `epoch audit <state-dir> [--head <name>]` (or a
   `.vfs` sibling) yields a machine-readable, schema-versioned log: for each
   consecutive first-parent event pair, the tree diff → entries
   `{ts, head, mount, op ∈ write|delete|rename-pair, path, content-hash}`.
   Rename pairs reconstructed per §1.3. Readable from outside the mount,
   without the mount live (store-side), and while live (current-head tail).
3. **Claims it makes, stated in the docs:** complete for *writes and deletes
   of paths under the mountpoint*; timestamp resolution = checkpoint debounce
   (not per-op); **reads are not recorded at all** (they fall through — audit
   of reads is not this layer's job and must not be implied); writes outside
   the mountpoint are not its job either (§3.3).
4. **What this gives the fence design.** ROLE-WRITE-AUTHORITY's calibration
   loop (fence §10) is *denial*-based: it records what the fence blocked.
   The audit tier records what a lane **did** — including writes a deny-fence
   would have blocked before recording them (research §4.1's point: better
   than denial *as a review surface*). These are complementary instruments
   and the calibration artifact should unify them: `(rule, child, target)`
   denials plus `(mount→lane, path, op)` contained writes in one reviewable
   stream. **This is a change to the fence design's §10, in as many words**
   (see §5.2 below).

### 3.3 Read-deny: where it lives, and where it *cannot* live

The brief asked whether containment-by-construction can deny rather than
capture, and whether epoch should grow a mount-scoped deny list. The answer
has a structural half the research gestured at but didn't carry to its
conclusion:

**A mount can only see paths that traverse it.** `~/Documents` is not under
the mountpoint, so a read of it from inside the mount never crosses epoch's
code — no `DenyVfs` decorator, however strictly checked before lower/overlay
resolution, can deny it. This is not an implementation gap; it is the shape of
a FUSE filesystem. The fence design's `sensitive-read:` list (fence §4, §5.2)
is therefore **permanently the syscall layer's job**, exactly as the research's
verdict leaned (research §4.2.3: "no read-deny mechanism at all … still needs
the syscall layer") — this proposal *upholds* that verdict and hardens it from
"not implemented today" to "not implementable here, ever, for host paths."

A mount-scoped deny list *would* be meaningful for subtrees **of the lower**
(e.g. deny `secrets/**` when several lanes share one lower) — a different,
narrower feature: checked at lookup/open in a decorator wrapping the router,
answering EPERM loudly. **Recommendation: do not build it now.** No current
workflow shares a lower across lanes with secret subtrees; it can be added
later as pure decoration without touching the store. If built: absolute host
paths outside the mountpoint must be *rejected at mount time* as
un-enforceable, not silently accepted — a deny that pretends is worse than
none.

### 3.4 Threat model — what the audit layer honestly claims

Accidents vs adversaries, per the fence design's own framing (fence §6):

- **Against accidents: strong.** Every write under the mountpoint is captured,
  checkpointed, diffable, and revertible (time travel measured, research §3);
  the lower is never mutated (containment verified, research §4.1). This is
  the best accident instrument available to the workflow — better than denial,
  because it records what denial never sees (attempts that "succeeded").
- **Against adversaries: nearly nothing, and epoch must say so in its docs.**
  The overlay is not a sandbox (research §4.2.3): a child inside a mount can
  exec anything, write anywhere on the host, read `~/Documents`, `cd` out and
  operate on the lower directly — the research's jj-escape accident
  (research §4.2.3) demonstrates the *shape* of escape even by accident. A
  malicious child could also `umount` its own mount and then write the lower
  directly. Denial against adversaries stays with the syscall layer (fence
  §5.2) and role authority; **epoch's audit claims are scoped to accidents
  and to review, and its docs must carry that sentence.**
- **The silence cost is real and stays (research §4.2.5):** wrong writes
  succeed from the child's perspective; detection is parent-side and
  after-the-fact. The compensating habit — the parent diffs every lane's
  overlay at every report — becomes cheap and mechanical once §3.2's audit
  log exists. Do **not** "fix" this by failing writes loudly in-band: a fake
  EPERM would destroy the consequence-free-scratch property that is the
  point, and still not be a fence (§3.3).

---

## 4. Open questions, ranked by what each one changes

1. **Does epoch own landing (export CLI), or does each consumer?** Changes who
   owns the lane lifecycle and whether "lane" can mean anything without a
   bespoke exporter. *Recommendation: epoch owns it — `epoch diff`/`epoch
   export` pulled forward ahead of the daemon (§1.3, §5.1). It is store-side,
   small, and the alternative is N drifting 50-line exporters.*
2. **Is silent-success containment acceptable for lanes, or must lanes get
   loud failure?** Changes the whole posture toward epoch (fence-tier vs
   scratch-tier). *Recommendation: accept silence for scratch lanes with the
   mandatory parent-side overlay-diff habit (§3.4); never add in-band loud
   failure. The loud tier is the fence design's, not epoch's.*
3. **Does the audit log become part of the fence design's calibration
   artifact (unified denial + contained-write stream)?** Changes fence §10's
   instrument and the review workflow. *Recommendation: yes — one stream,
   two record types (§3.2.4); without it, "zero denials" can read as clean
   while a lane wrote freely into its overlay, and the two logs drift.*
4. **Roadmap order: does symlink (and the rename fix) jump ahead of the
   daemon?** Changes when this repo can pilot anything. *Recommendation: yes —
   rename-fix and symlink are small, unblock all real toolchains, and are
   prerequisites for even benchmarking the daemon's world honestly (§5.1).*
5. **Does the setattr/mode persistence batch with other storage-format
   changes now, or wait?** Changes whether builds work in-mount sooner vs
   format-churn amortized. *Recommendation: batch with the next
   manifest-format change; until then epoch documents "chmod is a no-op in
   overlays" so agents don't misread EACCES from `cargo run`.*

---

## 5. Where this proposal contradicts the research doc and the fence design

1. **Against the research (§8): the ordering.** Research: rename → symlink →
   daemon → export CLI. This proposal: rename-fix → symlink → **export CLI →
   daemon**. Reason: the daemon is shared-state ergonomics (N independent
   state dirs work today — degraded but functional, research §5), while
   export is the correctness of the lane lifecycle itself; a lane that cannot
   land is not a lane. The daemon remains epoch's biggest unlock, but it is
   not the gate for the first pilot.
2. **Against the research (§4.2.3, implied only): read-deny is not merely
   missing — it is structurally out of scope for host paths.** The research
   correctly measured "no read-deny mechanism at all" but left open the
   impression that epoch *could* grow one. It cannot, for any path outside
   the mountpoint (§3.3). Sharpened from implementation-gap to
   not-its-job-ever.
3. **Against the research's blocker list (implicit): setattr/mode is added as
   P1.** The research never ran a build toolchain inside a mount, so chmod-
   after-write breakage (cargo's exec bit, §1.5) went unhit. Reasoned from
   epoch's own stub citation, not measured — flagged in §1.5.
4. **Against the fence design (§10): the calibration artifact is
   denial-only.** ROLE-WRITE-AUTHORITY's delete/promote signals key on
   denials and observed violations; an epoch audit tier adds a third signal
   — contained writes that *would have been denials* under a fence — and the
   calibration loop should consume it (§3.2.4). Without it, mount-resident
   scratch lanes are invisible to the fence's measurements entirely.
5. **A framing the fence design doesn't yet carry (its §5.3's territory):**
   for mount-resident scratch lanes, the write-path question changes
   *kind*, not degree — under a mount, all writes under the mountpoint are
   contained, so role-write rules stop being runtime denials and become
   review-time policy over the audit log. That is a different philosophy
   than the fence's (deny at the sound layer) and they will coexist
   (scratch lanes vs fenced lanes); the fence design should eventually name
   which lane types are which rather than implying one fence shape for all.

---

## 6. What each fix gives up / costs

| Fix | Gives up / costs |
|---|---|
| rename on macFUSE 5.x (§1.1) | The fix is likely a fuser fork/upgrade (their MOUNTING.md already names it; upstream issue open) — an external dependency epoch must carry or chase. Everything else is confined to the adapter. Nothing semantic is given up; EXDEV-on-lower-dir-rename stays (correct, costs copy+delete). |
| symlink as overlay object (§1.2) | New node body + CAS object kind + snapshot hashing + passthrough parity — real work across four crates, and another storage-format change (batch with §1.5). Cheap passthrough-only variant is refused deliberately (§1.2 rationale). |
| export/diff CLI (§1.3) | A CLI surface to stabilize *before* the daemon reshapes the verbs; schema commitments to early consumers. Kept deliberately minimal (diff + export, no merge) to limit that. |
| daemon + shared store (§1.4) | Their own big lift; until it lands, N-state-dirs is the supported story (dedup forgone). No new cost imposed by this proposal beyond audit-for-all-mounts. |
| setattr persistence (§1.5) | Storage-format churn (manifests gain mode); mtime/atime left best-effort, so `make`-style mtime tooling stays broken in-mount until a later batch. |
| audit surface (§3.2) | The first *stable, versioned contract* in a project whose README says "formats change without migration" — the cost is discipline, scoped small by deriving from the timeline rather than adding a store. Attribution in `meta` commits that field to a schema. |
| read-deny in epoch (§3.3) | **Rejected — cost avoided.** Building it would be a pretend fence for host paths and a maintenance surface for a use case nobody has. |
| performance ceilings (§2.2) | Fast-path work in the fall-through router (O(1) membership check) risks subtle coherence bugs with tombstones/dirty files — the 60×→3× fix is the one item here whose difficulty epoch must size itself; if the structure fights it, the honest outcome is a documented architectural decision, not a quietly lowered ceiling. |

---

## 7. Provenance — verified this session vs. inherited from the research

**Re-verified in epoch's tree (read-only, this session):** rename overlay
semantics implemented (`epoch-worktree/src/lib.rs:873-895`) and the adapter's
rename logging (`fuser_adapter.rs:315-326`); fuser/macFUSE 4.x-protocol
incompatibility pre-documented (`docs/MOUNTING.md:22-26`, incl. the open
upstream issue); the `VfsService` trait's full operation list (no symlink,
link, setattr-mode, xattr, statfs — `epoch-vfs-api/src/lib.rs:250-299`);
xattr stubs (`fuser_adapter.rs:343-371`) and setattr's size-only handling
(`:165-190`); mount options (no writeback/direct_io — `:43-45`); no lock
handler in the adapter; no `deny` anywhere in `epoch-vfs`/`epoch-fuse` (grep);
`TimelineEvent {parents, root, timestamp_ms, meta}` (`epoch-timeline/src/event.rs:35-54`);
`.vfs/debug/overlay` is a live delta listing, no timestamps/attribution
(`debug_dir.rs`); the binary's three verbs (`main.rs` usage); TASKS.md/TIMELINE.md
roadmap placement of every §1 item.

**Inherited from the research (cited, not re-measured):** every number in §2.2
(research §3); rename/symlink/hardlink breakage on this machine (research §3,
§4.2.1-2); containment, tombstones, time travel, audit listing demos (research
§4.1); single-writer store and fork-is-`set_head` (research §5); jj-escape
accident (research §4.2.3); the 23 s gate's sleep-bound character and /tmp
writes (research §3, §6.2).

**Inferred here, flagged:** cargo's chmod-after-write breakage (reasoned from
epoch's own stub, §1.5 — not measured); flock's kernel-side emulation
(succeeds per research's probe + no handler verified; cross-path incoherence
is structural reasoning); the 3×/2× ceilings' achievability (§2.2 — sizing is
epoch's to do).

---

*End. Read in one sitting, ~20 minutes. Decisions asked of Chris: the §4
recommendations, taken or corrected; the ceilings in §2.2 accepted or
renegotiated; the read-deny rejection in §3.3 confirmed.*
