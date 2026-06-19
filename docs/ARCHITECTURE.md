# epoch — Architecture

**Status:** Architecture Draft (for review)
**Supersedes:** `docs/INITIAL.md` (the system design, retained for reference) — this document is the source of truth for the architecture and crate boundaries.
**Authors:** Chris & Solasta (reconciled)

---

## 0. Reading guide

This document builds on the system design in `docs/INITIAL.md`, defines the async-trait crate boundaries, and calls out every architectural decision explicitly. It is organized so that:

- §1–2 give you the *what* and the *crate map*.
- §3–8 give you the *contracts and mechanics* (the async boundaries, storage, VFS, control plane, timeline/GC, cross-cutting concerns).
- §9 gives you the *parallel build plan* — how multiple people build pieces simultaneously against frozen contracts.
- §10–11 capture what is *deferred* and what is *still open*.

Decisions are labelled **D1–D14** and cross-referenced throughout.

---

## 1. Overview & objectives

`epoch` is a high-performance, time-traveling **Virtual Filesystem (VFS)** for autonomous AI agents and developer workflows. It decouples **immutable file state** (Content-Addressable Storage, CAS) from **temporal lineage** (a Directed Acyclic Graph, DAG). By intercepting filesystem operations via FUSE, it provides consequence-free state scrubbing, O(1) recursive forking, and strict blast-radius isolation without altering the underlying physical project.

### Objectives

| Objective | Mechanism |
|---|---|
| **POSIX compliance** | FUSE mount usable by LSPs, compilers, IDEs unchanged. |
| **Automated versioning** | Fine-grained micro-mutations captured automatically; no manual commits. |
| **Instant branching** | O(1) recursive forking for parallel multi-agent workflows (memory-only, zero file copies). |
| **Control API** | Dual-transport control plane: HTTP/REST (UI timeline scrubbing) **and gRPC** (strongly-typed binary access — spine-fetch, blob-fetch). See **D13**. |
| **Dynamic ignore** | Respect `.gitignore`; divert ephemeral build artifacts from persistent history. |
| **Zero-latency I/O** | Storage writes never block the FUSE hot path. |
| **Isolation** | Rogue agents are *physically incapable* of destroying the underlying source (enforced at the type level — see **D12**). |
| **Storage efficiency** | Perfect structural dedup of identical states across all branches (content addressing + Merkle trees). |
| **Crash resilience** | Async write batching makes DB corruption impossible; only the last unpersisted interval is at risk. |

### Component vocabulary (design → crate)

| Design component (INITIAL §3) | Crate(s) | Role |
|---|---|---|
| `agent-vfsd` (global daemon) | `epoch-daemon` (binary) | Long-running service: cache, DBs, background tasks, control plane. |
| `agent-cli` (thin client) | `epoch-cli` (binary) | Ephemeral command client. |
| FUSE Interceptor (data plane) | `epoch-fuse` | Kernel syscalls → engine routing. |
| Side API (control plane) | `epoch-control-api`/`epoch-control-proto` + `epoch-control-http` (REST) + `epoch-control-grpc` (gRPC) | Dual transport; gRPC carries the strongly-typed binary sub-surface — spine-fetch, blob-fetch (**D13**). |
| `.vfs/` control plane | `epoch-fuse` (adapter, **D12**) | Plan-9-style control via file writes. |
| BlobStore / TreeStore / TimelineStore | `epoch-cas-api`, `epoch-timeline-api` (+ backends) | Pluggable storage contracts (BlobStore/TreeStore are specializations of a generic CAS — **D14**). |
| CAS engine, diff | `epoch-cas` (generic) → `epoch-file-cas`, `epoch-tree-cas` | Generic content-addressing; file-content + strongly-typed tree/spine stores build on it; diff lives in `epoch-tree-cas` (**D14**). |
| Timeline DAG, GC | `epoch-timeline`, `epoch-gc` | Lineage + reclamation. |
| Volatile HEAD, overlay, ignore, `AgentFs` | `epoch-vfs` + `epoch-vfs-api` | The engine and its contract. |

---

## 2. Crate workspace map & dependency DAG

A Cargo workspace (virtual root manifest; crates under `crates/`). **21 crates** in four waves. The build **starts with the id-agnostic generic CAS kernel** (`epoch-cas`, **D15**) and its two local adapters (`epoch-cas-redb`, `epoch-cas-moka`); concrete ids + the domain binding come in Wave 1, and the control plane is delayed to the last wave (§9.2). Wave membership is about *build order*, not runtime layering.

### 2.1 The crates

**Wave 0 — Generic CAS kernel + local adapters (id-agnostic; start here):**

| Crate | Responsibility |
|---|---|
| `epoch-cas` | The generic, **id-agnostic** CAS kernel — depends on nothing internal. `SyncCAS<V>` + `AsyncCAS<V>` traits with an associated `Id` (defaults to the kernel's content-address `Hash`), `ByteBackend`/`Codec` traits, `SyncGenericCAS`/`AsyncGenericCAS` engine (BLAKE3, zstd, dedup+verify), the `BatchingCAS` write-coalescing decorator, `Blocking`/`Asyncify` bridges, an in-memory `MemoryByteBackend`, plus `Hash` + `CASError`. Knows nothing of `BlobId`/`TreeId` or `moka` (**D14**, **D15**). |
| `epoch-cas-redb` | redb `SyncByteBackend` (persistent, opaque content-keyed KV) for the kernel; async view via `spawn_blocking`. Depends only on `epoch-cas`. |
| `epoch-cas-moka` | moka-backed caching: the `CachingCAS<S>` decorator (sync `moka::sync` + async `moka::future` variants), id-agnostic. Depends only on `epoch-cas`. |
| `epoch-file-cas` | `FileCAS` = generic CAS over `Bytes` + identity codec (binary file content), content-keyed by `Hash`. Builds only on `epoch-cas`. |
| `epoch-tree-cas` | `TreeCAS` = generic CAS over the `Spine` Merkle codec; owns the `Spine`/`TreeEntry`/`FileMetadata` types (children referenced by `Hash`) + **Merkle build/walk + diff engine**. Builds only on `epoch-cas`. |

**Wave 1 — Domain ids, contracts, backends & FS adapter (the tie-in begins):**

| Crate | Responsibility |
|---|---|
| `epoch-ids` | Semantic id newtypes, introduced *now* to bind the abstract stores to the domain: `BlobId`/`TreeId` (newtypes over the kernel's `Hash`), `EventId`, `SessionId`, `WorkspaceId`, `Ino`, `FileHandle`, `HeadRef`/`HeadKind`, path wrappers. No async, no I/O. |
| `epoch-cas-api` | The **binding/contract** layer (the deferred home for the domain surface): object-safe `BlobStore`/`TreeStore` (+ async variants) bound to `BlobId`/`TreeId`, wrapping `epoch-file-cas`/`epoch-tree-cas`; `Arc<dyn _>` blanket impls; object-safety tests. Lets `epoch-vfs`/`epoch-gc` hold `Arc<dyn BlobStore>` over domain ids. |
| `epoch-timeline-api` | `TimelineStore` async trait; `TimelineEvent`/`TimelineHead`/`HeadKind`; `SnapshotRequest`/`DiffRequest`; `TimelineError`. |
| `epoch-vfs-api` | VFS capability traits `VfsMetadata`/`VfsRead`/`VfsWrite` + aggregate `AgentFs`; `VfsRouter` policy trait; `Attr`/`DirEntry`/`OpenFileHandle`/`WriteDisposition`/`ResolvedNode`; `FsError`+`to_errno()`; `MockAgentFs`. |
| `epoch-testkit` | Conformance suites for the domain-bound stores + timeline + `AgentFs`, with `Memory*` reference impls. (The kernel ships its own `MemoryByteBackend` + unit tests in Wave 0.) Dev-only. |
| `epoch-timeline` | DAG logic: heads, tags, O(1) fork, GC **mark** (reachable spine `TreeId`s). |
| `epoch-timeline-sqlite` | rusqlite backend for `TimelineStore` (dedicated-thread actor). |
| `epoch-fuse` | `fuser` ⇄ `AgentFs` adapter: sync hot path, sync cache, async bridge, `.cas/` EROFS, `.vfs/ctl` hook. |

**Wave 2 — Engine integration (CAS × timeline join):**

| Crate | Responsibility |
|---|---|
| `epoch-gc` | Tracing mark/sweep spanning timeline reachability → CAS tree/blob walk → purge. |
| `epoch-vfs` | Real `AgentFs` engine: volatile HEAD, 3-tier overlay, ignore routing, batching wiring. |
| `epoch-daemon` | Daemon binary `agent-vfsd` (initial): compose backends + CAS (sync local / async remote), mount FUSE, run GC/flush loops. REST + gRPC serving is added in Wave 3. |

**Wave 3 — Control plane (delayed; see §9.2):**

| Crate | Responsibility |
|---|---|
| `epoch-control-api` | **Transport-agnostic** protocol model: `ControlService` (timeline, diff, snapshot, flush, fork, scrub, mount, quarantine) + `DataService` (spine-fetch, blob-fetch) traits; domain DTOs; control error model. No server, no wire codec (**D13**). |
| `epoch-control-proto` | The **gRPC/protobuf contract**: `.proto` + prost/tonic-generated strongly-typed messages & stubs; conversions to/from `epoch-control-api` domain types (**D13**). |
| `epoch-control-http` | axum **REST** adapter over `ControlService`/`DataService` (JSON; UI timeline scrubbing). |
| `epoch-control-grpc` | tonic **gRPC** adapter using `epoch-control-proto`; strongly-typed binary plane (spine-fetch, streamed blob-fetch). |
| `epoch-cli` | Thin client binary `agent-cli` (gRPC via `epoch-control-proto`, REST fallback); the daemon gains REST + gRPC serving here. |

### 2.2 Dependency DAG

Layered top-down; every crate depends only on layers above it.

Two independent roots — the **id-agnostic CAS kernel** and the **domain ids** — meet only at the binding layer.

```
Kernel root (no internal deps, keyed by content Hash):
   epoch-cas        (SyncCAS/AsyncCAS + Id assoc, ByteBackend, Codec, GenericCAS, BatchingCAS, bridges, in-mem, Hash, CASError)
     ├── epoch-cas-redb   (SyncByteBackend — persistent redb KV)
     ├── epoch-cas-moka   (CachingCAS — moka cache decorator)
     ├── epoch-file-cas   (FileCAS — binary, Hash-keyed)
     └── epoch-tree-cas   (TreeCAS — Spine + Merkle codec + diff, Hash-keyed)

Domain-id root (independent of the kernel):
   epoch-ids        (BlobId/TreeId over Hash; EventId, SessionId, Ino, FileHandle, HeadRef, paths)

Contracts (bind kernel × ids):
   epoch-cas-api ─► (file-cas + tree-cas) + epoch-ids   (dyn BlobStore/TreeStore on domain ids)
   epoch-timeline-api ─► epoch-ids        epoch-vfs-api ─► epoch-ids
   [epoch-control-api ── epoch-control-proto]           (Wave 3 — delayed)

Backends & adapters:
   epoch-timeline ─► timeline-api          epoch-timeline-sqlite ─► timeline-api
   epoch-fuse ─► vfs-api
   [epoch-control-http / -grpc ─► control-api(+proto)   epoch-cli ─► control-api+proto]   (Wave 3)

Join:        epoch-gc  ─► cas-api + tree-cas + timeline           (mark/sweep — the only cas×timeline crate)
             epoch-vfs ─► cas-api + tree-cas + timeline + vfs-api (the real AgentFs engine)

Binary:      epoch-daemon   (composes all: backends + CAS, FUSE mount, REST + gRPC servers, GC/flush)

Dev-only:    epoch-testkit ─► cas-api, timeline-api, vfs-api   (conformance suites + Memory* impls)
```

The control-plane crates (`epoch-control-api`/`-proto`/`-http`/`-grpc`, `epoch-cli`) sit at the layers shown, but are **built last** (Wave 3, delayed — §9.2).

### 2.3 Acyclicity invariants (enforced by a dependency-lint, e.g. `cargo-deny` ban-list)

- **`epoch-cas` is id-agnostic** — it depends on *nothing internal* (not even `epoch-ids`); it is keyed by content `Hash` and is the build's starting point. Domain ids (`BlobId`/`TreeId`) enter only at `epoch-cas-api` (**D15**).
- **CAS family ⊥ `epoch-timeline`** — no `epoch-cas`/`epoch-file-cas`/`epoch-tree-cas` depends on timeline and vice-versa (**D3**). They meet only in `epoch-gc` and `epoch-vfs`.
- **`epoch-file-cas` ⊥ `epoch-tree-cas`** — the two specializations don't depend on each other; both build only on the generic `epoch-cas` (**D14**).
- **The local CAS adapters depend on the kernel only** — `epoch-cas-redb` (a `SyncByteBackend`) and `epoch-cas-moka` (a `CachingCAS` decorator) build on `epoch-cas` and never touch a specialization, `epoch-ids`, or an engine. Backends store opaque content-keyed bytes with zero CAS/tree semantics (**D4**, **D14**).
- **Transport adapters depend on `epoch-control-api` (+ `-proto` for gRPC) only**, never on the engine; the daemon injects the `ControlService`/`DataService` impl (**D13**).
- **No `*-api` or implementation crate depends on a binary** (`epoch-daemon`/`epoch-cli`).
- **`epoch-vfs-api` knows nothing about storage** — it depends only on `epoch-ids`. This decouples every adapter (FUSE/HTTP/gRPC) from the storage stack until the daemon links them (**D12**).

---

## 3. Async trait boundary

### 3.1 D1 — Sync FUSE hot path, async stores (the load-bearing seam)

This is the central architectural decision and the reason the storage traits are async at all.

`fuser`'s `Filesystem` callbacks are **synchronous** and run on fuser's **own OS threads** (not tokio workers). We deliberately keep the hot path synchronous and bridge to async **only when we must touch the stores**:

- **Synchronous cache at the boundary.** A `moka::sync::Cache` sits right at the FUSE layer. Cache **hits never touch the async runtime** — they return immediately, no executor round-trip, no `block_on`.
- **Volatile-HEAD writes are answered synchronously in RAM.** A `write` updates a `parking_lot`-guarded in-memory buffer and returns instantly (`Ok`). It does **not** cross into async land at all (see §6.1). This is the true "zero-latency I/O" path.
- **`block_on` is the bridge, only on miss.** For ops that genuinely need the stores (cold `read`, `lookup`, `readdir` of persisted trees, persistence), the FUSE adapter packages the request and crosses into the tokio runtime — via a **channel + dedicated async worker** (§3.3), not a raw `Handle::block_on` inside the callback.
- **The store traits stay async on purpose.** `BlobStore`/`TreeStore`/`TimelineStore` are async even though today's local backends (redb/rusqlite) are embedded — *specifically so the same contracts can later be satisfied by remote/cloud backends* (S3, R2, Redis, Postgres) **without the FUSE layer ever knowing or changing.**

The seam in one line:

```
sync hot path (FUSE callbacks + sync moka cache + in-RAM volatile HEAD)
        │   only on store-touching miss
        ▼
   bridge (bounded channel → spawned async tasks on tokio)
        │
        ▼
async store traits  ──►  local today (redb / rusqlite)
                    └──►  remote tomorrow (S3 / R2 / Redis / Postgres)
```

The sync cache is what makes this cheap: it minimizes how often we cross the seam.

### 3.2 D2 — Dispatch style: `#[async_trait]`, object-safe, `Arc<dyn>`-swappable

All boundary traits use **`#[async_trait]`** (plain `async fn` desugaring to `Pin<Box<dyn Future + Send + 'a>>`). Rationale:

- The design requires **runtime-selected backends** (`Arc<dyn BlobStore>`). Native `async fn`-in-traits (AFIT) is not dyn-compatible without `trait-variant` boilerplate that buys nothing here.
- Every store call is I/O-bound (disk seek or network round-trip), so one box allocation per call (~tens of ns) is noise.
- `#[async_trait]` bakes in the `+ Send` bound on the returned future, sidestepping AFIT's unconstrained-`Send` problem for futures spawned on the multi-thread runtime and crossed between fuser and runtime threads.

Rules:

- Every boundary trait is `Send + Sync + 'static`. **Never** `#[async_trait(?Send)]` — futures cross threads.
- Ship an `impl BlobStore for Arc<dyn BlobStore>` **blanket impl** so compile-time decorator stacks compose with runtime-selected backends (see §4.4).

> If we later want the foundational API crates to be proc-macro-free, the hand-written `BoxFuture` form is ABI-identical to what `#[async_trait]` generates and can replace the macro per-crate without touching impls. We start with the macro for readability.

### 3.3 The FUSE sync→async bridge (deadlock-safe)

We use **channel + dedicated async worker**, not `Handle::block_on` directly in the callback:

```rust
// epoch-fuse — inside a fuser callback (a fuser thread, NOT a runtime thread)
fn read(&mut self, _req: &Request, ino: u64, offset: i64, size: u32, /*…*/ reply: ReplyData) {
    if let Some(bytes) = self.cache.get(&key) {     // sync moka — no runtime round-trip
        return reply.data(&bytes);
    }
    let (tx, rx) = oneshot::channel();
    // hand work to the async side; bounded channel applies backpressure
    self.dispatch.blocking_send(FsOp::Read { ino, offset, size, reply: tx }).ok();
    match rx.blocking_recv() {                       // fuser thread parks here
        Ok(Ok(bytes)) => reply.data(&bytes),
        Ok(Err(e))    => reply.error(e.to_errno()),
        Err(_)        => reply.error(libc::EIO),     // worker dropped → Shutdown
    }
}
```

On the runtime side a dispatcher **spawns each op as a tokio task**, so concurrent FUSE ops run concurrently on the runtime (heavy async I/O — e.g. S3 round-trips — happens on runtime workers, not pinned to a fuser thread's stack). Why this beats raw `block_on`:

- `block_on` *from a fuser thread* is technically legal (fuser threads aren't runtime threads), but it pins a fixed-size fuser thread for the entire async call tree; under parallel multi-agent load you exhaust fuser's dispatch threads while tokio sits idle.
- The channel design keeps a hard, auditable invariant: **fuser callbacks touch only channels, never the runtime directly** — so the "no `block_on` on a runtime thread" panic can never be introduced as the code evolves.
- It gives us explicit **backpressure** (bounded channel, **D9**) and clean **shutdown** (dropped `oneshot` → `Shutdown` → `EIO`).

---

## 4. Storage contracts

### 4.1 D14 — Generic CAS, sync + async, with typed specializations

The content-addressed store is **one generic, reusable, id-agnostic abstraction** with two trait variants (sync and async), specialized into a binary **file** store and a strongly-typed **tree/spine** store. It is the single most reused piece in the system and the **first thing built** (Wave 0): it depends on nothing internal, is keyed by its own content-address `Hash`, and learns about domain ids (`BlobId`/`TreeId`) only later through the binding layer (`epoch-cas-api`, **D15**).

**The generic CAS trait — sync and async (`epoch-cas`).** Both exist *on purpose*: the sync variant lets the synchronous FUSE hot path (**D1**) talk to a local CAS with **zero runtime involvement** (no `block_on`, no executor); the async variant supports remote/cloud backends.

```rust
// Sync — embedded/local backends; consumed directly by the sync FUSE hot path
pub trait SyncCAS<V>: Send + Sync + 'static {
    type Id;
    fn put(&self, value: V) -> Result<Self::Id, CASError>;          // Id = BLAKE3(canonical bytes)
    fn get(&self, id: &Self::Id) -> Result<Option<V>, CASError>;
    fn has(&self, id: &Self::Id) -> Result<bool, CASError>;
    fn delete(&self, id: &Self::Id) -> Result<(), CASError>;        // GC sweep
    fn iter_ids(&self) -> Box<dyn Iterator<Item = Result<Self::Id, CASError>> + '_>;  // GC mark
}

// Async — remote/cloud backends (S3, R2, Redis, Postgres); same surface, awaited
#[async_trait] pub trait AsyncCAS<V>: Send + Sync + 'static {
    type Id;
    async fn put(&self, value: V) -> Result<Self::Id, CASError>;
    async fn get(&self, id: &Self::Id) -> Result<Option<V>, CASError>;
    async fn has(&self, id: &Self::Id) -> Result<bool, CASError>;
    async fn delete(&self, id: &Self::Id) -> Result<(), CASError>;
    fn iter_ids(&self) -> BoxStream<'_, Result<Self::Id, CASError>>;
}
```

**The opaque byte backend (`epoch-cas`).** What `redb`/S3 actually implement — a dumb content-keyed KV with **zero CAS/tree semantics** (it never hashes, compresses, or parses). Sync for embedded, async for network:

```rust
pub trait SyncByteBackend: Send + Sync + 'static {           // redb implements this
    fn get(&self, key: &Hash) -> Result<Option<Bytes>, CASError>;
    fn put(&self, key: &Hash, bytes: Bytes) -> Result<(), CASError>;
    fn has(&self, key: &Hash) -> Result<bool, CASError>;
    fn delete(&self, key: &Hash) -> Result<(), CASError>;
    fn iter_keys(&self) -> Box<dyn Iterator<Item = Result<Hash, CASError>> + '_>;
}
#[async_trait] pub trait AsyncByteBackend: Send + Sync + 'static { /* same, awaited — S3/R2/Redis */ }
```

**Typed specializations (`epoch-cas-api`, Wave 1).** These bind the kernel to domain ids and arrive only once `epoch-ids` exists. Fixing `V` and `Id` makes each store object-safe (`dyn BlobStore`) and gives the consumer the right value type — raw bytes for files, a structured `Spine` for trees (`BlobId`/`TreeId` are newtypes over the kernel's `Hash`):

```rust
pub trait BlobStore:      SyncCAS<Bytes, Id = BlobId> {}      // file content — binary
pub trait AsyncBlobStore: AsyncCAS<Bytes, Id = BlobId> {}
pub trait TreeStore:      SyncCAS<Spine, Id = TreeId> {}      // strongly-typed tree/spine
pub trait AsyncTreeStore: AsyncCAS<Spine, Id = TreeId> {}
```

**The generic engine (`epoch-cas`).** `SyncGenericCAS<B, C>` / `AsyncGenericCAS<B, C>` implement `SyncCAS<V>` / `AsyncCAS<V>` over a byte backend `B` and a `Codec<V>` `C`: `encode → zstd → BLAKE3 → backend.put`, reversed and **integrity-verified** on read. The id is `BLAKE3(canonical uncompressed encoding)`, so it's stable regardless of compression level. `epoch-file-cas` instantiates it with an identity codec (`V = Bytes`); `epoch-tree-cas` with the Merkle `Spine` codec. Both are content-keyed by `Hash`; the `BlobId`/`TreeId` domain binding is added later in `epoch-cas-api`.

**Sync ↔ async bridges (`epoch-cas`).** Two adapters let either world consume the other, chosen at composition time in the daemon:

```rust
pub struct Blocking<A> { inner: A, handle: tokio::runtime::Handle }  // AsyncCAS → SyncCAS (drives via block_on)
pub struct Asyncify<S> { inner: Arc<S> }                             // SyncCAS → AsyncCAS (offloads via spawn_blocking)
```

So the **local default is fully sync on the hot path** (FUSE → `SyncCAS` → redb `SyncByteBackend`, no runtime); a **remote backend** is reached by wrapping its `AsyncCAS` in `Blocking` at the FUSE bridge (**D1**); and async consumers (gRPC blob-fetch, background flush) drive a local sync CAS via `Asyncify`.

The timeline store stays async-only (its appends funnel through the sqlite actor, §4.3):

```rust
// epoch-timeline-api
#[async_trait] pub trait TimelineStore: Send + Sync + 'static {
    async fn append_event(&self, event: TimelineEvent) -> Result<EventId, TimelineError>;
    async fn get_event(&self, id: &EventId) -> Result<Option<TimelineEvent>, TimelineError>;
    async fn parents(&self, id: &EventId) -> Result<Vec<EventId>, TimelineError>;
    async fn set_head(&self, head: &HeadRef, target: EventId) -> Result<(), TimelineError>;
    async fn get_head(&self, head: &HeadRef) -> Result<Option<EventId>, TimelineError>;
    async fn list_heads(&self) -> Result<Vec<(HeadRef, EventId)>, TimelineError>;
    fn iter_all_events(&self) -> BoxStream<'_, Result<EventId, TimelineError>>;        // GC
}
```

### 4.2 D4 — Opaque bytes & codec ownership

The decoupling that makes **D14** sound, and the reason the byte backend is semantics-free:

- **The canonical, versioned `Spine`/`TreeEntry`/`FileMetadata` types + their `Codec` live in `epoch-tree-cas`** (`encode()`/`decode()` with a version header) — that's why "shape the `Spine` object" is its own build step (§9.2, Wave 0 step 3). The encoding is a *domain invariant* — it determines the BLAKE3 hash, which determines CAS convergence — so it has exactly one source of truth.
- **The byte backend stores opaque bytes keyed by `Hash`.** `epoch-cas-redb` is a dumb KV: it never parses a `Spine`, never knows what a `TreeEntry` is. It implements `SyncByteBackend` and depends on the kernel `epoch-cas` for the trait + `Hash` only — it **never calls the codec**.
- **The generic engine applies the codec; `epoch-tree-cas` owns the `Spine` codec instance** — `Spine ↔ bytes`, then BLAKE3 + zstd in `epoch-cas`, then opaque bytes to the backend. `epoch-file-cas` uses the identity codec (file bytes are already canonical).

Consequences: one format definition (no divergence → dedup stays correct across all backends and both store variants); the byte backend can't corrupt tree semantics; a version byte enables future migrations; and a `cas → backend → cas` cycle is structurally impossible.

Spines are sorted (`BTreeMap` semantics) for deterministic convergence. We do **not** let backends range-scan or partial-read — that would force tree semantics into the backend; structural queries are `epoch-tree-cas` reconstructing typed spines in memory.

### 4.3 D7 — Blocking embedded backends behind an async trait

redb and rusqlite have **blocking** APIs. A blocking call inside an `async fn` body would stall a tokio worker. Patterns:

- **`epoch-cas-redb`:** `redb::Database` is `Send + Sync` (MVCC). It implements **`SyncByteBackend`** natively — so the local sync hot path (FUSE → `SyncCAS` → redb) needs **no `spawn_blocking` at all**. When the daemon needs an *async* view (e.g. for a background flush task), it wraps the sync backend via `Asyncify`/`spawn_blocking`. Writes are funneled through `BatchingCAS` (§4.4) so each batch is a single write transaction, avoiding "spawn_blocking per tiny write."
- **`epoch-timeline-sqlite`:** `rusqlite::Connection` is **`!Sync`** and single-threaded. It uses a **dedicated-thread actor**: one OS thread owns the `Connection` for its lifetime; the async trait methods send command enums over an `mpsc` channel, each carrying a `oneshot` reply.

```rust
enum TimelineCmd { Append { event: TimelineEvent, reply: oneshot::Sender<Result<EventId, TimelineError>> }, /*…*/ }

#[async_trait] impl TimelineStore for SqliteTimeline {
    async fn append_event(&self, event: TimelineEvent) -> Result<EventId, TimelineError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(TimelineCmd::Append { event, reply }).await.map_err(|_| TimelineError::Shutdown)?;
        rx.await.map_err(|_| TimelineError::Shutdown)?
    }
}
```

The actor fits SQLite's single-writer model naturally (serializes appends, coalesces them into one transaction, caches prepared statements on the one connection) and is the reference pattern other embedded blocking backends copy.

> **Lint rule:** no `redb::`/`rusqlite::` calls inside an `async fn` body — only inside `spawn_blocking` closures or the actor thread.

### 4.4 D8 — Caching & batching as generic CAS decorators

Both cross-cutting behaviors are **generic decorators that implement the same CAS trait they wrap**, so they nest and apply equally to the file and tree stores. They live in separate crates so the kernel stays `moka`-free: **`BatchingCAS<S>` ships in `epoch-cas`** (kernel), **`CachingCAS<S>` ships in `epoch-cas-moka`**. The sync `CachingCAS` uses `moka::sync` (it sits on the sync hot path, **D1**); the async one uses `moka::future`. Composed by the daemon, innermost-out:

```
CachingCAS< BatchingCAS< SyncGenericCAS< RedbByteBackend > > >
  (epoch-cas-moka,       (epoch-cas,           (file-cas / tree-cas
   moka::sync, outermost)  coalesce writes)      over epoch-cas-redb)
```

- **`CachingCAS<S>`** (`epoch-cas-moka`) short-circuits reads before the batch queue; populates on read-miss and on write.
- **`BatchingCAS<S>`** (`epoch-cas`) drains a bounded queue and flushes in bulk on a timer or `>500 MB` watermark. Because `put` is content-addressed, it returns `Id = BLAKE3(bytes)` *immediately* and lets the bytes ride the queue — this is what lets the persistence path return instantly.
- **Composition:** the `Arc<dyn _>` blanket impls (**D2**) let a decorator wrap either a concrete store or a runtime-selected `Arc<dyn BlobStore>`/`Arc<dyn AsyncBlobStore>`, so the daemon picks the innermost backend from config and still stacks decorators statically.

Two coherence rules:
1. **GC `delete` must invalidate the cache** — immutable content-addressed values have exactly one mutation (deletion), so this is the only coherence concern.
2. **Read-your-writes is the engine's job, not the decorators'.** A read of just-written, not-yet-flushed data is served by the volatile HEAD (§6.1), never by reaching into the batch queue. Keeping it out of the decorators avoids reinventing a read index inside `BatchingCAS`.

---

## 5. VFS boundary (refined) — D12

This section refines the single filesystem trait into narrow, composable capability traits.

### 5.1 Narrow capability traits + aggregate

Instead of one fat filesystem trait, `epoch-vfs-api` decomposes the surface into **capability traits**, with an aggregate for the read-write adapter:

```rust
#[async_trait] pub trait VfsMetadata: Send + Sync + 'static {
    async fn lookup(&self, parent: Ino, name: OsString) -> Result<Attr, FsError>;
    async fn getattr(&self, ino: Ino) -> Result<Attr, FsError>;
    async fn readdir(&self, ino: Ino, offset: u64) -> Result<Vec<DirEntry>, FsError>;
    async fn readlink(&self, ino: Ino) -> Result<VfsPath, FsError>;
}
#[async_trait] pub trait VfsRead: Send + Sync + 'static {
    async fn open(&self, ino: Ino, flags: i32) -> Result<OpenFileHandle, FsError>;
    async fn read(&self, ino: Ino, offset: u64, size: u32) -> Result<Bytes, FsError>;
    async fn release(&self, ino: Ino, fh: FileHandle) -> Result<(), FsError>;
}
#[async_trait] pub trait VfsWrite: Send + Sync + 'static {
    async fn create(&self, parent: Ino, name: OsString, mode: u32) -> Result<(Ino, FileHandle, Attr), FsError>;
    async fn write(&self, ino: Ino, offset: u64, data: Bytes) -> Result<u32, FsError>;  // volatile HEAD
    async fn mkdir(&self, parent: Ino, name: OsString, mode: u32) -> Result<Attr, FsError>;
    async fn unlink(&self, parent: Ino, name: OsString) -> Result<(), FsError>;
    async fn rename(&self, p: Ino, n: OsString, np: Ino, nn: OsString) -> Result<(), FsError>;
    async fn flush(&self) -> Result<SnapshotId, FsError>;                 // /flush, /snapshot, SIGTERM drain
}

/// The aggregate the read-write FUSE adapter binds to.
pub trait AgentFs: VfsMetadata + VfsRead + VfsWrite {}
impl<T: VfsMetadata + VfsRead + VfsWrite> AgentFs for T {}
```

### 5.2 Type-level blast-radius isolation

Read-only mounts bind to **`Arc<dyn VfsMetadata + VfsRead>`** — they have *no `VfsWrite` in scope* and are therefore **physically incapable of writing, enforced at compile time**. This is how the architecture satisfies, structurally rather than by runtime check:

- INITIAL §2 "rogue agents must be physically incapable of permanently destroying the underlying source code."
- The `.cas/` history portal's **EROFS** guarantee (read-only by construction).
- **Autopsy / Quarantine** sessions: a failed agent's lineage is mounted read-only for safe backward scrubbing.

### 5.3 `VfsRouter` — the 3-tier overlay as a policy trait

The 3-tier routing decision is a trait so it is unit-testable in isolation from FUSE and swappable:

```rust
pub enum RouteDecision { Scratchpad, CasSpine, LowerDir, Erofs }
pub trait VfsRouter: Send + Sync {
    fn route(&self, path: &VfsPath, op: AccessKind) -> RouteDecision;
}
```

Implemented in `epoch-vfs` over the `ignore` crate, backed by an **LRU path cache invalidated when the `.gitignore` CAS hash mutates**. The three tiers (INITIAL §6.3):

- **Tier 1 — Scratchpad (ignored):** `node_modules/`, build artifacts → temporary physical host dir; bypasses CAS to prevent DB bloat.
- **Tier 2 — CAS Spine (tracked):** writes → volatile HEAD / CAS.
- **Tier 3 — Lower Dir (untouched):** read-only fallback to the pristine physical project.

Retroactively-ignored files stay frozen in historical spines; future mutations route to the scratchpad to keep the hot path O(1).

### 5.4 `.vfs/` control interface is an adapter, not engine internals

The Plan-9-style control plane (INITIAL §9 V2; `echo "snapshot" > /.vfs/ctl/trigger`) is a **thin adapter in `epoch-fuse`** that re-encodes writes to `.vfs/ctl/*` into `epoch-control-api` commands — the same `ControlService` the REST and gRPC planes use (**D13**). The route hook exists from day one (it's just a `RouteDecision` branch); the command surface lights up incrementally. This unifies the control and data planes without leaking daemon state into the engine.

### 5.5 Adapter-agnostic core

Because `AgentFs` lives in its own crate (`epoch-vfs-api`) and is FUSE-independent, the same call-in is later satisfiable by **`pyo3` / `napi` / WASM / container bind-mount / direct-library** adapters (INITIAL §9). `epoch-fuse` is simply the first adapter; nothing about the engine assumes a kernel mount.

### 5.6 The magic `.cas/` directory

An O(1) direct-access portal inside the workspace for agents to read historical states by hash. `readdir` is **blocked** (prevents context-window blowout); `lookup` and `read` are permitted under strict **EROFS** enforcement. Implemented in `epoch-fuse` as a synthetic subtree backed by `VfsRead`+`VfsMetadata` only (§5.2).

---

## 6. Engine mechanics

### 6.1 Volatile HEAD & flush-driven persistence (debouncing)

To avoid storage bloat from hyperactive IDE auto-saves or agent loops, micro-mutations are **not** written to disk continuously:

- FUSE `write`/`release` updates an in-RAM **Volatile HEAD** (a `BytesMut` buffer + a Tree entry, guarded by `parking_lot`). Intermediate states overwrite each other in memory — answered synchronously, returns instantly (**D1**).
- Only on an explicit trigger (`/flush`, `/snapshot`, or a macroscopic timer) are the *surviving* states hashed (BLAKE3), compressed (zstd), and persisted to the CAS.
- **Trade-off:** eliminates junk-blob I/O and slashes GC load, at the cost of losing sub-interval micro-keystrokes in a hard crash.

### 6.2 Asynchronous write batching

For states that *do* persist, POSIX durability is relaxed in exchange for zero FUSE latency (§4.4): data drops into a bounded concurrent queue, FUSE returns `Ok` immediately, and a background tokio task commits in bulk on a timer or `>500 MB` watermark. A power-loss crash loses the last few seconds of buffer, but **DB corruption is mathematically impossible** (every commit is one atomic transaction).

### 6.3 Embedded metadata

File metadata (mode, permissions) is embedded directly in Tree objects. `chmod` cascades into a new root Tree hash, but FUSE `getattr`/`ls -l` are O(1) reads with no N+1 query latency.

---

## 7. Control plane — D13: dual transport (REST + gRPC)

The Side API offers **both HTTP/REST and gRPC** over a single transport-agnostic service core. REST is the human/UI-friendly channel; **gRPC is the strongly-typed binary channel** for access that REST encodes poorly — structured spine objects and raw file content.

> **Build order:** the control plane is **delayed to Wave 3** (§9.2). Until then, the FUSE mount + `.vfs/ctl` hook (§5.4) are the control surface — the design below is what gets built last, not first.

### 7.1 Transport-agnostic core

- **`epoch-control-api`** defines the protocol model once, with **no wire codec and no server**:
  - `ControlService` — `timeline`, `diff`, `snapshot`, `flush`, `fork`, `scrub`, `mount`, `quarantine` + event streams. Control operations; REST-friendly.
  - `DataService` — the **binary/strongly-typed plane**: `fetch_spine(TreeId) -> Spine` (a complex structured Merkle object: filename→entry map with embedded metadata + child hashes) and `fetch_blob(BlobId) -> stream<BlobChunk>` (server-streamed file content, BLAKE3-verified).
  - Domain DTOs + the control error model. Both transports and the CLI consume the same core, so they cannot diverge (DRY).
- **`epoch-control-proto`** is the **protobuf/gRPC contract** — `.proto` schema + prost/tonic-generated messages and service stubs, plus `From`/`TryFrom` conversions to/from the `epoch-control-api` domain types. Protobuf gives schema-checked, versioned, **cross-language strong typing**, which is exactly what the spine/blob surfaces need (and what enables non-Rust fleet members). The `Spine` message is the strongly-typed home INITIAL §10's "open timeline schema" question resolves into.

### 7.2 The two adapters (both thin, both Wave 1)

- **`epoch-control-http`** (axum) maps REST routes (`/timeline`, `/diff`, `/snapshot`, …) to `ControlService`/`DataService` over JSON. `DataService` is reachable here too, but blobs come back as `application/octet-stream` and spines as JSON — fine for UIs, clunky for typed/binary fleet access.
- **`epoch-control-grpc`** (tonic) maps the gRPC services to the same core using `epoch-control-proto`. This is the **primary channel for `DataService`**: `fetch_spine` returns a typed protobuf `Spine`; `fetch_blob` is server-streaming for large files with id-verified chunks.

Both adapters are pure translation — they hold **no engine state**; the daemon injects the `ControlService`/`DataService` implementation. Both can run fully against `MockAgentFs`/canned data before the real engine exists, and the daemon hosts them side by side (separate listeners, or one port via a tower service multiplexer).

> The `fetch_blob` gRPC surface and the local `.cas/` FUSE portal (§5.6) expose the same CAS to different consumers: `.cas/` for agents on the mount, gRPC for remote/strongly-typed programmatic access (e.g. distributed fleets — §11.1). Both are read-only.

### 7.3 The CLI

**`epoch-cli`** (`agent-cli`) is a featherweight client. It speaks **gRPC by default** (via `epoch-control-proto`) for typed commands and streaming, with a REST fallback. It depends only on `epoch-control-api` + `epoch-control-proto`, staying a fast ephemeral binary.

---

## 8. Timeline & garbage collection

### 8.1 The DAG

`epoch-timeline` manages the temporal multiverse:

- **Timeline_Events (nodes):** map an event → root tree spine `TreeId` + parent `EventId`.
- **Timeline_Heads (pointers):** volatile session anchors and stable tags (`v1.0-stable`).
- **O(1) recursive fork:** a memory operation requiring zero file copies; a shared LRU prevents duplicated RAM when parallel agents read the same files.

Timeline references spine roots as **opaque `TreeId`** values — it stores them but never dereferences them, which is why **`epoch-timeline ⊥ epoch-cas`** (**D3**).

### 8.2 D3 — Why GC is its own crate

The tracing GC is the *only* operation that legitimately spans both subsystems, so it gets its own crate, `epoch-gc`, which is the one place allowed to depend on both the CAS stores (`epoch-file-cas` + `epoch-tree-cas`) and `epoch-timeline`:

1. **Mark** (timeline-owned): trace parent pointers from all active heads + stable tags, collecting reachable spine `TreeId`s. **The mark set must also include live volatile-HEAD roots** (in-flight sessions), or GC could delete a blob a running agent still references.
2. **Sweep** (CAS-owned): walk reachable spines in the `TreeStore` to enumerate required blobs; purge any `BlobId`/`TreeId` in the CAS not in the reachable set; prune unreachable timeline nodes.

The **diff engine lives in `epoch-tree-cas`** (it compares two `TreeId` spine roots, pruning identical subtrees, descending only into differing blobs — it needs the typed `Spine` walk). The timeline/control layer supplies *which two roots* to diff by resolving `EventId → TreeId`; the tree-CAS does the walk.

---

## 9. Build plan — waves, ordering & the delayed control plane

### 9.1 The real risk: contract churn, not topology

Parallelism holds **only if each wave's contracts are frozen before the next leans on them**. The first is the **generic CAS kernel** (`epoch-cas`'s `SyncCAS`/`AsyncCAS`/`ByteBackend` traits) — frozen in Wave 0, id-agnostic. The `Spine` type/codec freezes mid-Wave-0 (CAS step 3). The domain contracts — `epoch-ids`, the `epoch-cas-api` binding, `epoch-timeline-api`, `epoch-vfs-api` (incl. `.cas/` + inode semantics) + conformance kit — freeze in Wave 1, once the kernel exists. The control-plane contracts (`epoch-control-api` + `epoch-control-proto` `.proto`) freeze only when **Wave 3** begins (control plane delayed). Whoever owns the contract of the moment owns velocity; treat any post-freeze signature or `.proto` change as a coordinated event.

Two guardrails make the boundaries hard to violate accidentally:
- **Object-safety compile tests** in each contract crate: `fn accepts(_: Arc<dyn BlobStore>) {}`, `fn accepts_ro(_: Arc<dyn VfsMetadata + VfsRead>) {}`.
- **Dependency-lint** (`cargo-deny` ban-list or a small xtask) encoding the §2.3 invariants.

### 9.2 The waves (recommended order; kernel-first, control plane delayed)

The DAG permits broad parallelism, but the **recommended order** builds the genuinely-reusable, id-agnostic CAS kernel first, ties in concrete ids + contracts next, reaches a working filesystem, and **defers the side API to the end**:

- **Wave 0 — Generic CAS kernel (the foundation; start here).** Build the id-agnostic kernel in this **strict order**, each step on the last:
  1. **`epoch-cas` generic engine + in-memory impl** — `SyncGenericCAS`/`AsyncGenericCAS` over a `MemoryByteBackend`; a fully working, id-agnostic in-memory CAS to build and test everything else against. Depends on nothing internal.
  2. **`epoch-file-cas`** — `FileCAS` (binary, identity codec), built on `epoch-cas`.
  3. **Shape the `Spine` object** — design the `Spine`/`TreeEntry`/`FileMetadata` type + Merkle codec (it lives in `epoch-tree-cas`).
  4. **`epoch-tree-cas`** — `TreeCAS` (+ Merkle build/walk + diff), built on `epoch-cas`.

  Alongside, the two **local adapters** (each needs only the kernel, so they can start as soon as step 1 lands): **`epoch-cas-redb`** (`SyncByteBackend`, persistence) and **`epoch-cas-moka`** (`CachingCAS`, the cache decorator) — together they give a real persistent + cached local CAS this early.
  **Exit gate:** `epoch-cas` + redb + moka + both specializations green with unit tests over the in-memory and redb backends; **no dependency on `epoch-ids`** (lint-enforced).

- **Wave 1 — Domain ids, contracts, timeline & FS adapter (the tie-in).** Introduce `epoch-ids`; bind the kernel via `epoch-cas-api` (dyn `BlobStore`/`TreeStore` on domain ids); freeze `epoch-timeline-api` + `epoch-vfs-api`; stand up `epoch-testkit`. In parallel: `epoch-timeline`, `epoch-timeline-sqlite`, and `epoch-fuse` (developed/mounted against `MockAgentFs`).
  **Exit gate:** `cargo check --workspace` green; object-safety tests pass; `cargo test -p epoch-testkit` green against the `Memory*` impls; dependency-lint clean.

- **Wave 2 — Engine integration.** `epoch-gc`, the real `epoch-vfs` engine, and a **minimal `epoch-daemon`** that mounts FUSE end-to-end over redb + sqlite — a usable time-traveling filesystem **with no side API yet** (the `.vfs/ctl` hook, §5.4, is the interim control surface).

- **Wave 3 — Control plane (DELAYED).** Only now: freeze `epoch-control-api` + `epoch-control-proto`, then build `epoch-control-http` (REST), `epoch-control-grpc` (gRPC — the strongly-typed binary plane), and `epoch-cli`; the daemon starts serving REST + gRPC. Delaying this keeps early effort on the core filesystem.

All 21 crates are still scaffolded up front (compiling stubs) so the workspace builds end-to-end from day one; the waves govern which crates get *fleshed out* first.

### 9.3 Why this parallelizes (vs. a monolithic core)

Funneling all orchestration through one `epoch-core` crate built in a single phase would be a serialization point. Splitting it into independent generic-CAS / file-CAS / tree-CAS / timeline / GC crates (joined only at Wave 2) plus the **in-memory reference impls + conformance kit** is what lets backend authors, the two CAS-specialization authors, and engine authors all make real progress simultaneously without waiting on each other (the two transport adapters are sequenced later — Wave 3, delayed). The generic `epoch-cas` is the highest-leverage shared piece (**D14**) and so **leads the build order** — get it right once, with its in-memory impl, and both specializations + both sync/async paths inherit it.

---

## 10. Cross-cutting concerns

- **D5 — Bytes.** `bytes::Bytes` in all blob signatures (`BytesMut` for the volatile-HEAD buffer, `.freeze()` to hand off). Cheap refcounted clone → zero-copy fan-out through cache and queue; matches what S3/R2 SDKs already hand you.
- **D6 — Errors.** One `thiserror` enum per api crate, with semantic variants callers branch on (`NotFound`, `IntegrityMismatch`, `Shutdown`) plus an opaque `Backend(Box<dyn Error + Send + Sync>)` escape hatch. Backends convert with `.map_err(Error::backend)` (orphan-safe; the api crate never depends on `redb`/`rusqlite`). `FsError::to_errno()` maps to POSIX errno at the fuser reply boundary (`NotFound`→`ENOENT`, integrity/backend→`EIO`, read-only→`EROFS`). No `anyhow` in library signatures — binaries only.
- **D9 — Backpressure.** The FUSE dispatch and write-batch queues are **bounded** (no OOM under a runaway agent loop). A dropped `oneshot` on shutdown surfaces as `Shutdown` → `EIO`, never a hung syscall.
- **D10 — Cancellation/shutdown.** `CancellationToken` lives on background loops (GC, flush), not in per-op signatures. Long-running GC scans return a `Stream` (drop = cancel). Graceful SIGTERM in `epoch-daemon`: stop accepting new FUSE ops → drain the volatile HEAD via the existing `flush()` path → cancel background loops → join with timeout → unmount.

---

## 11. Deferred & open

### 11.1 Deferred (V2)

- **D11 — Agent-runtime layer.** No `epoch-agent-api`/`epoch-agent-worker` in this build. Agents interact via the FUSE mount + control plane; a dedicated agent-execution / tool-gateway boundary (AgentRuntime, ToolGateway, task queue, MCP adapter) is a separate later epic. The seam is preserved by the adapter-agnostic core (§5.5) and the shared control DTOs.
- **Native drift detection** — `notify`-crate monitoring of the physical Lower Dir for external human edits, triggering conflict-resolution boundaries when an agent is active.
- **External syncing** — background workers syncing delta spines + blobs to remote object storage for distributed agent fleets. (The async CAS traits already make remote backends a drop-in — **D1**/**D14**; and the gRPC `DataService` spine-fetch/blob-fetch (**D13**) is the strongly-typed wire for fleet members to pull state without a FUSE mount.)
- **Non-FUSE adapters** — `pyo3` / `napi` / WASM / container, against the same `AgentFs` contract.

### 11.2 Open questions (carried from INITIAL §10)

- **Multi-file flush conflicts.** If `/flush` fires during a multi-file physical operation (`git pull`, `jj rebase`), how do we enforce atomic safety against the host's `.git` directory?
- **UI timeline visualization.** The optimal `/timeline` payload schema to render a 2D network graph of divergent parallel agent timelines without bottlenecking the DOM. The strongly-typed shape is now pinned by the `epoch-control-proto` message + `epoch-control-api` DTO and **must be frozen in Wave 0** (§7, §9.1) — *what fields* the graph payload carries (node/edge attributes, pagination/windowing for huge multiverses) is the remaining open design question, but the contract location is settled.

---

## Appendix A — Technology stack

| Domain | Library | Justification |
|---|---|---|
| Language | Rust (edition 2024) | Memory safety, zero-cost abstractions, kernel FUSE integration. |
| FUSE | `fuser` | High-performance userspace filesystem bindings. |
| KV storage | `redb` | Pure-Rust MVCC embedded DB; the `SyncByteBackend` for the CAS (**D14**). |
| Graph DB | `rusqlite` | DAG lineage + pointer tracking. |
| Hashing | `blake3` | SIMD, deterministic, content-addressing. |
| Compression | `zstd` | Fast block compression for blobs. |
| Caching | `moka` | Concurrent LRU — `sync` at the FUSE boundary + sync CAS hot path (**D1**, **D14**); the `CachingCAS` decorator crate `epoch-cas-moka` (**D8**). |
| Serialization | `bincode`/`rkyv` | Deterministic binary codec for CAS convergence (the `Spine` codec owned by `epoch-tree-cas`, **D4**). |
| Async/REST | `tokio` + `axum` | Background routines, GC scheduling, REST control plane (**D13**). |
| gRPC / IDL | `tonic` + `prost` + `tonic-build` | Strongly-typed binary control + data plane; `.proto` codegen in `epoch-control-proto` (**D13**). |
| Async traits | `async-trait` | Object-safe boxed futures for `Arc<dyn _>` swapping (**D2**). |
| Routing | `ignore` | `.gitignore` pattern matching for the `VfsRouter` (**D12**). |
| Bytes | `bytes` | Zero-copy refcounted buffers (**D5**). |
| Sync primitives | `parking_lot`, `tokio-util` | Volatile-HEAD guard; `CancellationToken` (**D10**). |

## Appendix B — Decision index

| # | Decision |
|---|---|
| D1 | Sync FUSE hot path + sync cache + in-RAM volatile HEAD; `block_on`/bridge only on store-touching miss; async store traits for future remote backends. |
| D2 | `#[async_trait]`, `Send + Sync + 'static`, `Arc<dyn>` blanket impls. |
| D3 | CAS family ⊥ `epoch-timeline`; diff in `epoch-tree-cas`; GC in its own crate. |
| D4 | Opaque content-keyed bytes at the byte-backend boundary; versioned `Spine` codec owned by `epoch-tree-cas`, applied only in the CAS engine. |
| D5 | `bytes::Bytes` everywhere; `BytesMut` for the write buffer. |
| D6 | One `thiserror` enum per api crate; opaque `Backend(..)` variant; `to_errno()` at the FUSE boundary. |
| D7 | redb via `spawn_blocking`; rusqlite via dedicated-thread actor. |
| D8 | Generic `CachingCAS`/`BatchingCAS` decorators (sync + async); cache invalidation on GC delete; read-your-writes in the engine. |
| D9 | Bounded channels; dropped `oneshot` → `Shutdown` → `EIO`. |
| D10 | Cancellation on background loops, not per-op; graceful SIGTERM flush in the daemon. |
| D11 | Defer the agent-runtime layer entirely. |
| D12 | Refined VFS boundary: capability traits + aggregate, type-level read-only isolation, `VfsRouter`, `.vfs/` control-as-adapter, adapter-agnostic core. |
| D13 | Dual-transport control plane: REST (`axum`) + gRPC (`tonic`/`prost`) over a transport-agnostic `ControlService`/`DataService`; gRPC is the strongly-typed binary channel for spine-fetch + streamed blob-fetch. |
| D14 | Generic CAS (`epoch-cas`) with `SyncCAS` **and** `AsyncCAS` trait variants + `ByteBackend`; specialized into `epoch-file-cas` (binary) and `epoch-tree-cas` (strongly-typed spines + diff); local adapters `epoch-cas-redb` (persistence) + `epoch-cas-moka` (cache); sync↔async bridges so the local hot path is runtime-free and remote backends drop in. |
| D15 | Bottom-up, kernel-first build: the **id-agnostic** generic `epoch-cas` (+ redb + moka + the two specializations) is built first, keyed only by content `Hash`. Concrete ids (`epoch-ids`) and the domain binding (`epoch-cas-api`, dyn stores over `BlobId`/`TreeId`) are deferred to Wave 1 — the kernel stays agnostic of other id types. |
