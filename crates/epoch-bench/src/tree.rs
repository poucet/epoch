//! Procedural lower-tree generator (EPOCH-FIXES §2.4.3).
//!
//! Content is generated with a fast xorshift PRNG (splitmix64-style), so a
//! 10 GB tree costs 10 GB of *writes*, not of entropy gathering, and the
//! generator itself is unit-testable. Spin-up trees may use `sparse: true`
//! (zero-filled files via `set_len`, one `pwrite` per file) — documented in
//! the suite README: spin-up latency is a *metadata* cost (lazy hydration,
//! research §5), and zero pages exercise the same lookup/readdir paths.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Fast deterministic PRNG (xorshift64*); per-file seed = hash of its index,
/// so trees are reproducible across runs and comparable between bare/mount.
pub struct XorShift(u64);

impl XorShift {
    pub fn new(seed: u64) -> Self {
        // Avoid the degenerate zero state.
        Self(seed | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn fill_bytes(&mut self, buf: &mut [u8]) {
        let mut chunks = buf.chunks_exact_mut(8);
        for c in &mut chunks {
            c.copy_from_slice(&self.next().to_le_bytes());
        }
        let rem = chunks.into_remainder();
        if !rem.is_empty() {
            let n = self.next().to_le_bytes();
            rem.copy_from_slice(&n[..rem.len()]);
        }
    }
}

/// Shape of a synthetic tree.
#[derive(Clone, Copy, Debug)]
pub struct TreeSpec {
    /// Total file count.
    pub files: u64,
    /// Total byte volume across all files.
    pub bytes: u64,
    /// Files are spread over this many leaf directories (fan-out).
    pub dirs: u64,
    /// Zero-fill via set_len instead of writing PRNG bytes (§2.4.3 choice).
    pub sparse: bool,
}

impl TreeSpec {
    /// Convenience from the doc's {files} × {bytes} grid.
    pub fn scaled(files: u64, bytes: u64, sparse: bool) -> Self {
        // ~100 files per directory keeps readdir costs realistic without
        // pathological 10⁵-entry directories.
        let dirs = (files / 100).max(1);
        Self { files, bytes, dirs, sparse }
    }

    fn file_size(&self, index: u64) -> u64 {
        // Even spread with the remainder on the first files.
        let base = self.bytes / self.files.max(1);
        let rem = self.bytes % self.files.max(1);
        base + u64::from(index < rem)
    }
}

/// Generate the tree under `root`; returns the file paths (relative).
pub fn generate(root: &Path, spec: TreeSpec) -> io::Result<Vec<PathBuf>> {
    fs::create_dir_all(root)?;
    let mut paths = Vec::with_capacity(spec.files as usize);
    let mut buf = vec![0u8; 1 << 20];
    for i in 0..spec.files {
        let dir = root.join(format!("d{:05}", i % spec.dirs.max(1)));
        fs::create_dir_all(&dir)?;
        let rel = PathBuf::from(format!("d{:05}/f{:07}.bin", i % spec.dirs.max(1), i));
        let path = root.join(&rel);
        let size = spec.file_size(i);
        if spec.sparse {
            let f = File::create(&path)?;
            f.set_len(size)?;
        } else {
            let mut rng = XorShift::new(i.wrapping_add(0x9E37_79B9_7F4A_7C15));
            let mut w = BufWriter::new(File::create(&path)?);
            let mut left = size;
            while left > 0 {
                let n = (left as usize).min(buf.len());
                rng.fill_bytes(&mut buf[..n]);
                w.write_all(&buf[..n])?;
                left -= n as u64;
            }
            w.flush()?;
        }
        paths.push(rel);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xorshift_is_deterministic_and_nonconstant() {
        let mut a = XorShift::new(42);
        let mut b = XorShift::new(42);
        let seq_a: Vec<u64> = (0..8).map(|_| a.next()).collect();
        let seq_b: Vec<u64> = (0..8).map(|_| b.next()).collect();
        assert_eq!(seq_a, seq_b);
        assert!(seq_a.windows(2).all(|w| w[0] != w[1]));
    }

    #[test]
    fn generated_tree_has_exact_shape() {
        let dir = std::env::temp_dir().join(format!("epoch-bench-tree-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let spec = TreeSpec { files: 250, bytes: 250 * 4096, dirs: 5, sparse: false };
        let paths = generate(&dir, spec).unwrap();
        assert_eq!(paths.len(), 250);
        let total: u64 = paths
            .iter()
            .map(|p| fs::metadata(dir.join(p)).unwrap().len())
            .sum();
        assert_eq!(total, 250 * 4096);
        assert_eq!(fs::read_dir(dir.join("d00000")).unwrap().count(), 50);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sparse_trees_have_exact_sizes() {
        let dir = std::env::temp_dir().join(format!("epoch-bench-sparse-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let spec = TreeSpec { files: 4, bytes: 4 * (3 << 20), dirs: 1, sparse: true };
        let paths = generate(&dir, spec).unwrap();
        for p in &paths {
            assert_eq!(fs::metadata(dir.join(p)).unwrap().len(), 3 << 20);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
