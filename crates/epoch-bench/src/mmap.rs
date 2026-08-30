//! MMAP SMOKE — §2.4.4: one number. sqlite with mmap enabled, open + bulk
//! insert + aggregate query, inside the mount vs the bare lower. Uses the
//! system `sqlite3` CLI via subprocess (no rusqlite dependency). Default FUSE
//! mode has no writeback cache, so mmap'd writes traverse the write path
//! page-by-page — this row exists to price that (§1.6).

use std::process::Command;
use std::time::Duration;

use crate::bench::{Ceiling, Pair, Row};
use crate::micro::classify;
use crate::resource;
use crate::target::{Target, TargetResult};

// journal_mode=TRUNCATE (classic rollback journal): WAL requires a working
// mmap'd -shm file, which default FUSE mounts don't guarantee — the smoke
// number must measure mmap reads + journaled writes, not WAL prerequisites.
const SCRIPT: &str = "PRAGMA mmap_size=268435456;\n\
     PRAGMA journal_mode=TRUNCATE;\n\
     CREATE TABLE t(k INTEGER PRIMARY KEY, v BLOB);\n\
     WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<20000)\n\
     INSERT INTO t SELECT x, randomblob(1024) FROM c;\n\
     CREATE INDEX tv ON t(k);\n\
     SELECT sum(length(v)) FROM t;";

fn sqlite3_path() -> Option<std::path::PathBuf> {
    let out = Command::new("which").arg("sqlite3").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then(|| std::path::PathBuf::from(p))
}

fn measure(name: &str, target: &Target, daemon: Option<u32>) -> Row {
    let Some(root) = target.host_path() else {
        return Row::skip(name, "sqlite3 is a subprocess: needs a kernel-visible path");
    };
    let Some(sqlite) = sqlite3_path() else {
        return Row::skip(name, "no sqlite3 binary on PATH");
    };
    let dir = root.join("ws").join("mmap");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Row::fail(name, e.to_string());
    }
    let db = dir.join("bench.db");
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(db.with_extension("db-wal"));
    let mut out: TargetResult<Duration> = Ok(Duration::ZERO);
    let res = resource::capture(daemon, None, || {
        let t = crate::bench::Timer::start();
        match Command::new(sqlite).arg(&db).arg(SCRIPT).output() {
            Ok(o) if o.status.success() => out = Ok(t.stop()),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                out = Err(crate::target::TargetError::Vfs(epoch_vfs_api::VfsError::Io(format!(
                    "sqlite3 exited {}: {stderr}",
                    o.status
                ))));
            }
            Err(e) => out = Err(e.into()),
        }
    });
    match out {
        Ok(wall) => Row::ok(name, 1, wall)
            .with_resources(res)
            .with_ceiling(Ceiling::None)
            .with_note("sqlite3 mmap_size=256MiB: create + 20k×1KiB insert + aggregate"),
        Err(e) => classify(name, &e),
    }
}

pub fn run(bare: &Target, mount: &Target, daemon_pid: Option<u32>) -> Vec<Pair> {
    vec![Pair::new(
        measure("mmap-sqlite", bare, None),
        measure("mmap-sqlite", mount, daemon_pid),
    )]
}
