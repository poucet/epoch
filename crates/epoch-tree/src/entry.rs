//! [`TreeEntry`] — one named child of a [`Tree`](crate::Tree).
//!
//! An entry is the tree analog of a git tree record: a `(name, mode, child link,
//! size)` tuple. The `name` is *raw* filename bytes, not a `String`: unix
//! filenames are an arbitrary byte sequence with only `/` and NUL excluded, and
//! are not guaranteed to be valid UTF-8, so forcing them through `String` would
//! be lossy. We keep them as [`Bytes`].
//!
//! `TreeEntry<L>` is generic over the **link** type `L` (see [`Link`]) — the
//! kind of child the entry points at. A *complete* tree uses
//! [`EntryId<B>`](crate::EntryId) (a blob or subtree, never a whiteout); a
//! *sparse overlay* tree uses [`Overlay<EntryId<B>>`](crate::Overlay) (which adds
//! the tombstone). `B` is the foreign blob-id type (see
//! [`BlobRef`](crate::BlobRef)): a tree references blobs by whatever id type the
//! file CAS produces, and only ever names its *own* subtrees via [`TreeId`].
//!
//! ## Symlinks need no special variant
//!
//! Following git, a symlink is *not* a distinct entry kind. It is a blob entry
//! whose [`mode`](TreeEntry::mode) is [`SYMLINK_MODE`] (`0o120000`) and whose
//! blob bytes *are the link target path*. Only the mode distinguishes a symlink
//! from a regular file. This keeps the object model small — there is exactly one
//! indirection kind (blob) and one container kind (tree).
//!
//! ## Tombstones (overlay whiteouts)
//!
//! A complete tree cannot hold a whiteout — [`tombstone`](TreeEntry::tombstone)
//! exists *only* on `TreeEntry<Overlay<E>>`, so an illegal "tombstone in a
//! non-overlay tree" layout will not type-check. In an overlay tree read against
//! a base: an **absent** name falls through to the lower layer, a tombstone
//! **hides** the lower entry (a deletion in the merged view), and a populated
//! entry **overrides** it. A tombstone addresses no object: its `id` is
//! [`Overlay::Tombstone`] and its [`mode`](TreeEntry::mode) and
//! [`size`](TreeEntry::size) are `0`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use epoch_cas::Bytes;

use crate::id::{Link, Overlay, TreeId};

/// The file mode marking a blob entry as a symlink.
///
/// Matches the unix/git convention `0o120000`. A symlink's blob content is the
/// raw target path bytes; see the [module docs](self) for the rationale behind
/// modelling symlinks as moded blobs rather than a separate entry kind.
pub const SYMLINK_MODE: u32 = 0o120000;

/// Nanoseconds per second; the canonical upper bound of [`EntryTime::nanos`].
const NANOS_PER_SEC: u32 = 1_000_000_000;

/// One persisted timestamp: seconds since the unix epoch plus a nanosecond
/// adjustment, in the normalized timespec form (`nanos` in `0..1e9`; negative
/// `secs` carries the pre-epoch range).
///
/// This is the tree format's time representation (format v2 persists `mtime`
/// and `atime` per entry). It maps losslessly to and from
/// [`SystemTime`](std::time::SystemTime); the normalized form is what keeps the
/// encoding canonical (one byte shape per instant), which content addressing
/// requires. The default is [`EntryTime::EPOCH`] — "no time recorded".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryTime {
    /// Whole seconds relative to the unix epoch (negative = before 1970).
    pub secs: i64,
    /// Nanosecond adjustment, always `< 1_000_000_000` (checked on decode).
    pub nanos: u32,
}

impl EntryTime {
    /// The unix epoch — the default for entries built without a known time.
    pub const EPOCH: EntryTime = EntryTime { secs: 0, nanos: 0 };

    /// Whether the value is in canonical (normalized) form.
    pub fn is_canonical(&self) -> bool {
        self.nanos < NANOS_PER_SEC
    }

    /// Convert a [`SystemTime`] to the normalized `(secs, nanos)` form,
    /// preserving pre-epoch instants.
    pub fn from_system_time(t: SystemTime) -> Self {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => EntryTime {
                secs: d.as_secs() as i64,
                nanos: d.subsec_nanos(),
            },
            Err(before) => {
                let d = before.duration();
                if d.subsec_nanos() == 0 {
                    EntryTime {
                        secs: -(d.as_secs() as i64),
                        nanos: 0,
                    }
                } else {
                    // e.g. 1.75 s before the epoch = -2 s + 250 ms.
                    EntryTime {
                        secs: -(d.as_secs() as i64) - 1,
                        nanos: NANOS_PER_SEC - d.subsec_nanos(),
                    }
                }
            }
        }
    }

    /// Convert back to a [`SystemTime`]. A non-canonical `nanos` field
    /// (≥ 1 s) is clamped rather than overflowed; such values never come from
    /// the validating codec.
    pub fn to_system_time(self) -> SystemTime {
        if self.secs >= 0 {
            UNIX_EPOCH + Duration::new(self.secs as u64, self.nanos.min(NANOS_PER_SEC - 1))
        } else {
            // magnitude = |secs| - nanos/1e9 (see from_system_time).
            let nanos = self.nanos.min(NANOS_PER_SEC - 1);
            let (mag_secs, mag_nanos) = if nanos == 0 {
                ((-(self.secs)) as u64, 0)
            } else {
                ((-(self.secs + 1)) as u64, NANOS_PER_SEC - nanos)
            };
            UNIX_EPOCH - Duration::new(mag_secs, mag_nanos)
        }
    }
}

/// One named child of a tree: a filename plus the child's link, mode, size and
/// persisted timestamps.
///
/// `name` is raw filename bytes (not necessarily UTF-8). `mode` carries the
/// unix permission/type bits (e.g. `0o100644` for a regular file, `0o040000`
/// for a directory, [`SYMLINK_MODE`] for a symlink). `size` is the logical
/// content size in bytes — for directories (and tombstones) it is conventionally
/// `0`. `mtime`/`atime` are the persisted modification/access times (tree
/// format v2); they default to [`EntryTime::EPOCH`] ("no time recorded") and
/// are set explicitly via [`with_times`](TreeEntry::with_times). A tombstone
/// addresses no object: its `id` is [`Overlay::Tombstone`] and its other fields
/// are all zeroed (times are not encoded for tombstones).
///
/// Generic over the link type `L` (see the [module docs](self)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry<L> {
    /// Raw filename bytes (unix names are not guaranteed UTF-8).
    pub name: Bytes,
    /// The child's link (blob / subtree, plus tombstone for overlay trees).
    pub id: L,
    /// Unix mode/permission bits (see [`SYMLINK_MODE`]); `0` for a tombstone.
    pub mode: u32,
    /// Logical content size in bytes (`0` by convention for directories and
    /// tombstones).
    pub size: u64,
    /// Persisted modification time (format v2); `0` for a tombstone.
    pub mtime: EntryTime,
    /// Persisted access time (format v2); `0` for a tombstone.
    pub atime: EntryTime,
}

impl<L: Link> TreeEntry<L> {
    /// Construct an entry pointing at a file blob.
    ///
    /// Pass [`SYMLINK_MODE`] as `mode` (with `size` = the target path length and
    /// the blob bytes = the target) to represent a symlink; see the
    /// [module docs](self). Works for any link type `L` (complete or overlay).
    pub fn file(name: impl Into<Bytes>, id: L::Blob, mode: u32, size: u64) -> TreeEntry<L> {
        TreeEntry {
            name: name.into(),
            id: L::from_blob(id),
            mode,
            size,
            mtime: EntryTime::EPOCH,
            atime: EntryTime::EPOCH,
        }
    }

    /// Construct an entry pointing at a subtree. `size` is fixed to `0` — a
    /// directory has no meaningful content length.
    pub fn dir(name: impl Into<Bytes>, id: TreeId, mode: u32) -> TreeEntry<L> {
        TreeEntry {
            name: name.into(),
            id: L::from_tree(id),
            mode,
            size: 0,
            mtime: EntryTime::EPOCH,
            atime: EntryTime::EPOCH,
        }
    }

    /// Set the persisted timestamps (builder form): the entry's modification
    /// and access times, sealed into the tree object.
    pub fn with_times(mut self, mtime: EntryTime, atime: EntryTime) -> TreeEntry<L> {
        self.mtime = mtime;
        self.atime = atime;
        self
    }

    /// Set the persisted timestamps from [`SystemTime`]s (builder form).
    pub fn with_system_times(self, mtime: SystemTime, atime: SystemTime) -> TreeEntry<L> {
        self.with_times(
            EntryTime::from_system_time(mtime),
            EntryTime::from_system_time(atime),
        )
    }
}

impl<E> TreeEntry<Overlay<E>> {
    /// Construct a tombstone (whiteout) entry hiding `name` in an overlay layer.
    ///
    /// Defined *only* for an overlay link, so a complete tree
    /// (`TreeEntry<EntryId<B>>`) cannot construct one — the illegal layout does
    /// not type-check. A tombstone addresses no object, so both `mode` and
    /// `size` are fixed to `0`; see the [module docs](self).
    pub fn tombstone(name: impl Into<Bytes>) -> TreeEntry<Overlay<E>> {
        TreeEntry {
            name: name.into(),
            id: Overlay::Tombstone,
            mode: 0,
            size: 0,
            mtime: EntryTime::EPOCH,
            atime: EntryTime::EPOCH,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{EntryId, Link, LinkKind};
    use epoch_cas::CASHash;

    // CASHash is used as the blob-id type B in tests (it satisfies BlobRef).
    type Complete = EntryId<CASHash>;
    type Sparse = Overlay<EntryId<CASHash>>;

    fn sample_hash(seed: u8) -> CASHash {
        CASHash::from_bytes([seed; 32])
    }

    #[test]
    fn file_constructor_sets_blob_link() {
        let id = sample_hash(1);
        let e = TreeEntry::<Complete>::file(Bytes::from_static(b"readme.txt"), id, 0o100644, 42);
        assert_eq!(e.name, Bytes::from_static(b"readme.txt"));
        assert_eq!(e.id, EntryId::Blob(id));
        assert_eq!(e.mode, 0o100644);
        assert_eq!(e.size, 42);
        assert!(e.id.is_blob());
    }

    #[test]
    fn dir_constructor_forces_zero_size() {
        let id = TreeId::from_cas_hash(sample_hash(2));
        let e = TreeEntry::<Complete>::dir(Bytes::from_static(b"src"), id, 0o040000);
        assert_eq!(e.id, EntryId::Tree(id));
        assert_eq!(e.size, 0);
        assert!(e.id.is_tree());
    }

    #[test]
    fn overlay_file_wraps_in_populated() {
        let id = sample_hash(3);
        let e = TreeEntry::<Sparse>::file(Bytes::from_static(b"f"), id, 0o100644, 7);
        assert_eq!(e.id, Overlay::Populated(EntryId::Blob(id)));
        assert_eq!(e.id.kind(), LinkKind::Blob(id));
    }

    #[test]
    fn tombstone_constructor_is_zero_moded_whiteout() {
        let e = TreeEntry::<Sparse>::tombstone(Bytes::from_static(b"gone"));
        assert_eq!(e.name, Bytes::from_static(b"gone"));
        assert_eq!(e.id, Overlay::Tombstone);
        assert_eq!(e.mode, 0);
        assert_eq!(e.size, 0);
        assert!(e.id.is_tombstone());
        assert_eq!(Link::cas_hash(&e.id), None);
    }

    #[test]
    fn symlink_is_a_blob_with_symlink_mode() {
        // A symlink "link" -> "target/path" is just a blob entry whose mode is
        // SYMLINK_MODE; the blob's bytes (elsewhere in the file CAS) are the
        // target path.
        let target = b"target/path";
        let id = sample_hash(4);
        let e = TreeEntry::<Complete>::file(
            Bytes::from_static(b"link"),
            id,
            SYMLINK_MODE,
            target.len() as u64,
        );
        assert_eq!(e.mode, SYMLINK_MODE);
        assert!(e.id.is_blob());
        assert_eq!(e.size, target.len() as u64);
    }

    #[test]
    fn entry_time_default_is_the_epoch() {
        let e = TreeEntry::<Complete>::file(Bytes::from_static(b"f"), sample_hash(1), 0o100644, 1);
        assert_eq!(e.mtime, EntryTime::EPOCH);
        assert_eq!(e.atime, EntryTime::EPOCH);
        assert_eq!(EntryTime::default(), EntryTime::EPOCH);
    }

    #[test]
    fn entry_time_system_time_roundtrips() {
        let cases = [
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789),
            SystemTime::now(),
            // Pre-epoch instants: exact second and sub-second.
            SystemTime::UNIX_EPOCH - Duration::new(1, 0),
            SystemTime::UNIX_EPOCH - Duration::new(1, 750_000_000),
            SystemTime::UNIX_EPOCH - Duration::new(0, 1),
        ];
        for t in cases {
            let et = EntryTime::from_system_time(t);
            assert!(et.is_canonical(), "{et:?} not canonical");
            assert_eq!(et.to_system_time(), t, "round-trip failed for {t:?}");
        }
    }

    #[test]
    fn with_times_sets_both_fields() {
        let m = EntryTime { secs: 100, nanos: 5 };
        let a = EntryTime { secs: 200, nanos: 7 };
        let e = TreeEntry::<Complete>::file(Bytes::from_static(b"f"), sample_hash(1), 0o100644, 1)
            .with_times(m, a);
        assert_eq!((e.mtime, e.atime), (m, a));
        let t0 = SystemTime::UNIX_EPOCH + Duration::new(42, 42);
        let e2 = TreeEntry::<Complete>::dir(
            Bytes::from_static(b"d"),
            TreeId::from_cas_hash(sample_hash(2)),
            0o040755,
        )
        .with_system_times(t0, t0);
        assert_eq!(e2.mtime.to_system_time(), t0);
    }
}
