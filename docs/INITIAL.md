Here is the complete, consolidated design document. It is stripped of conversational filler and highly structured for immediate use in your Google Doc.
# System Design Document: Temporal Virtual Filesystem for AI Agents
**Authors:** Chris & Solasta
**Date:** June 18, 2026
**Status:** Historical — the founding design, retained for reference. The as-built architecture is `docs/ARCHITECTURE.md`; checkpointing/timeline specifics are `docs/TIMELINE.md`.
## 1. Overview & Objectives
A high-performance, time-traveling Virtual Filesystem (VFS) designed for autonomous AI agents and developer workflows. It decouples immutable file state (Content-Addressable Storage) from temporal lineage (Directed Acyclic Graph). By intercepting filesystem operations via FUSE, it provides consequence-free state scrubbing, zero-cost recursive forking, and strict blast-radius isolation without altering the underlying physical project.
## 2. Core Requirements
### Functional
 * **POSIX Compliance:** Seamless integration with standard developer tools (LSPs, compilers, IDEs).
 * **Automated Versioning:** Capture fine-grained micro-mutations automatically; no manual commits required.
 * **Instant Branching:** O(1) recursive forking for parallel multi-agent workflows.
 * **Control API:** Provide an HTTP/REST Control Plane for UI timeline scrubbing and agent tool calling (e.g., Model Context Protocol).
 * **Dynamic Ignore:** Strictly respect .gitignore rules, diverting ephemeral build artifacts from the persistent history log.
### Non-Functional
 * **Zero-Latency I/O:** Storage writes must not block the critical FUSE hot path.
 * **Isolation:** Rogue agents must be physically incapable of permanently destroying the underlying source code.
 * **Storage Efficiency:** Perfect structural deduplication of identical file states across all temporal branches.
 * **Crash Resilience:** Ensure active, unpersisted session states survive daemon crashes without database corruption.
## 3. High-Level Architecture
The system utilizes a global daemon and thin-client model to maximize memory deduplication and safely serialize database writes.
### 3.1. Core Components
| Component | Role | Function |
|---|---|---|
| **agent-vfsd** | Global Daemon | Long-running Rust background service. Manages shared memory (moka LRU), databases, background tasks (GC/Flushing), and the Control Plane API. |
| **agent-cli** | Thin Client | Ephemeral CLI executable used to issue commands (e.g., mount, scrub, flush) to the daemon. |
| **FUSE Interceptor** | Data Plane | Kernel-level translation of OS filesystem calls into memory/database routing logic. |
| **Side API** | Control Plane | HTTP/REST interface exposing endpoints (/timeline, /diff, /snapshot) to manipulate the temporal anchor. |
| **VFS Control Plane | Control Plane | .vfs/ files that are never stored but act as secondary ontrol plane interface |
### 3.2. Execution Environment Agnosticism
The FUSE mount appears to the OS as a standard directory, supporting any execution context:
 * **Bare Metal:** Scripts and IDEs run directly against the mount (~/agent-workspace).
 * **Sandboxed Containers:** The host daemon bind-mounts the VFS into standard Docker containers (-v /tmp/agent-vfs:/workspace), allowing agents to run completely unprivileged.
 * **Remote Execution:** The thin client can trigger dynamic mounts over SSH or CI/CD pipelines.
## 4. Storage Engine & Pluggable Backend Architecture
Persistence strictly separates pure file state from temporal lineage using abstract Rust traits, allowing storage backends to be decoupled or swapped per deployment.
### 4.1. Pure State CAS (CasStore Dual-Trait Architecture)
 * **BlobStore (Raw File Content):**
   * **Role:** High-throughput bulk storage for opaque, compressed binary bytes.
   * **Structure:** Verified via BLAKE3 hashes. Immutable.
   * **Backends:** redb (local default), AWS S3, Cloudflare R2.
 * **TreeStore / SpineStore (Structured Directory Maps):**
   * **Role:** Ultra-low latency storage for serialized Merkle trees.
   * **Structure:** Maps filenames to embedded metadata (mode, type) and points to child BlobStore or TreeStore hashes. Sorted alphabetically (BTreeMap) for deterministic convergence.
   * **Backends:** redb / sled (local default), Redis, Postgres.
### 4.2. Timeline Graph Database (TimelineStore)
Manages the Directed Acyclic Graph (DAG) of the temporal multiverse.
 * **Backends:** rusqlite (local default), Postgres, Graph DBs.
 * **Tables:**
   * **Timeline_Events (Nodes):** Maps a specific event to a Root Tree Spine_CAS_ID and a Parent_Event_ID.
   * **Timeline_Heads (Pointers):** Tracks volatile session anchors and stable tags (e.g., v1.0-stable).
## 5. Key Features & Use Cases
 * **The Magic /.cas/ Directory:** An O(1) direct-access portal exposed inside the workspace for agents to instantly read historical states via hash. readdir is blocked to prevent context-window blowout; lookup and read are permitted under strict POSIX EROFS (Read-Only) enforcement.
 * **Multi-Agent Multiverse:** Recursive branching is an O(1) memory operation requiring zero file copies. A shared LRU cache prevents duplicated RAM overhead when parallel agents read the same files.
 * **Autopsy Quarantine:** Failed/crashed agent workspaces are tagged QUARANTINE with a Time-To-Live (TTL). Developers can mount a read-only Autopsy Session to safely scrub backward through the failure lineage.
 * **Write-Back Flushing:** The /flush command calculates a structural diff between the active CAS Spine and the physical Lower Directory, executing precise overwrites to permanently commit finalized work.
 * **Universal Diff Engine:** Rapid timeline diffing is achieved via Merkle tree graph pruning; the API skips identical sub-trees instantly without evaluating underlying file blobs.
## 6. Architecture Mechanics & Trade-Offs
### 6.1. State Coalescing & Flush-Driven Persistence (Debouncing)
To prevent storage bloat from hyperactive IDE auto-saves or agent loops, granular micro-mutations are NOT written to disk continuously.
 * **Mechanism:** FUSE release syscalls update the file buffer and Tree entry strictly within a RAM-based Volatile HEAD. Intermediate states continuously overwrite each other in memory.
 * **Persistence Trigger:** Only upon an explicit trigger (e.g., /flush, /snapshot, or a macroscopic timer) are the *surviving* states hashed, compressed, and persisted to the CasStore.
 * **Trade-off:** Eliminates "junk blob" I/O and vastly reduces GC load, at the cost of losing sub-interval micro-keystrokes in a hard crash.
### 6.2. Asynchronous Write Batching
For actions that *do* require persistence, POSIX durability guarantees are relaxed.
 * **Mechanism:** Data is dropped into a concurrent memory queue. FUSE instantly returns success (Ok(())). A background tokio thread commits the queue to the database in bulk transactions on a strict interval or high-watermark RAM trigger (>500MB).
 * **Trade-off:** FUSE latency drops to zero. A power-loss crash loses the last few seconds of uncommitted buffer, but database corruption is mathematically impossible.
### 6.3. 3-Tier Overlay & Hybrid Ignore Engine
Ephemeral artifacts (node_modules/, binaries) bypass the CAS to prevent database bloat. Routing relies on a fast LRU Path Cache that invalidates instantly if the .gitignore CAS hash mutates.
 * **Tier 1: Scratchpad (Ignored):** Written strictly to a temporary physical host directory.
 * **Tier 2: CAS Spine (Tracked):** Written to the Volatile HEAD memory buffer / CasStore.
 * **Tier 3: Lower Dir (Untouched):** Read-only fallback for the pristine physical project.
 * **Trade-off:** Retroactively ignored files remain safely frozen in historical Spines, but all future mutations route to the Scratchpad to keep the hot path O(1).
### 6.4. Embedded vs. Separated Metadata
 * **Decision:** File metadata (mode, permissions) is embedded directly into the Tree Objects.
 * **Trade-off:** Running chmod cascades into a new Root Tree hash, but guarantees O(1) database reads for FUSE getattr and ls -l requests, avoiding catastrophic N+1 query latency.
## 7. Technology Stack
| Domain | Component / Library | Justification |
|---|---|---|
| **Language** | Rust | Memory safety, zero-cost abstractions, kernel FUSE integration. |
| **FUSE Layer** | fuser | High-performance userspace filesystem bindings. |
| **KV Storage** | redb | Pure-Rust MVCC embedded database for blobs/trees. |
| **Graph DB** | rusqlite | Pointer tracking and DAG lineage metadata. |
| **Hashing** | blake3 | SIMD-optimized, deterministic cryptographic hashing. |
| **Compression** | zstd | Ultra-fast block compression for file blobs. |
| **Caching** | moka | Highly concurrent, low-latency LRU cache. |
| **Serialization** | bincode / rkyv | Fast, deterministic binary serialization for CAS convergence. |
| **Async / API** | tokio + axum | Background routines, GC scheduling, and HTTP Control Plane. |
| **Routing** | ignore | Advanced .gitignore pattern matching. |
## 8. Lifecycle & Garbage Collection
An asynchronous, lazy Tracing Garbage Collector runs in the background to reclaim unreachable multiversal timelines.
 1. **Mark Phase:** Traces parent pointers in the TimelineStore starting from all active Timeline_Heads and stable tags. Collects all reachable Spine_CAS_IDs into a set.
 2. **Sweep Phase:** Prunes unvisited nodes from the TimelineStore. Traverses the reachable Spines in the TreeStore to identify required blobs. Safely purges any key in the CasStore not present in the reachable sets.
## 9. Potential Improvements (V2)
 * **Plan 9-Style Virtual Control Interface:** Expose a hidden directory inside the FUSE mount (e.g., /.vfs/ctl) accepting commands via standard file writes (e.g., echo "snapshot" > /.vfs/ctl/trigger), unifying the control/data planes natively.
 * **Native Drift Detection:** Use the notify crate to monitor the physical Lower Directory for external human edits, triggering safe conflict-resolution boundaries if an agent is active.
 * **Container & WebAssembly Adapters:** Abstract core logic into an AgentFS trait, decoupling the VFS from FUSE to run natively inside Python (pyo3), Node (napi-rs), or WASM sandboxes.
 * **External Syncing:** Background workers syncing Delta Spines and Blobs to remote object storage for distributed agent fleet collaboration.
## 10. Open Questions & Unaddressed Issues
 * **Multi-File Flush Conflicts:** If /flush is triggered during a multi-file physical operation (e.g., git pull or jj rebase), how does the system enforce atomic safety against the host's .git directory?
 * **UI Timeline Visualization:** Determining the optimal JSON payload schema for the /timeline API to render a 2D network graph of divergent parallel agent timelines without bottlenecking the DOM.


