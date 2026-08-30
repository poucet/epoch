//! The two sides of every measurement pair: the **bare lower** (a plain host
//! directory) and the **mount side**. Two mount-side modes:
//!
//! - `HostMount`: a real `epoch-fuse` FUSE mount reached through the kernel —
//!   the numbers that decide (kernel round-trips included).
//! - `InProcess`: a `CasVfs` overlay driven directly through `VfsService` —
//!   VFS-layer cost without FUSE, runnable headless and in unit tests, and
//!   immune to the known macFUSE 5.x rename wire bug (the bug is in the wire
//!   decode, not the overlay; EPOCH-FIXES §1.1).

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use epoch_vfs_api::{
    CreateRequest, InodeId, NodeKind, OpenMode, OpenRequest, ReadRequest, VfsError, VfsResult,
    VfsService, WriteRequest, ROOT_INODE_ID,
};

/// Uniform error so workload code doesn't care which side failed.
#[derive(Debug)]
pub enum TargetError {
    Io(io::Error),
    Vfs(VfsError),
}

impl TargetError {
    /// The errno-style signature used for graceful KNOWN-FAIL degradation.
    pub fn signature(&self) -> String {
        match self {
            TargetError::Io(e) => match e.raw_os_error() {
                Some(code) => format!("errno {code}"),
                None => e.to_string(),
            },
            TargetError::Vfs(e) => e.to_string(),
        }
    }
}

impl From<io::Error> for TargetError {
    fn from(e: io::Error) -> Self {
        TargetError::Io(e)
    }
}

impl From<VfsError> for TargetError {
    fn from(e: VfsError) -> Self {
        TargetError::Vfs(e)
    }
}

pub type TargetResult<T> = Result<T, TargetError>;

/// One measured side of the suite.
pub enum Target {
    /// Plain host directory (bare lower, or a real FUSE mount reached via fs).
    Host(PathBuf),
    /// In-process `VfsService` mount rooted at `workspace` under the root.
    Vfs { vfs: Arc<dyn VfsService>, workspace: String },
}

impl Target {
    pub fn host(path: impl Into<PathBuf>) -> Self {
        Target::Host(path.into())
    }

    pub fn vfs(vfs: Arc<dyn VfsService>, workspace: &str) -> TargetResult<Self> {
        // Create the workspace dir so all ops below stay inside it. An empty
        // workspace means "the mount root itself" (spin-up probes).
        if !workspace.is_empty() {
            let root = lookup_path(vfs.as_ref(), Path::new(""))?;
            mkdir_at(vfs.as_ref(), root, OsStr::new(workspace))?;
        }
        Ok(Target::Vfs { vfs, workspace: workspace.to_string() })
    }

    fn lookup(&self, rel: &Path) -> TargetResult<InodeId> {
        match self {
            Target::Vfs { vfs, workspace } => {
                lookup_path(vfs.as_ref(), &Path::new(workspace).join(rel)).map_err(Into::into)
            }
            Target::Host(_) => unreachable!("host target has no inodes"),
        }
    }

    fn parent_of(&self, rel: &Path) -> TargetResult<(InodeId, OsString)> {
        let parent = rel.parent().unwrap_or_else(|| Path::new(""));
        let name = rel.file_name().unwrap_or_else(|| OsStr::new("")).to_os_string();
        Ok((self.lookup(parent)?, name))
    }

    /// Read a whole file; returns bytes read.
    pub fn read_file(&self, rel: &Path, buf: &mut Vec<u8>) -> TargetResult<u64> {
        match self {
            Target::Host(root) => {
                let mut f = File::open(root.join(rel))?;
                let n = f.read_to_end(buf)?;
                Ok(n as u64)
            }
            Target::Vfs { vfs, .. } => {
                let id = self.lookup(rel)?;
                let opened = vfs.open(OpenRequest {
                    inode_id: id,
                    mode: OpenMode::ReadOnly,
                    truncate: false,
                    append: false,
                })?;
                let mut total = 0u64;
                loop {
                    let chunk = vfs.read(ReadRequest {
                        inode_id: id,
                        handle: opened.handle,
                        offset: total,
                        size: 1 << 20,
                    })?;
                    if chunk.is_empty() {
                        break;
                    }
                    total += chunk.len() as u64;
                }
                vfs.release(opened.handle)?;
                Ok(total)
            }
        }
    }

    /// create + write ≤ 4 KiB + fsync + close: the §2.2 small-file class.
    pub fn write_small(&self, rel: &Path, data: &[u8], fsync: bool) -> TargetResult<()> {
        match self {
            Target::Host(root) => {
                let mut f = File::create(root.join(rel))?;
                f.write_all(data)?;
                if fsync {
                    f.sync_all()?;
                }
                Ok(())
            }
            Target::Vfs { vfs, .. } => {
                let (parent, name) = self.parent_of(rel)?;
                let created = vfs.create(CreateRequest::file(parent, name, 0o644))?;
                let id = created.entry.attr.inode_id;
                let handle = created.opened.map(|o| o.handle).unwrap_or(epoch_vfs_api::FileHandle(0));
                let mut off = 0u64;
                while off < data.len() as u64 {
                    let n = vfs.write(WriteRequest {
                        inode_id: id,
                        handle,
                        offset: off,
                        data: &data[off as usize..],
                    })?;
                    if n == 0 {
                        return Err(TargetError::Vfs(VfsError::Io("short write".into())));
                    }
                    off += n as u64;
                }
                vfs.flush(handle)?;
                if fsync {
                    vfs.fsync(id, handle, false)?;
                }
                vfs.release(handle)?;
                Ok(())
            }
        }
    }

    /// pwrite at an offset into an *existing* file — the copy-up trigger for
    /// lower files on the mount side (§2.2 copy-up class).
    pub fn write_at(&self, rel: &Path, offset: u64, data: &[u8]) -> TargetResult<()> {
        match self {
            Target::Host(root) => {
                let mut f = OpenOptions::new().read(true).write(true).open(root.join(rel))?;
                f.seek(SeekFrom::Start(offset))?;
                f.write_all(data)?;
                f.sync_all()?;
                Ok(())
            }
            Target::Vfs { vfs, .. } => {
                let id = self.lookup(rel)?;
                let opened = vfs.open(OpenRequest {
                    inode_id: id,
                    mode: OpenMode::ReadWrite,
                    truncate: false,
                    append: false,
                })?;
                let mut off = offset;
                let mut left = data;
                while !left.is_empty() {
                    let n = vfs.write(WriteRequest { inode_id: id, handle: opened.handle, offset: off, data: left })?;
                    if n == 0 {
                        return Err(TargetError::Vfs(VfsError::Io("short write".into())));
                    }
                    off += n as u64;
                    left = &left[n as usize..];
                }
                vfs.fsync(id, opened.handle, false)?;
                vfs.release(opened.handle)?;
                Ok(())
            }
        }
    }

    pub fn mkdir_all(&self, rel: &Path) -> TargetResult<()> {
        match self {
            Target::Host(root) => Ok(fs::create_dir_all(root.join(rel))?),
            Target::Vfs { vfs, workspace } => {
                let mut cur = PathBuf::from(workspace);
                let mut id = lookup_path(vfs.as_ref(), &cur)?;
                for comp in rel.components() {
                    cur.push(comp.as_os_str());
                    id = match lookup_path(vfs.as_ref(), &cur) {
                        Ok(id) => id,
                        Err(_) => {
                            mkdir_at(vfs.as_ref(), id, comp.as_os_str())?;
                            lookup_path(vfs.as_ref(), &cur)?
                        }
                    };
                }
                Ok(())
            }
        }
    }

    pub fn rename(&self, from: &Path, to: &Path) -> TargetResult<()> {
        match self {
            Target::Host(root) => Ok(fs::rename(root.join(from), root.join(to))?),
            Target::Vfs { vfs, .. } => {
                let (p1, n1) = self.parent_of(from)?;
                let (p2, n2) = self.parent_of(to)?;
                Ok(vfs.rename(p1, &n1, p2, &n2)?)
            }
        }
    }

    pub fn unlink(&self, rel: &Path) -> TargetResult<()> {
        match self {
            Target::Host(root) => Ok(fs::remove_file(root.join(rel))?),
            Target::Vfs { vfs, .. } => {
                let (parent, name) = self.parent_of(rel)?;
                Ok(vfs.unlink(parent, &name)?)
            }
        }
    }

    /// Recursive readdir + getattr over the tree under `rel`; returns
    /// entries visited. The §2.2 stat-walk class.
    pub fn stat_walk(&self, rel: &Path) -> TargetResult<u64> {
        match self {
            Target::Host(root) => {
                let mut count = 0u64;
                let mut stack = vec![root.join(rel)];
                while let Some(dir) = stack.pop() {
                    for entry in fs::read_dir(&dir)? {
                        let entry = entry?;
                        let meta = entry.metadata()?; // getattr per entry
                        count += 1;
                        if meta.is_dir() {
                            stack.push(entry.path());
                        }
                    }
                }
                Ok(count)
            }
            Target::Vfs { vfs, workspace } => {
                let mut count = 0u64;
                let start = Path::new(workspace).join(rel);
                let mut stack = vec![lookup_path(vfs.as_ref(), &start)?];
                while let Some(dir) = stack.pop() {
                    for entry in vfs.readdir(dir)? {
                        // VFS readdir includes `.`/`..` (FUSE contract) — skip
                        // or the walk cycles forever.
                        if entry.name == "." || entry.name == ".." {
                            continue;
                        }
                        let attr = vfs.getattr(entry.inode_id)?;
                        count += 1;
                        if attr.kind == NodeKind::Directory {
                            stack.push(entry.inode_id);
                        }
                    }
                }
                Ok(count)
            }
        }
    }

    /// One-level readdir of `rel` (first-readdir-of-root spin-up probe).
    pub fn stat_walk_shallow(&self, rel: &Path) -> TargetResult<u64> {
        match self {
            Target::Host(root) => Ok(fs::read_dir(root.join(rel))?.count() as u64),
            Target::Vfs { vfs, workspace } => {
                let id = lookup_path(vfs.as_ref(), &Path::new(workspace).join(rel))?;
                Ok(vfs.readdir(id)?.len() as u64)
            }
        }
    }

    /// Host path of the workspace, when this target is reachable via fs
    /// (bare lower always; real FUSE mount; None for in-process).
    pub fn host_path(&self) -> Option<PathBuf> {
        match self {
            Target::Host(root) => Some(root.clone()),
            Target::Vfs { .. } => None,
        }
    }
}

/// Walk `path` component by component from the root inode.
fn lookup_path(vfs: &dyn VfsService, path: &Path) -> VfsResult<InodeId> {
    let mut id = ROOT_INODE_ID;
    for comp in path.components() {
        let name = match comp {
            std::path::Component::RootDir => continue,
            std::path::Component::Normal(n) => n,
            _ => return Err(VfsError::InvalidInput("no . or .. in bench paths".into())),
        };
        id = vfs.lookup(id, name)?.attr.inode_id;
    }
    Ok(id)
}

fn mkdir_at(vfs: &dyn VfsService, parent: InodeId, name: &OsStr) -> VfsResult<()> {
    vfs.mkdir(CreateRequest::directory(parent, name.to_os_string(), 0o755))?;
    Ok(())
}
