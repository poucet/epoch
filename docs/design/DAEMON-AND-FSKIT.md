# Shared Machine Daemon (Option A), Single-Binary Mode, and FSKit Strategy

**Status:** Design Proposal, 2026-09-10  
**Context:** Multi-mount concurrency, Docker + macOS host sharing, Apple FSKit integration, and developer-account-free workflows.

---

## 1. Overview & Goals

1. **Shared Daemon (Option A)**: Enable multiple concurrent filesystem views — such as a Docker container running Linux tools over FUSE and a native macOS client (macFUSE or FSKit) — to share one single underlying `epoch.redb` state store and CAS deduplication engine.
2. **First-Class Single-Binary Mode**: Preserve the frictionless, zero-daemon `epoch cas <mount> <lower> --state <dir>` experience for quick local iteration and single-process mounts.
3. **FSKit Integration Without Apple Developer Account**: Establish a practical path for developing and using FSKit without requiring a paid Apple Developer subscription or production entitlement signing.
4. **Continuous CI Benchmarking**: Compare raw disk vs epoch-fs under high-churn operations (procedural git cloning, jj workflows, cargo builds from scratch) and publish regression metrics to GitHub Actions.

---

## 2. Two Runtime Topologies

### Topology 1: The Machine Daemon (`epoch-daemon`)

Redb enforces a single-writer-per-process constraint. To allow N concurrent mounts to read and write against the same timeline and CAS store, the daemon cut separates the storage engine from the mount shims:

```
┌──────────────────────────────────────────────────────────┐
│                      epoch-daemon                        │
│                                                          │
│   WorkingTrees · Memory Staging · Redb Durable Store     │
│             Timeline DAG · Checkpoint Engine             │
│                          │                               │
│                VfsService Server (UDS)                   │
└──────────────────────────┬───────────────────────────────┘
                           │
             Unix Domain Socket / IPC Stream
         ┌─────────────────┼──────────────────┐
         ▼                 ▼                  ▼
┌──────────────────┐ ┌───────────────┐ ┌──────────────┐
│  Docker VM FUSE  │ │  macOS FUSE   │ │  macOS FSKit │
│  (in-container)  │ │  (macFUSE)    │ │ (Swift shim) │
└──────────────────┘ └───────────────┘ └──────────────┘
```

- **UDS Protocol**: The daemon exposes an object-safe RPC mirroring `VfsService` (`lookup`, `getattr`, `readdir`, `read_at`, `write_at`, `create`, `mkdir`, `unlink`, `rename`, `symlink`, `readlink`, `setattr`, `statfs`).
- **Cross-Environment Sharing**: The daemon runs as a native host process on macOS. A Docker container mounts the UDS socket via a volume bind (`-v /var/run/epoch:/var/run/epoch`) and runs an ultra-lightweight FUSE shim that translates Linux `/dev/fuse` requests into UDS calls.
- **Deduplication & State Coherence**: Writes from both the host and the Docker container land in the same deduplicated CAS and timeline DAG.

### Topology 2: Single-Binary In-Process Mode

For quick local runs, testing, or one-off tasks where running a background service is unnecessary:
```
┌──────────────────────────────────────────────────────────┐
│                   epoch-fuse (Single Binary)             │
│                                                          │
│   fuser Adapter ───▶ CasVfs ───▶ In-Process WorkingTree  │
│                                           │              │
│                              Redb CAS & Timeline Store   │
└──────────────────────────────────────────────────────────┘
```
- Completely standalone: no background process to manage or clean up.
- Invoked simply with: `epoch cas ~/epoch ~/project --state ~/.epoch`
- Full functionality, zero configuration.

---

## 3. FSKit Strategy Without Apple Developer Account

Apple’s FSKit framework requires the `com.apple.developer.fskit.fsmodule` entitlement for distributed System Extensions. However, development can proceed effectively without an Apple Developer account:

### 1. The FUSE Primary Path (Zero Cost, Zero Entitlements)
- **Linux**: In-kernel `/dev/fuse` is fully open, unprivileged, and runs out of the box (in Docker, CI, and production).
- **macOS**: macFUSE runs with local user approval. It requires no Apple Developer subscription and provides full POSIX semantics today.

### 2. Local FSKit Development Mode
- macOS supports testing system extensions locally without an App Store provisioning profile by enabling developer mode:
  ```sh
  systemextensionsctl developer on
  ```
- Extensions can be signed with local ad-hoc certificates (`codesign -s -`) for local debugging and experimentation on macOS 15+.

### 3. Decoupled Architecture
Because the FSKit adapter in Option A is merely a thin Swift shim mapping `FSVolume` calls to the UDS `VfsService` socket:
- 100% of the filesystem logic, Merkle trees, and snapshot persistence remain in portable Rust.
- Switching between FUSE and FSKit is purely a frontend selection with zero backend changes.

---

## 4. High-File-Count Benchmark Profiles in CI

The CI suite validates high-churn operations inside the mount:
1. **`git-flow`**: `git init`, generation of 200 distributed files, staging, commit, status, and `git clone` inside the mount.
2. **`jj-flow`**: `jj git init`, commit, and log operations over tracked files.
3. **`cargo-build`**: Generating a fresh 8-module Rust crate inside the mount and compiling it with `cargo build --offline --target-dir target` (measuring compiler write churn and artifact generation).
4. **Automated Publishing**: The raw disk vs epoch-fs metrics are published automatically to the GitHub Actions Job Summary (`$GITHUB_STEP_SUMMARY`) on every run.
