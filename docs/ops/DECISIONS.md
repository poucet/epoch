# Decisions

One row, one decision, one date — so a settled question stays settled. Carries
both a clock and the jj change id (SYSTEM.md §2).

| When · change id | Decision | Rationale |
|---|---|---|
| 2026-08-30 02:15 · nttzrqvn | Store-side landing verbs (`epoch diff\|export\|audit`) ahead of daemon | Export and audit verbs query `<state-dir>/epoch.redb` directly store-side without needing a live FUSE mount or daemon; mount attribution is serialized as schema-v1 JSON in checkpoint `meta`. |
| 2026-08-30 02:44 · ynvtzmko | Times never live in CAS (three-layer timestamp model) | Persisting atime/mtime in CAS tree entries would alter content hashes for unchanged files and destroy deduplication across checkpoints and forks; in-memory session times on live mount + checkpoint-stamped history views satisfy POSIX semantics while preserving structural dedup. |
| 2026-08-30 03:02 · qupstnsn | Resource costs (CPU + RSS) captured alongside latency in benchmarks | Latency alone masks daemon CPU overhead and in-memory staging tier memory pressure; rusage CPU deltas and ps/mach RSS sampling quantify operational costs per workload. |
