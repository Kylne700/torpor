//! `torpor status` (spec section 8): the health of one folder in plain words, and where problems come from.
//! Reads only: the store, faults.log, `<claude_dir>/sessions/*.json` (pid, cwd, sessionId) and transcript mtimes.
//! It writes one thing: it marks the faults it has shown as seen.
use crate::capture::{NORMAL_CAP, PRECIOUS_CAP, SECRET_CAP};
use crate::store::*;
use crate::util::*;
use crate::{retention, scan, turn};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct LastTurn {
    pub n: i64,
    pub preview: Option<String>,
    pub edits: i64,
    pub shell: i64,
    pub sub: i64,
}

#[derive(Default)]
pub struct Report {
    pub off: bool,
    /// Something stops saving right now (disk full, hooks not running).
    pub blocking: bool,
    pub points: i64,
    pub oldest: Option<i64>,
    pub newest: Option<i64>,
    pub bytes: i64,
    pub last_turn: Option<LastTurn>,
    /// (path, why)
    pub not_saved: Vec<(String, String)>,
    pub not_saved_total: usize,
    pub problems: Vec<String>,
    pub notes: Vec<String>,
    pub mode: Option<String>,
    pub scan_ms: Option<i64>,
    pub file_count: Option<i64>,
    pub retain_days: i64,
    /// The reach comes from `torpor retain`, not from Claude Code's setting.
    pub retain_custom: bool,
    pub promises: PromiseStats,
    pub now: i64,
}

/// The promises that count (not dropped, not still being qualified): how many, how many passed last time, how many failures they caught.
#[derive(Default)]
pub struct PromiseStats {
    pub total: usize,
    pub ok: usize,
    pub caught: i64,
    /// (label, count) in the order blocking, shadow, advisory, guard only.
    pub tiers: Vec<(&'static str, usize)>,
}

fn promise_stats(db: &Connection) -> Result<PromiseStats> {
    let mut s = PromiseStats::default();
    let mut counts = [0usize; 4];
    for p in crate::promise::all(db, false)?.iter().filter(|p| p.tier != "proposed") {
        s.total += 1;
        s.caught += p.catches;
        s.ok += crate::promise::last_run(db, p.id).is_some_and(|(v, _)| v == "PASS") as usize;
        if let Some(i) = ["blocking", "shadow", "advisory", "unproven"].iter().position(|t| *t == p.tier) {
            counts[i] += 1;
        }
    }
    s.tiers = ["blocking", "shadow", "advisory", "guard only"].into_iter().zip(counts).filter(|(_, n)| *n > 0).collect();
    Ok(s)
}

struct Fault {
    at: i64,
    event: String,
    kind: String,
    detail: String,
}

fn fault_text(kind: &str, event: &str, detail: &str, at: i64, count: usize) -> String {
    let when = local_hhmm(at);
    let ev = if event.is_empty() || event == "-" { "a hook".to_string() } else { event.to_string() };
    let more = if detail.is_empty() { String::new() } else { format!(" ({detail})") };
    let base = match kind {
        "panic" => format!("A Torpor hook crashed during {ev} at {when}. Nothing was changed."),
        "timeout" => format!("A Torpor hook ran out of time during {ev} at {when} and was stopped. Nothing was changed."),
        "parse" => format!("Torpor could not read what Claude Code sent during {ev} at {when}."),
        "disk" => format!("Save points were paused at {when}: the disk is almost full{more}."),
        "busy" => format!("Torpor skipped {ev} at {when} because its records were busy."),
        "io" => format!("Torpor could not read or write a file during {ev} at {when}{more}."),
        "launcher" => format!("Torpor's launcher could not start Torpor{more}. Hooks do nothing until this is fixed."),
        "schema" => format!("Torpor could not use its records for this project at {when}{more}."),
        "key" => format!("Torpor's key file had a problem at {when}{more}."),
        other => format!("Torpor hit a problem ({other}) during {ev} at {when}{more}."),
    };
    if count > 1 { format!("{base} ({count} times)") } else { base }
}

/// Faults not yet seen: the table rows plus the lines of faults.log (for this folder or for no folder) that the table lacks.
/// Returns the faults and the highest timestamp and row id covered.
fn unseen_faults(ws: &Ws, db: &Connection) -> Result<(Vec<Fault>, i64, i64)> {
    let mut out: Vec<Fault> = Vec::new();
    let (mut max_at, mut max_id) = (0i64, 0i64);
    let mut st = db.prepare("SELECT id,at,COALESCE(event,''),kind,COALESCE(detail,'') FROM faults WHERE seen=0 ORDER BY id")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, Fault { at: r.get(1)?, event: r.get(2)?, kind: r.get(3)?, detail: r.get(4)? })))?;
    for r in rows {
        let (id, f) = r?;
        max_id = max_id.max(id);
        max_at = max_at.max(f.at);
        out.push(f);
    }
    let seen_at = meta_get_i64(db, "log_seen_at").unwrap_or(0);
    let table_len = out.len();
    if let Ok(bytes) = std::fs::read(ws.home.join("faults.log")) {
        for line in String::from_utf8_lossy(&bytes).lines().rev().take(2000) {
            let mut it = line.splitn(5, ' ');
            let (Some(ms), Some(wsid), Some(event), Some(kind)) = (it.next(), it.next(), it.next(), it.next()) else { continue };
            let Ok(ms) = ms.parse::<i64>() else { continue };
            let at = ms.saturating_mul(1_000_000);
            if at <= seen_at || (wsid != ws.id && wsid != "-") {
                continue;
            }
            max_at = max_at.max(at);
            // The same fault is usually also a table row, written in the same moment.
            let dup = out[..table_len].iter().any(|f| f.kind == kind && (f.event == event || event == "-") && (f.at - at).abs() < 3 * NS_PER_SEC);
            if !dup {
                out.push(Fault { at, event: event.to_string(), kind: kind.to_string(), detail: it.next().unwrap_or("").to_string() });
            }
        }
    }
    out.sort_by_key(|f| f.at);
    Ok((out, max_at, max_id))
}

fn pid_alive(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Live Claude Code sessions in this folder that never ran a Torpor hook.
fn sessions_without_hooks(ws: &Ws, db: &Connection) -> usize {
    let Ok(rd) = std::fs::read_dir(claude_dir().join("sessions")) else { return 0 };
    let mut n = 0;
    for ent in rd.flatten().take(200) {
        let p = ent.path();
        // Only `*.json`. A `*.key` file is never opened.
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let (Some(pid), Some(cwd), Some(sid)) = (v.get("pid").and_then(|x| x.as_i64()), v.get("cwd").and_then(|x| x.as_str()), v.get("sessionId").and_then(|x| x.as_str())) else { continue };
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.into());
        if !cwd.starts_with(&ws.root) || !pid_alive(pid) {
            continue;
        }
        let beats: i64 = db.query_row("SELECT COUNT(*) FROM heartbeat WHERE session_id=?1", [sid], |r| r.get(0)).unwrap_or(0);
        if beats == 0 {
            n += 1;
        }
    }
    n
}

/// Open sessions whose last hook was a Stop and whose transcript moved on afterwards: hooks stopped running.
fn silent_sessions(db: &Connection, now: i64) -> Vec<i64> {
    let mut out = Vec::new();
    let Ok(mut st) = db.prepare("SELECT s.transcript_path, h.event, h.last_at FROM sessions s JOIN heartbeat h ON h.session_id=s.id WHERE s.ended_at IS NULL AND s.transcript_path IS NOT NULL AND h.last_at=(SELECT MAX(last_at) FROM heartbeat WHERE session_id=s.id) GROUP BY s.id") else { return out };
    let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))) else { return out };
    for (path, event, last) in rows.flatten() {
        if event != "Stop" && event != "StopFailure" {
            continue;
        }
        let mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i64);
        if let Some(m) = mtime {
            if m > last + 120 * NS_PER_SEC && now - m < 86_400 * NS_PER_SEC {
                out.push(last);
            }
        }
    }
    out
}

/// Folder of Claude Code's managed (organization) settings: CLAUDE_CODE_MANAGED_SETTINGS_PATH, else the folder of this OS.
fn managed_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CODE_MANAGED_SETTINGS_PATH") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ if cfg!(target_os = "macos") => PathBuf::from("/Library/Application Support/ClaudeCode"),
        _ => PathBuf::from("/etc/claude-code"),
    }
}

/// Source 4: the settings files that can stop hooks, read only. Returns (problems, notes).
/// Later files win, as in Claude Code: user, project, local, managed. Only four keys are looked at; nothing else is read out.
fn settings_findings(root: &Path) -> (Vec<String>, Vec<String>) {
    const MANAGED: &str = "your organization's managed settings";
    let md = managed_dir();
    let mut files: Vec<(&str, PathBuf)> = vec![
        ("your user settings", claude_dir().join("settings.json")),
        ("this project's settings", root.join(".claude").join("settings.json")),
        ("this project's local settings", root.join(".claude").join("settings.local.json")),
        (MANAGED, md.join("managed-settings.json")),
    ];
    if let Ok(rd) = std::fs::read_dir(md.join("managed-settings.d")) {
        let mut extra: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
        extra.sort();
        files.extend(extra.into_iter().take(20).map(|p| (MANAGED, p)));
    }
    let (mut off, mut only, mut strict, mut plugin): (Option<(bool, &str)>, Option<(bool, &str)>, Option<(bool, &str)>, Option<(bool, &str)>) = (None, None, None, None);
    for (scope, path) in &files {
        let Some(v) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
        if let Some(b) = v.get("disableAllHooks").and_then(|x| x.as_bool()) {
            off = Some((b, scope));
        }
        if let Some(b) = v.pointer("/enabledPlugins/torpor@kylne700").and_then(|x| x.as_bool()) {
            plugin = Some((b, scope));
        }
        // Claude Code honours these two in managed settings only.
        if *scope == MANAGED {
            if let Some(b) = v.get("allowManagedHooksOnly").and_then(|x| x.as_bool()) {
                only = Some((b, scope));
            }
            let locked = match v.get("strictPluginOnlyCustomization") {
                Some(Value::Bool(b)) => Some(*b),
                Some(Value::Array(a)) => Some(a.iter().any(|x| x == "hooks")),
                Some(Value::Object(o)) => Some(!o.is_empty()),
                _ => None,
            };
            if let Some(b) = locked {
                strict = Some((b, scope));
            }
        }
    }
    let (mut problems, mut notes) = (Vec::new(), Vec::new());
    if let Some((true, s)) = off {
        problems.push(format!("Hooks are turned off by disableAllHooks in {s}, so nothing is being saved."));
    }
    if let Some((true, s)) = only {
        problems.push(format!("Only managed hooks may run, set by allowManagedHooksOnly in {s}, so Torpor's hooks do not run and nothing is being saved."));
    }
    if let Some((false, s)) = plugin {
        problems.push(format!("The Torpor plugin is turned off by enabledPlugins in {s}, so nothing is being saved."));
    }
    if let Some((true, s)) = strict {
        notes.push(format!("strictPluginOnlyCustomization is set in {s}. If no Torpor hooks run, ask whoever manages Claude Code here to allow the Torpor plugin."));
    }
    (problems, notes)
}

pub fn why(reason: &str, path: &str, size: Option<i64>) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match reason {
        "too_big" => {
            let cap = if scan::is_secret(path) {
                SECRET_CAP
            } else if scan::is_precious_name(name) {
                PRECIOUS_CAP
            } else {
                NORMAL_CAP
            };
            format!("bigger than {}", human_bytes(cap))
        }
        "cap" => match size {
            Some(s) if s > 0 => format!("left out, this save point reached its size limit ({})", human_bytes(s as u64)),
            _ => "left out, this save point reached its size limit".to_string(),
        },
        "unstable" => "kept changing while it was read".to_string(),
        "unreadable" => "could not be read".to_string(),
        "name" => "its name is not valid text".to_string(),
        "special" => "not a regular file".to_string(),
        "db_busy" => "database in use right now".to_string(),
        "outside_root" => "points outside the project".to_string(),
        "ignored_folder" => "inside a folder your .gitignore skips".to_string(),
        other => other.to_string(),
    }
}

/// Gather everything `status` shows. With `mark_seen` the faults shown are marked seen.
pub fn gather(ws: &Ws, all: bool, mark_seen: bool, now: i64) -> Result<Report> {
    let db = &ws.conn;
    let mut r = Report { now, ..Default::default() };
    r.off = meta_get(db, "off").as_deref() == Some("1") || std::env::var_os("TORPOR_OFF").is_some_and(|v| !v.is_empty());
    let (n, oldest, newest): (i64, Option<i64>, Option<i64>) =
        db.query_row("SELECT COUNT(*),MIN(created_at),MAX(created_at) FROM points WHERE dropped=0 AND visible=1", [], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)))?;
    r.points = n;
    r.oldest = oldest;
    r.newest = newest;
    r.bytes = retention::store_bytes(db);
    r.mode = meta_get(db, "mode");
    r.scan_ms = meta_get_i64(db, "scan_ms");
    r.file_count = meta_get_i64(db, "file_count");
    let (w, note) = retention::window_days(db);
    r.retain_days = w;
    r.retain_custom = meta_get_i64(db, "retain_days").is_some_and(|x| x >= w);
    r.notes.extend(note);

    // Last turn that exists, with what it changed.
    if let Some((id, tn, preview)) = db.query_row("SELECT id,n,preview FROM turns ORDER BY id DESC LIMIT 1", [], |x| Ok((x.get::<_, i64>(0)?, x.get::<_, i64>(1)?, x.get::<_, Option<String>>(2)?))).optional()? {
        let (edits, shell, sub) = turn::net_counts(db, id)?;
        r.last_turn = Some(LastTurn { n: tn, preview, edits, shell, sub });
    }

    r.promises = promise_stats(db)?;

    // Not saved: what the newest save point could not take.
    let newest_id: Option<i64> = db.query_row("SELECT MAX(id) FROM points WHERE dropped=0", [], |x| x.get(0)).optional()?.flatten();
    if let Some(pid) = newest_id {
        let mut st = db.prepare("SELECT path,reason,size FROM not_captured WHERE point_id=?1 ORDER BY path")?;
        let rows: Vec<(String, String, Option<i64>)> = st.query_map([pid], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
        r.not_saved_total = rows.len();
        let keep = if all { usize::MAX } else { 5 };
        r.not_saved = rows.iter().take(keep).map(|(p, why_code, size)| (p.clone(), why(why_code, p, *size))).collect();
        let partial: i64 = db.query_row("SELECT COUNT(*) FROM points WHERE dropped=0 AND partial=1", [], |x| x.get(0))?;
        if partial > 0 {
            r.notes.push(format!("{partial} save point{} could not take every file (a size or count limit was reached).", if partial == 1 { "" } else { "s" }));
        }
    }

    // Problems.
    let (faults, max_at, max_id) = unseen_faults(ws, db)?;
    let mut groups: Vec<(String, String, String, i64, usize)> = Vec::new(); // kind, event, detail, last at, count
    for f in &faults {
        match groups.iter_mut().find(|g| g.0 == f.kind && g.1 == f.event) {
            Some(g) => {
                g.2 = f.detail.clone();
                g.3 = f.at;
                g.4 += 1;
            }
            None => groups.push((f.kind.clone(), f.event.clone(), f.detail.clone(), f.at, 1)),
        }
    }
    for (kind, event, detail, at, count) in &groups {
        r.problems.push(fault_text(kind, event, detail, *at, *count));
    }
    if let Err(Error::Disk(d)) = check_disk(&ws.home) {
        r.problems.push(format!("Save points are paused: the disk is almost full ({d})."));
        r.blocking = true;
    }
    let beats: i64 = db.query_row("SELECT COUNT(*) FROM heartbeat", [], |x| x.get(0))?;
    if beats == 0 {
        r.problems.push("No Torpor hook has run in this folder yet. If Claude Code asked whether you trust this folder, answer yes; hooks wait for that.".to_string());
        r.blocking = true;
    }
    let live = sessions_without_hooks(ws, db);
    if live > 0 {
        r.problems.push(format!("{live} Claude Code session{} in this folder ha{} not run any Torpor hook, so nothing from {} is being saved. Check that the Torpor plugin is turned on.", if live == 1 { "" } else { "s" }, if live == 1 { "s" } else { "ve" }, if live == 1 { "it" } else { "them" }));
        r.blocking = true;
    }
    for last in silent_sessions(db, now) {
        r.problems.push(format!("No Torpor hooks ran in this session since {}.", local_hhmm(last)));
        r.blocking = true;
    }
    let (found, notes) = settings_findings(&ws.root);
    r.blocking |= !found.is_empty();
    r.problems.extend(found);
    r.notes.extend(notes);
    if let Some(n) = crate::hook::lost_key_files(db) {
        let before: i64 = match Key::load().ok().flatten() {
            Some(k) => db.query_row("SELECT COALESCE(MIN(created_at),?2) FROM blobs WHERE kind='secret' AND key_id=?1", params![k.id, now], |x| x.get(0)).unwrap_or(now),
            None => now,
        };
        r.problems.push(format!("{n} secret file{} saved before {} can't be restored: their key file is gone.", if n == 1 { "" } else { "s" }, local_md(before)));
    }
    r.problems.extend(deep_check_lines(db));
    if meta_get_i64(db, "thinned_harder_at").is_some_and(|t| now - t < 14 * retention::DAY_NS) {
        r.notes.push("Torpor reached its 2 GB limit for this project and kept fewer old save points.".to_string());
    }
    if meta_get(db, "texts_off").as_deref() == Some("1") {
        r.notes.push("Torpor keeps no prompt text because CLAUDE_CODE_SKIP_PROMPT_HISTORY is set, so it has no goal words or constraints.".to_string());
    }

    if mark_seen && (max_id > 0 || max_at > 0) {
        // Best effort: a busy database just means the faults are shown again next time.
        let _ = (|| -> Result<()> {
            db.busy_timeout(std::time::Duration::from_millis(200))?;
            let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
            tx.execute("UPDATE faults SET seen=1 WHERE seen=0 AND id<=?1", [max_id])?;
            let prev = meta_get_i64(&tx, "log_seen_at").unwrap_or(0);
            meta_set(&tx, "log_seen_at", &prev.max(max_at).to_string())?;
            tx.commit()?;
            Ok(())
        })();
    }
    Ok(r)
}

/// Precious files inside folders `.gitignore` skips, as found by the first scan (up to 3 lines and a count).
pub fn deep_check_lines(db: &Connection) -> Vec<String> {
    let list: Vec<(String, String)> = meta_get(db, "deep_check_json").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let mut out: Vec<String> = list
        .iter()
        .take(3)
        .map(|(file, folder)| format!("{file} is inside a folder your .gitignore skips, so it is not saved. To save it, add the line !{folder} to a file named .torporignore."))
        .collect();
    if list.len() > 3 {
        out.push(format!("{} more files are inside folders your .gitignore skips.", list.len() - 3));
    }
    out
}

/// Faults for a folder that has no store yet (nothing ran there, or only the launcher did): faults.log lines for no folder.
pub fn global_faults(home: &Path) -> Vec<String> {
    let mut groups: Vec<(String, String, String, i64, usize)> = Vec::new();
    if let Ok(bytes) = std::fs::read(home.join("faults.log")) {
        for line in String::from_utf8_lossy(&bytes).lines().rev().take(2000) {
            let mut it = line.splitn(5, ' ');
            let (Some(ms), Some(wsid), Some(event), Some(kind)) = (it.next(), it.next(), it.next(), it.next()) else { continue };
            let (Ok(ms), "-") = (ms.parse::<i64>(), wsid) else { continue };
            let at = ms.saturating_mul(1_000_000);
            match groups.iter_mut().find(|g| g.0 == kind && g.1 == event) {
                Some(g) => g.4 += 1,
                None => groups.push((kind.to_string(), event.to_string(), it.next().unwrap_or("").to_string(), at, 1)),
            }
        }
    }
    groups.iter().map(|(k, e, d, at, c)| fault_text(k, e, d, *at, *c)).collect()
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn render(r: &Report) -> String {
    let mut o = String::new();
    o.push_str(if r.off {
        "Torpor is turned off for this folder.\n"
    } else if r.blocking {
        "Torpor is not saving this project right now.\n"
    } else {
        "Torpor is saving this project.\n"
    });
    match (r.oldest, r.newest) {
        (Some(old), Some(new)) => o.push_str(&format!("Save points: {} (oldest {}), last one {}, using {}.\n", r.points, local_stamp(old, r.now), ago(r.now, new), human_bytes(r.bytes.max(0) as u64))),
        _ => o.push_str("Save points: none yet.\n"),
    }
    if r.promises.total > 0 {
        let tiers: Vec<String> = r.promises.tiers.iter().map(|(t, n)| format!("{n} {t}")).collect();
        o.push_str(&format!("Promises: {} ({}) · caught {}\n", r.promises.total, tiers.join(", "), r.promises.caught));
    }
    if let Some(t) = &r.last_turn {
        let total = t.edits + t.shell + t.sub;
        let what = match &t.preview {
            Some(p) => format!(" \"{p}\""),
            None => String::new(),
        };
        if total == 0 {
            o.push_str(&format!("Last turn: {}{what} changed no files.\n", t.n));
        } else {
            let mut parts: Vec<String> = Vec::new();
            if t.edits > 0 {
                parts.push(plural(t.edits, "edit", "edits"));
            }
            if t.shell > 0 {
                parts.push(plural(t.shell, "shell command", "shell commands"));
            }
            if t.sub > 0 {
                parts.push(plural(t.sub, "subagent edit", "subagent edits"));
            }
            o.push_str(&format!("Last turn: {}{what} changed {} ({}).\n", t.n, plural(total, "file", "files"), parts.join(", ")));
        }
    }
    if let Some(m) = &r.mode {
        let size = match (r.scan_ms, r.file_count) {
            (Some(ms), Some(n)) => format!(" (last scan {ms} ms, {n} files)"),
            _ => String::new(),
        };
        o.push_str(&if m == "fast" {
            format!("Checks: after every shell command{size}.\n")
        } else {
            format!("Checks: at the end of each turn{size}.\n")
        });
    }
    if !r.not_saved.is_empty() {
        let list: Vec<String> = r.not_saved.iter().map(|(p, w)| format!("{p} ({w})")).collect();
        o.push_str(&format!("Not saved: {}", list.join(", ")));
        let more = r.not_saved_total.saturating_sub(r.not_saved.len());
        if more > 0 {
            o.push_str(&format!(" and {more} more: torpor status --all"));
        }
        o.push_str(".\n");
    }
    if r.problems.is_empty() {
        o.push_str("Problems: none.\n");
    } else {
        o.push_str("Problems:\n");
        for p in &r.problems {
            o.push_str(&format!("  - {p}\n"));
        }
    }
    for n in &r.notes {
        o.push_str(&format!("{n}\n"));
    }
    let by = if r.retain_custom { "set with torpor retain" } else { "your Claude Code cleanup setting" };
    o.push_str(&format!("Undo and back reach back {} days ({by}).\n", r.retain_days));
    if let Some(t) = &r.last_turn {
        o.push_str(&format!("Undo the last turn: torpor undo {}. Go back one step: torpor back 1.\n", t.n));
    }
    o
}

pub fn to_json(r: &Report) -> Value {
    let ms = |t: Option<i64>| t.map(|x| x / 1_000_000);
    json!({
        "saving": !r.off && !r.blocking,
        "off": r.off,
        "points": r.points,
        "oldest_at_ms": ms(r.oldest),
        "last_at_ms": ms(r.newest),
        "bytes": r.bytes,
        "mode": r.mode,
        "scan_ms": r.scan_ms,
        "file_count": r.file_count,
        "retain_days": r.retain_days,
        "last_turn": r.last_turn.as_ref().map(|t| json!({"n": t.n, "preview": t.preview, "edits": t.edits, "shell": t.shell, "subagent": t.sub})),
        "not_saved": r.not_saved.iter().map(|(p, w)| json!({"path": p, "why": w})).collect::<Vec<_>>(),
        "not_saved_total": r.not_saved_total,
        "problems": r.problems,
        "notes": r.notes,
        "promises": {"total": r.promises.total, "ok": r.promises.ok, "caught": r.promises.caught, "tiers": r.promises.tiers.iter().map(|(t, n)| json!({"tier": t, "n": n})).collect::<Vec<_>>()},
    })
}
