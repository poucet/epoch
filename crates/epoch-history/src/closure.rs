//! [`pending_closure`] — enumerate the not-yet-durable closure of a root, in
//! promotion order.

use std::collections::HashSet;

use epoch_cas::{CASError, CASHash, SyncByteBackend, SyncCAS};
use epoch_file_cas::{BlobId, FileCas, FileObject};
use epoch_tree::{Link, LinkKind, TreeId, TreeStore};

use crate::error::HistoryError;

/// Every CAS object reachable from `root` for which `is_durable` is false, in
/// **post-order**: a tree or manifest appears only after everything it
/// references. Promoting the list in order (batched or not) therefore keeps the
/// durable tier closed under reachability at every intermediate point — which
/// is also what makes the pruning sound: `is_durable(tree)` implies the tree's
/// whole closure is durable, so the walk stops there. The same holds per blob
/// (chunks before manifest) with one nuance: chunk dedup means a *chunk* can be
/// durable while its manifest is not, so chunks are tested individually.
///
/// `is_durable` is typically `|k| far.has(k)` on a
/// [`TieredByteBackend`](epoch_cas::TieredByteBackend)'s far tier. Tombstone
/// links (in an overlay tree) address no object and contribute nothing.
///
/// Errors with [`HistoryError::MissingTree`]/[`MissingBlob`](HistoryError::MissingBlob)
/// if the root references an object the (combined) store does not hold.
pub fn pending_closure<TBk, FBk, L>(
    trees: &TreeStore<TBk, L>,
    files: &FileCas<FBk>,
    root: TreeId,
    is_durable: &dyn Fn(&CASHash) -> Result<bool, CASError>,
) -> Result<Vec<CASHash>, HistoryError>
where
    TBk: SyncByteBackend + Clone,
    FBk: SyncByteBackend + Clone,
    L: Link + Send + Sync + 'static,
{
    let mut out = Vec::new();
    let mut seen: HashSet<CASHash> = HashSet::new();
    // (tree, expanded): pushed unexpanded, re-pushed expanded above its
    // children so it pops (and emits) only after they are fully handled.
    let mut stack: Vec<(TreeId, bool)> = vec![(root, false)];

    while let Some((tid, expanded)) = stack.pop() {
        let hash = tid.into_cas_hash();
        if expanded {
            out.push(hash);
            continue;
        }
        if !seen.insert(hash) || is_durable(&hash)? {
            continue;
        }
        let tree = trees.get(&tid)?.ok_or(HistoryError::MissingTree(tid))?;
        stack.push((tid, true));
        for entry in tree.entries() {
            match entry.id.kind() {
                LinkKind::Tree(child) => stack.push((TreeId::from_cas_hash(child), false)),
                LinkKind::Blob(child) => {
                    visit_blob(files, BlobId::from(child), is_durable, &mut seen, &mut out)?;
                }
                LinkKind::Tombstone => {}
            }
        }
    }

    Ok(out)
}

/// Append a blob's pending objects — non-durable chunks, then the manifest —
/// to `out`. A blob has no further recursion, so its post-order is inline.
fn visit_blob<FBk>(
    files: &FileCas<FBk>,
    blob: BlobId,
    is_durable: &dyn Fn(&CASHash) -> Result<bool, CASError>,
    seen: &mut HashSet<CASHash>,
    out: &mut Vec<CASHash>,
) -> Result<(), HistoryError>
where
    FBk: SyncByteBackend + Clone,
{
    let hash = blob.into_cas_hash();
    if !seen.insert(hash) || is_durable(&hash)? {
        return Ok(());
    }
    let manifest = files.manifest(&blob)?.ok_or(HistoryError::MissingBlob(blob))?;
    if let FileObject::Chunked { chunks } = manifest {
        for chunk in chunks {
            if seen.insert(chunk.id) && !is_durable(&chunk.id)? {
                out.push(chunk.id);
            }
        }
    }
    out.push(hash);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::ffi::OsStr;

    use epoch_cas::{MemoryByteBackend, TieredByteBackend};
    use epoch_vfs_api::ROOT_INODE_ID;
    use epoch_worktree::WorkingTree;

    use super::*;

    type Tiered = TieredByteBackend<MemoryByteBackend, MemoryByteBackend>;

    /// A worktree over a tiered backend, so tests can promote and re-walk.
    fn setup() -> (WorkingTree<Tiered>, Tiered) {
        let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
        (WorkingTree::new(tier.clone()), tier)
    }

    /// Walk `root` against the tier's far side (the production configuration).
    fn walk(tree: &WorkingTree<Tiered>, tier: &Tiered, root: TreeId) -> Vec<CASHash> {
        // The walk reads through stores built over the same shared tier.
        let trees: TreeStore<Tiered, epoch_tree::Overlay<epoch_tree::EntryId<BlobId>>> =
            TreeStore::new(tier.clone());
        let files: FileCas<Tiered> = FileCas::new(tier.clone());
        let _ = tree; // fixture handle kept alive alongside its stores
        pending_closure(&trees, &files, root, &|k| tier.far().has(k)).unwrap()
    }

    /// A file big enough to be chunked (2 MiB + change → 3 chunks).
    fn big(n: u8) -> Vec<u8> {
        let mut v = vec![0u8; 2 * 1024 * 1024 + 17];
        v.iter_mut().enumerate().for_each(|(i, b)| *b = (i % 251) as u8 ^ n);
        v
    }

    #[test]
    fn walk_is_postorder_and_complete() {
        let (mut wt, tier) = setup();
        let dir = wt.mkdir(ROOT_INODE_ID, OsStr::new("dir"), 0o755).unwrap();
        let f = wt.create(dir, OsStr::new("f"), 0o644).unwrap();
        wt.write_at(f, 0, b"hello").unwrap();
        let big_f = wt.create(ROOT_INODE_ID, OsStr::new("big"), 0o644).unwrap();
        wt.write_at(big_f, 0, &big(1)).unwrap();
        let root = wt.seal().unwrap();

        let order = walk(&wt, &tier, root);

        // Root last; every referenced object present exactly once and emitted
        // before its referrer.
        assert_eq!(*order.last().unwrap(), root.into_cas_hash());
        let unique: HashSet<_> = order.iter().collect();
        assert_eq!(unique.len(), order.len(), "no duplicates");
        // 2 trees (root, dir) + 2 manifests + 3 chunks = 7 objects.
        assert_eq!(order.len(), 7);

        // Promoting strictly in order keeps far closed under reachability:
        // after each single promotion, re-walking from any already-promoted
        // object finds nothing pending. Cheap proxy: each prefix of the order
        // never references an object outside prefix ∪ durable.
        for key in &order {
            tier.promote(key).unwrap();
        }
        assert!(walk(&wt, &tier, root).is_empty(), "fully durable → empty walk");
    }

    #[test]
    fn prunes_durable_subtrees_second_checkpoint_is_delta_only() {
        let (mut wt, tier) = setup();
        let dir = wt.mkdir(ROOT_INODE_ID, OsStr::new("stable"), 0o755).unwrap();
        let f = wt.create(dir, OsStr::new("f"), 0o644).unwrap();
        wt.write_at(f, 0, b"unchanged").unwrap();
        let root1 = wt.seal().unwrap();
        for key in walk(&wt, &tier, root1) {
            tier.promote(&key).unwrap();
        }

        // Mutate outside the stable subtree only.
        let g = wt.create(ROOT_INODE_ID, OsStr::new("new"), 0o644).unwrap();
        wt.write_at(g, 0, b"fresh").unwrap();
        let root2 = wt.seal().unwrap();

        let delta = walk(&wt, &tier, root2);
        // New manifest + new root tree only — the `stable/` subtree is pruned
        // at its (durable) TreeId without being decoded.
        assert_eq!(delta.len(), 2);
        assert_eq!(*delta.last().unwrap(), root2.into_cas_hash());
    }

    #[test]
    fn shared_chunk_durable_but_new_manifest_pending() {
        let (mut wt, tier) = setup();
        // Two big files sharing identical leading chunks: same content, so all
        // chunks dedup; then extend one so its manifest differs.
        let a = wt.create(ROOT_INODE_ID, OsStr::new("a"), 0o644).unwrap();
        wt.write_at(a, 0, &big(2)).unwrap();
        let root1 = wt.seal().unwrap();
        for key in walk(&wt, &tier, root1) {
            tier.promote(&key).unwrap();
        }

        let b = wt.create(ROOT_INODE_ID, OsStr::new("b"), 0o644).unwrap();
        let mut content = big(2);
        content.extend_from_slice(b"tail");
        wt.write_at(b, 0, &content).unwrap();
        let root2 = wt.seal().unwrap();

        let delta = walk(&wt, &tier, root2);
        // Pending: b's last chunk (differs), b's manifest, new root. The two
        // shared leading chunks are already durable and individually pruned.
        assert_eq!(delta.len(), 3);
    }

    #[test]
    fn missing_object_is_an_error() {
        let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
        let trees: TreeStore<Tiered, epoch_tree::Overlay<epoch_tree::EntryId<BlobId>>> =
            TreeStore::new(tier.clone());
        let files: FileCas<Tiered> = FileCas::new(tier.clone());
        let ghost = TreeId::from_cas_hash(CASHash::from_bytes([9; 32]));
        assert!(matches!(
            pending_closure(&trees, &files, ghost, &|k| tier.far().has(k)),
            Err(HistoryError::MissingTree(t)) if t == ghost
        ));
    }
}
