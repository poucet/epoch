//! Live debug statistics behind the `.vfs/debug` control-plane directory.
//!
//! [`DebugStats`] is the provider seam: the live filesystem (in practice
//! [`CasVfs`](crate::CasVfs)) renders a point-in-time [`DebugSnapshot`], and
//! [`ControlPlaneVfs`](crate::ControlPlaneVfs) — handed the provider via
//! [`with_debug`](crate::ControlPlaneVfs::with_debug) — serves it as three
//! read-only virtual files:
//!
//! - `.vfs/debug/near-objects` — objects staged in the near tier,
//! - `.vfs/debug/dirty-files` — files with un-sealed dirty writers,
//! - `.vfs/debug/overlay` — one materialized overlay path per line.
//!
//! [`NearCount`] is the backend-side half: only a backend whose staging tier is
//! an in-memory map can count its objects cheaply, so the trait defaults to
//! `None` and is implemented concretely for the countable shapes
//! ([`MemoryByteBackend`], and [`TieredByteBackend`] with a memory near tier) —
//! no specialization involved.

use epoch_cas::{MemoryByteBackend, TieredByteBackend};

/// A point-in-time view of the live filesystem's internals, rendered fresh for
/// every `.vfs/debug` read. See the [module docs](self) for the three files it
/// feeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugSnapshot {
    /// Objects currently staged in the near (staging) tier, or `None` when the
    /// backend cannot count them cheaply (see [`NearCount`]).
    pub near_objects: Option<usize>,
    /// Files with a live, un-sealed dirty writer in the working tree.
    pub dirty_files: usize,
    /// One rendered line per node materialized in the overlay (the delta vs the
    /// lower): plain paths for files, trailing `/` for directories, and
    /// ` (tombstone)` / ` (sealed subtree)` annotations — the
    /// [`Display`](std::fmt::Display) form of
    /// [`OverlayEntry`](epoch_worktree::OverlayEntry).
    pub overlay_paths: Vec<String>,
}

/// A live filesystem that can report a [`DebugSnapshot`] of its internals.
///
/// Implemented by [`CasVfs`](crate::CasVfs); consumed by
/// [`ControlPlaneVfs::with_debug`](crate::ControlPlaneVfs::with_debug).
pub trait DebugStats: Send + Sync {
    /// Render a fresh point-in-time snapshot.
    fn debug_snapshot(&self) -> DebugSnapshot;
}

/// A byte backend that may know how many objects sit in its near (staging)
/// tier.
///
/// The default answers `None` (counting would mean scanning durable storage);
/// the concrete impls below answer for the shapes whose near tier is an
/// in-memory map. A blanket-default-plus-concrete-impls trait, not
/// specialization.
pub trait NearCount {
    /// The number of objects in the near tier, if cheaply countable.
    fn near_count(&self) -> Option<usize> {
        None
    }
}

/// A plain in-memory backend *is* its own staging tier: everything it holds is
/// near.
impl<K> NearCount for MemoryByteBackend<K> {
    fn near_count(&self) -> Option<usize> {
        Some(self.len())
    }
}

/// The tiered shape the durable mount uses: an in-memory staging tier over any
/// far tier. Only the near side is counted.
impl<K, Far> NearCount for TieredByteBackend<MemoryByteBackend<K>, Far> {
    fn near_count(&self) -> Option<usize> {
        Some(self.near().len())
    }
}
