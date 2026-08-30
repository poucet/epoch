//! `epoch` — the store-side companion to `epoch-fuse`: the lane lifecycle's
//! landing verbs and the audit surface. No mount needed: every verb reads a
//! state dir's `epoch.redb` directly (checkpointed state only — anything not
//! yet checkpointed is, by design, invisible).
//!
//! ```text
//! epoch diff   <state-dir> [--head <name>] [--base <event-hex>] [--lower <dir>]
//! epoch export <state-dir> [--head <name>] [--lower <dir>] <dest-dir>
//! epoch audit  <state-dir> [--head <name>] [--lower <dir>]
//! ```
//!
//! All output is schema-versioned JSON on stdout. See
//! `docs/reference/EXPORT-AUDIT.md` for the schema and the audit claims.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use epoch_cas_redb::RedbByteBackend;
use epoch_history::{AuditRecord, DiffEntry, MaterializeStats, audit_log, diff_roots, materialize};
use epoch_timeline::{Attribution, EventId, TimelineStore, escape_json_string_into};
use epoch_timeline_redb::RedbTimelineStore;

const USAGE: &str = "usage:
  epoch diff   <state-dir> [--head <name>] [--base <event-hex>] [--lower <dir>]
               JSON diff of the head's checkpoint vs the lower base
               (or vs an explicit base event)
  epoch export <state-dir> [--head <name>] [--lower <dir>] <dest-dir>
               materialize the head's merged snapshot (overlay over lower,
               tombstones honored) into an empty <dest-dir>
  epoch audit  <state-dir> [--head <name>] [--lower <dir>]
               JSON write log: the diff of every consecutive first-parent
               event pair, timestamped and mount-attributed

All verbs are store-side: they read <state-dir>/epoch.redb directly and need
no live mount. redb is single-writer, so a state dir held by a RUNNING mount
cannot be opened — unmount first, or copy the state dir (e.g. `cp -Rc`) and
point the verb at the copy. Only checkpointed state is visible.

--lower defaults to the lower path recorded in the head event's attribution
meta (mounts record it since schema v1). Paths in JSON are rendered lossily
(unix names are arbitrary bytes; JSON strings are not).";

/// The JSON schema version stamped as `"schema"` on every output document.
const OUTPUT_SCHEMA: u32 = 1;

/// The audit surface's claims, embedded in every audit document so a consumer
/// never has to guess what the log does and does not promise.
const AUDIT_CLAIMS: &str = "complete for writes and deletes of paths under the mountpoint captured into checkpoints; timestamps are checkpoint-debounce resolution, not per-op; reads are not recorded; the overlay is not a sandbox — this audit is scoped to accidents and review";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("epoch: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let verb = args.next().ok_or(USAGE)?;
    let verb = verb.to_str().ok_or(USAGE)?;
    if !matches!(verb, "diff" | "export" | "audit") {
        return Err(USAGE.into());
    }

    let mut head = String::from("main");
    let mut base: Option<String> = None;
    let mut lower: Option<PathBuf> = None;
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--head") => {
                head = args
                    .next()
                    .and_then(|s| s.to_str().map(str::to_owned))
                    .ok_or(USAGE)?;
            }
            Some("--base") => {
                base = Some(
                    args.next()
                        .and_then(|s| s.to_str().map(str::to_owned))
                        .ok_or(USAGE)?,
                );
            }
            Some("--lower") => lower = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    let state_dir = positional.first().cloned().ok_or(USAGE)?;

    let (backend, timeline) = open_state(&state_dir)?;
    let trees = epoch_tree::TreeStore::new(backend.clone());
    let files = epoch_file_cas::FileCas::new(backend);

    let head_event_id = timeline
        .get_head(&head)?
        .ok_or_else(|| format!("unknown head {head:?} (no checkpoints on it in this state dir)"))?;
    let head_event = timeline
        .get(&head_event_id)?
        .ok_or_else(|| format!("head event {head_event_id} missing from the store"))?;

    // The lower: explicit flag wins; otherwise the attribution the mount
    // recorded on its latest checkpoint.
    let recorded = head_event
        .meta
        .as_deref()
        .and_then(Attribution::parse)
        .and_then(|a| a.lower.map(PathBuf::from));
    let lower = lower.or(recorded);

    match verb {
        "diff" => {
            let (old_root, base_json) = match &base {
                Some(hex) => {
                    let id: EventId = hex.parse().map_err(|_| "bad --base event id")?;
                    let event = timeline
                        .get(&id)?
                        .ok_or_else(|| format!("base event {id} missing from the store"))?;
                    (Some(event.root), format!("{{\"kind\":\"event\",\"event\":\"{id}\"}}"))
                }
                None => (
                    None,
                    match &lower {
                        Some(l) => format!(
                            "{{\"kind\":\"lower\",\"lower\":{}}}",
                            json_string(&l.display().to_string())
                        ),
                        None => "{\"kind\":\"lower\",\"lower\":null}".to_string(),
                    },
                ),
            };
            if base.is_none() && lower.is_none() {
                eprintln!(
                    "epoch: note: no lower known (no --lower, none recorded) — \
                     treating the base as empty; renames across the lower boundary \
                     and restore-vs-delete are not distinguishable"
                );
            }
            let entries = diff_roots(&trees, &files, old_root.as_ref(), &head_event.root, lower.as_deref())?;
            let mut out = format!(
                "{{\"schema\":{OUTPUT_SCHEMA},\"type\":\"diff\",\"state\":{},\"head\":{},\"head_event\":\"{head_event_id}\",\"base\":{base_json},\"entries\":[",
                json_string(&state_dir.display().to_string()),
                json_string(&head),
            );
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&entry_json(entry));
            }
            out.push_str("]}");
            println!("{out}");
        }
        "export" => {
            let dest = positional.get(1).ok_or(USAGE)?;
            if lower.is_none() {
                eprintln!(
                    "epoch: note: no lower known (no --lower, none recorded) — \
                     exporting overlay content only"
                );
            }
            let stats: MaterializeStats =
                materialize(&trees, &files, &head_event.root, lower.as_deref(), dest)?;
            println!(
                "{{\"schema\":{OUTPUT_SCHEMA},\"type\":\"export\",\"state\":{},\"head\":{},\"head_event\":\"{head_event_id}\",\"dest\":{},\"files\":{},\"dirs\":{},\"symlinks\":{},\"bytes\":{}}}",
                json_string(&state_dir.display().to_string()),
                json_string(&head),
                json_string(&dest.display().to_string()),
                stats.files,
                stats.dirs,
                stats.symlinks,
                stats.bytes,
            );
        }
        "audit" => {
            let records: Vec<AuditRecord> =
                audit_log(&timeline, &trees, &files, &head, lower.as_deref())?;
            let mut out = format!(
                "{{\"schema\":{OUTPUT_SCHEMA},\"type\":\"audit\",\"state\":{},\"head\":{},\"claims\":{},\"entries\":[",
                json_string(&state_dir.display().to_string()),
                json_string(&head),
                json_string(AUDIT_CLAIMS),
            );
            let mut first = true;
            for record in &records {
                let (mount, user, host) = match &record.attribution {
                    Some(a) => (
                        json_string(&a.mount),
                        a.user.as_deref().map_or("null".into(), json_string),
                        a.host.as_deref().map_or("null".into(), json_string),
                    ),
                    None => ("null".into(), "null".into(), "null".into()),
                };
                for entry in &record.entries {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    let fields = entry_json(entry);
                    // Splice the per-event fields in front of the op fields.
                    out.push_str(&format!(
                        "{{\"ts\":{},\"event\":\"{}\",\"mount\":{mount},\"user\":{user},\"host\":{host},",
                        record.timestamp_ms, record.event,
                    ));
                    out.push_str(&fields[1..]);
                }
            }
            out.push_str("]}");
            println!("{out}");
        }
        _ => unreachable!("verb checked above"),
    }
    Ok(())
}

/// Open a state dir's shared redb and build the two read faces over it.
///
/// The honest failure mode is part of the contract: redb is single-writer, so
/// a live mount's state dir refuses to open and we say what to do about it.
fn open_state(state_dir: &Path) -> Result<(RedbByteBackend, RedbTimelineStore), Box<dyn Error>> {
    let db_path = state_dir.join("epoch.redb");
    if !db_path.is_file() {
        return Err(format!("no epoch state at {} (expected <state-dir>/epoch.redb)", db_path.display()).into());
    }
    let db = match redb::Database::create(&db_path) {
        Ok(db) => db,
        Err(err) => {
            let locked = matches!(err, redb::DatabaseError::DatabaseAlreadyOpen);
            let hint = if locked {
                " — the state dir is held by a RUNNING mount; unmount it, or copy \
                 the state dir (`cp -Rc`) and point the verb at the copy"
            } else {
                ""
            };
            return Err(format!("cannot open {}: {err}{hint}", db_path.display()).into());
        }
    };
    let db = Arc::new(db);
    let backend = RedbByteBackend::with_database(Arc::clone(&db))?;
    let timeline = RedbTimelineStore::new(db)?;
    Ok((backend, timeline))
}

/// A quoted, escaped JSON string literal.
fn json_string(s: &str) -> String {
    let mut out = String::new();
    escape_json_string_into(s, &mut out);
    out
}

/// A path as a (lossy) JSON string: unix names are arbitrary bytes.
fn json_path(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    json_string(&String::from_utf8_lossy(p.as_os_str().as_bytes()))
}

/// The JSON object literal for one diff/audit entry (shared by both verbs).
fn entry_json(entry: &DiffEntry) -> String {
    match entry {
        DiffEntry::Write { path, blob, size, mode } => format!(
            "{{\"op\":\"write\",\"path\":{},\"content_hash\":\"{blob}\",\"size\":{size},\"mode\":{mode}}}",
            json_path(path)
        ),
        DiffEntry::Delete { path, previous, dir } => format!(
            "{{\"op\":\"delete\",\"path\":{},\"previous_hash\":{},\"dir\":{dir}}}",
            json_path(path),
            previous.map_or("null".into(), |b| format!("\"{b}\"")),
        ),
        DiffEntry::Mkdir { path } => {
            format!("{{\"op\":\"mkdir\",\"path\":{}}}", json_path(path))
        }
        DiffEntry::Rename { from, to, blob, dir } => format!(
            "{{\"op\":\"rename\",\"from\":{},\"to\":{},\"content_hash\":{},\"dir\":{dir}}}",
            json_path(from),
            json_path(to),
            blob.map_or("null".into(), |b| format!("\"{b}\"")),
        ),
        DiffEntry::Restore { path, dir } => {
            format!("{{\"op\":\"restore\",\"path\":{},\"dir\":{dir}}}", json_path(path))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_string_escapes() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(
            json_string("q\"u\\o\nted"),
            "\"q\\\"u\\\\o\\nted\""
        );
    }
}
