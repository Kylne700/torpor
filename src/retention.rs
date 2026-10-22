//! Retention (spec 2.4): the window, thinning, the store cap and garbage collection.
use crate::store::*;
use crate::util::*;
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use std::collections::HashSet;

pub const DAY_NS: i64 = 86_400 * NS_PER_SEC;
/// Unreferenced blobs younger than this are kept: they may belong to a capture that crashed half way.
pub const GRACE_NS: i64 = DAY_NS;
pub const STORE_CAP: i64 = 2 << 30;
const QUIET_NS: i64 = 10 * 60 * NS_PER_SEC;

/// Claude Code's own setting: `cleanupPeriodDays` of the user's settings (read only), 30 if unset, at least 2 (with a note).
pub fn claude_days() -> (i64, Option<String>) {
    let mut w = 30i64;
    let mut note = None;
    if let Ok(s) = std::fs::read_to_string(claude_dir().join("settings.json")) {
        if let Some(d) = serde_json::from_str::<serde_json::Value>(&s).ok().and_then(|v| v.get("cleanupPeriodDays").and_then(|x| x.as_i64())) {
            if d < 2 {
                note = Some(format!("Claude Code is set to keep chats for {d} day(s); Torpor keeps save points for at least 2 days."));
                w = 2;
            } else {
                w = d;
            }
        }
    }
    (w, note)
}

/// W in days: Claude Code's setting, lengthened by `torpor retain <days>`.
pub fn window_days(db: &Connection) -> (i64, Option<String>) {
    let (mut w, note) = claude_days();
    if let Some(r) = meta_get_i64(db, "retain_days") {
        w = w.max(r);
    }
    (w, note)
}

pub fn store_bytes(db: &Connection) -> i64 {
    db.query_row("SELECT COALESCE(SUM(stored_size),0) FROM blobs", [], |r| r.get(0)).unwrap_or(0)
}

/// Drop points by age (spec 2.4). Never the newest point. Returns how many were dropped.
pub fn thin(db: &Connection, now: i64, w_days: i64, harder: bool) -> Result<usize> {
    let newest: i64 = db.query_row("SELECT COALESCE(MAX(id),0) FROM points", [], |r| r.get(0))?;
    let w_ns = w_days * DAY_NS;
    let (keep_all, window) = if harder { (DAY_NS, 7 * DAY_NS) } else { (2 * DAY_NS, w_ns) };
    let mut n = db.execute("UPDATE points SET dropped=1 WHERE dropped=0 AND id<>?1 AND created_at<?2", params![newest, now - w_ns])?;
    if harder {
        n += db.execute("UPDATE points SET dropped=1 WHERE dropped=0 AND pinned=0 AND id<>?1 AND created_at<?2", params![newest, now - window])?;
    }
    n += db.execute("UPDATE points SET dropped=1 WHERE dropped=0 AND kind='tool' AND id<>?1 AND created_at<?2", params![newest, now - keep_all])?;
    // From then on: the last visible, unpinned point of each day.
    let mut st = db.prepare("SELECT id,created_at FROM points WHERE dropped=0 AND visible=1 AND pinned=0 AND created_at<?1 ORDER BY id")?;
    let rows: Vec<(i64, i64)> = st.query_map([now - keep_all], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
    let mut drop_ids: Vec<i64> = Vec::new();
    for (i, (id, at)) in rows.iter().enumerate() {
        let next_same_day = rows.get(i + 1).is_some_and(|(_, at2)| utc_day(*at2) == utc_day(*at));
        if next_same_day && *id != newest {
            drop_ids.push(*id);
        }
    }
    let mut upd = db.prepare("UPDATE points SET dropped=1 WHERE id=?1")?;
    for id in drop_ids {
        n += upd.execute([id])?;
    }
    // Text goes at W; the metadata rows stay.
    db.execute("UPDATE turns SET preview=NULL WHERE preview IS NOT NULL AND started_at<?1", [now - w_ns])?;
    db.execute("UPDATE tool_events SET cmd_preview=NULL WHERE cmd_preview IS NOT NULL AND at<?1", [now - w_ns])?;
    db.execute("UPDATE points SET note=NULL WHERE kind='pre_bash' AND note IS NOT NULL AND created_at<?1", [now - w_ns])?;
    Ok(n)
}

/// Thinning at most once per 24 h (SessionStart). A store over its cap thins harder.
pub fn thin_if_due(db: &Connection, now: i64) -> Result<()> {
    if now - meta_get_i64(db, "last_thin").unwrap_or(0) < DAY_NS {
        return Ok(());
    }
    let (w, _) = window_days(db);
    let harder = store_bytes(db) > STORE_CAP;
    thin(db, now, w, harder)?;
    if harder {
        meta_set(db, "thinned_harder_at", &now.to_string())?;
    }
    meta_set(db, "last_thin", &now.to_string())?;
    Ok(())
}

#[derive(Debug, Default, Clone)]
pub struct GcStats {
    pub blobs: usize,
    pub bytes: u64,
    pub orphan_files: usize,
    pub thinned: usize,
}

pub enum GcOutcome {
    Done(GcStats),
    /// A session was active in the last 10 minutes.
    NotQuiet,
}

/// Every blob id reachable from a point that is not dropped (and from the newest point).
fn mark(ws: &Ws, db: &Connection) -> Result<HashSet<[u8; 32]>> {
    let mut live: HashSet<[u8; 32]> = HashSet::new();
    let mut st = db.prepare("SELECT DISTINCT tree_id FROM points WHERE dropped=0 OR id=(SELECT MAX(id) FROM points)")?;
    let roots: Vec<Vec<u8>> = st.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
    let mut stack: Vec<[u8; 32]> = roots.iter().filter_map(|b| blob_id_from_vec(b)).collect();
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    while let Some(t) = stack.pop() {
        if !seen.insert(t) {
            continue;
        }
        live.insert(t);
        // A tree that cannot be read stops the whole run: better to delete nothing.
        for e in ws.load_tree(db, &t)? {
            match e.kind {
                'd' => {
                    if let Some(id) = unhex32(&e.id) {
                        stack.push(id);
                    }
                }
                'x' => {}
                _ => {
                    if let Some(id) = unhex32(&e.id) {
                        live.insert(id);
                    }
                }
            }
        }
    }
    Ok(live)
}

/// Garbage collection. The caller holds no lock; this takes the exclusive one.
pub fn gc(ws: &mut Ws, if_quiet: bool, now: i64) -> Result<GcOutcome> {
    gc_opts(ws, if_quiet, now, false)
}

/// `force` (forget): no quiet check and no grace period, because the user asked for the records to be gone now.
pub fn gc_opts(ws: &mut Ws, if_quiet: bool, now: i64, force: bool) -> Result<GcOutcome> {
    ws.lock_exclusive(5000)?;
    let grace = if force { 0 } else { GRACE_NS };
    let recent: bool = !force && ws.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM heartbeat h LEFT JOIN sessions s ON s.id=h.session_id WHERE h.last_at>?1 AND (?2=0 OR s.ended_at IS NULL))",
        params![now - QUIET_NS, if_quiet as i64],
        |r| r.get(0),
    )?;
    if recent {
        return Ok(GcOutcome::NotQuiet);
    }
    let mut stats = GcStats::default();
    let mut dead_files: Vec<std::path::PathBuf> = Vec::new();
    {
        let tx = Transaction::new_unchecked(&ws.conn, TransactionBehavior::Immediate)?;
        if store_bytes(&tx) > STORE_CAP {
            let (w, _) = window_days(&tx);
            stats.thinned = thin(&tx, now, w, true)?;
            meta_set(&tx, "thinned_harder_at", &now.to_string())?;
        }
        let live = mark(ws, &tx)?;
        let mut st = tx.prepare("SELECT id,loose,stored_size FROM blobs WHERE created_at<?1")?;
        let rows: Vec<(Vec<u8>, i64, i64)> = st.query_map([now - grace], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
        drop(st);
        let mut del = tx.prepare("DELETE FROM blobs WHERE id=?1")?;
        for (id, loose, size) in rows {
            let Some(arr) = blob_id_from_vec(&id) else { continue };
            if live.contains(&arr) {
                continue;
            }
            del.execute([&id[..]])?;
            if loose != 0 {
                dead_files.push(ws.loose_path(&arr));
            }
            stats.blobs += 1;
            stats.bytes += size.max(0) as u64;
        }
        drop(del);
        // A cached hash must never name a blob that is gone.
        tx.execute("DELETE FROM stat_cache WHERE blob_id NOT IN (SELECT id FROM blobs)", [])?;
        meta_set(&tx, "last_gc", &now.to_string())?;
        tx.commit()?;
    }
    for f in dead_files {
        let _ = std::fs::remove_file(f);
    }
    stats.orphan_files = sweep_orphan_files(ws, now - grace);
    Ok(GcOutcome::Done(stats))
}

/// `torpor forget --before`: drop every point made before `cutoff` (never the newest), the text that names its files and commands, and free the blobs now.
pub fn forget_before(ws: &mut Ws, cutoff: i64, now: i64) -> Result<(usize, GcStats)> {
    ws.lock_exclusive(5000)?;
    let n = {
        let tx = Transaction::new_unchecked(&ws.conn, TransactionBehavior::Immediate)?;
        let newest: i64 = tx.query_row("SELECT COALESCE(MAX(id),0) FROM points", [], |r| r.get(0))?;
        let n = tx.execute("UPDATE points SET dropped=1 WHERE dropped=0 AND id<>?1 AND created_at<?2", params![newest, cutoff])?;
        for t in ["changes", "not_captured"] {
            tx.execute(&format!("DELETE FROM {t} WHERE point_id IN (SELECT id FROM points WHERE dropped=1 AND created_at<?1)"), [cutoff])?;
        }
        tx.execute("UPDATE turns SET preview=NULL WHERE started_at<?1", [cutoff])?;
        tx.execute("UPDATE tool_events SET cmd_preview=NULL,paths_json=NULL WHERE at<?1", [cutoff])?;
        tx.execute("UPDATE points SET note=NULL WHERE id<>?1 AND created_at<?2", params![newest, cutoff])?;
        tx.commit()?;
        n
    };
    match gc_opts(ws, false, now, true)? {
        GcOutcome::Done(s) => Ok((n, s)),
        GcOutcome::NotQuiet => Ok((n, GcStats::default())),
    }
}

/// Loose files that no blob row names (a rolled-back capture, a crash) and temp files, once last written before `before`.
fn sweep_orphan_files(ws: &Ws, before: i64) -> usize {
    let mut n = 0;
    let Ok(top) = std::fs::read_dir(ws.objects_dir()) else { return 0 };
    for d in top.flatten() {
        let Ok(files) = std::fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            let old = f
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|d| (d.as_nanos() as i64) < before);
            if !old {
                continue;
            }
            let temp = name.starts_with(".torpor-") && name.ends_with(".tmp");
            let orphan = !temp && {
                let full = format!("{}{}", d.file_name().to_string_lossy(), name);
                unhex32(&full).is_none_or(|id| !ws.blob_exists(&ws.conn, &id))
            };
            if (temp || orphan) && std::fs::remove_file(f.path()).is_ok() {
                n += 1;
            }
        }
    }
    n
}
