//! Hook mode (spec section 3): `torpor hook <Event>`. Fail-open: never stderr, never exit 2, never `allow`.
use crate::capture::{self, Captured, Mode, PointKind, PointSpec, Req};
use crate::store::*;
use crate::turn::{self, EventRow, SessionInfo, Turn};
use crate::util::*;
use crate::{mask, retention, scan, transcript};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use std::cell::Cell;
use std::io::{Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_STDIN: u64 = 8 << 20;
const LOCK_WAIT_MS: u64 = 100;
const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
pub const STOP_LINE_MAX: usize = 160;

pub const MSG_REFUSED_ROOT: &str = "Torpor does not record your whole home folder. Start Claude Code inside a project folder to get save points.";
pub const MSG_NEWER: &str = "This project's records were written by a newer Torpor. Update the plugin.";
pub const MSG_RESTORE_INTERRUPTED: &str = "Torpor: a restore was interrupted and some files are mixed. Ask the user to run torpor recover before editing.";
pub const MSG_PROTECTED: &str = "Torpor: this file belongs to Torpor's save points; the AI can't change it.";

/// Own deadline per event (spec 3.2). None = unknown event: exit 0 at once.
pub fn own_deadline_ms(event: &str) -> Option<u64> {
    Some(match event {
        "SessionStart" => 3000,
        "UserPromptSubmit" => 5000,
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => 2000,
        "Stop" => 8000,
        "StopFailure" => 5000,
        "SubagentStop" | "PreCompact" => 1000,
        "SessionEnd" => 1200,
        _ => return None,
    })
}

// ---------------------------------------------------------------- payload

pub struct P {
    pub v: Value,
}

impl P {
    pub fn s(&self, k: &str) -> Option<String> {
        self.v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string())
    }
    pub fn i(&self, k: &str) -> Option<i64> {
        let x = self.v.get(k)?;
        x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))
    }
    fn input_str(&self, k: &str) -> Option<String> {
        self.v.get("tool_input").and_then(|t| t.get(k)).and_then(|x| x.as_str()).map(|s| s.to_string())
    }
}

fn read_payload() -> Result<P> {
    let mut buf = Vec::new();
    let mut stdin = std::io::stdin().lock();
    (&mut stdin).take(MAX_STDIN + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_STDIN {
        let _ = std::io::copy(&mut stdin, &mut std::io::sink());
        return Err(Error::Parse("stdin over 8 MiB".into()));
    }
    let v: Value = serde_json::from_slice(&buf).map_err(|_| Error::Parse("stdin is not JSON".into()))?;
    if !v.is_object() {
        return Err(Error::Parse("stdin is not a JSON object".into()));
    }
    Ok(P { v })
}

// ---------------------------------------------------------------- output shapes (spec 3.3)

fn sys_message(text: &str) -> Value {
    json!({ "systemMessage": text })
}
fn ups_context(text: &str) -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": "UserPromptSubmit", "additionalContext": text } })
}
fn pre_deny(reason: &str) -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason } })
}

// ---------------------------------------------------------------- run

struct Shared {
    out_started: AtomicBool,
    /// (store home, workspace folder if known, workspace id if known)
    ws: Mutex<(PathBuf, Option<PathBuf>, String)>,
}

fn emit(shared: &Shared, v: &Value) {
    if shared.out_started.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        // The watchdog owns the exit; wait for it.
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    let mut s = serde_json::to_string(v).unwrap_or_default();
    s.push('\n');
    let out = std::io::stdout();
    let mut l = out.lock();
    let _ = l.write_all(s.as_bytes());
    let _ = l.flush();
}

/// Entry point for `torpor hook <Event>`. Always returns 0.
pub fn run(event: &str) -> i32 {
    std::panic::set_hook(Box::new(|_| {}));
    let Some(own_ms) = own_deadline_ms(event) else { return 0 };
    let t0 = Instant::now();
    let shared = Arc::new(Shared { out_started: AtomicBool::new(false), ws: Mutex::new((home_dir(), None, String::new())) });
    {
        let sh = shared.clone();
        let ev = event.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(own_ms));
            if sh.out_started.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                if let Ok(g) = sh.ws.lock() {
                    log_fault_line(&g.0, &g.2, &ev, "timeout", &format!("no answer within {own_ms} ms"));
                }
                // SAFETY: _exit ends the process at once; nothing was written to stdout.
                unsafe { libc::_exit(0) }
            }
        });
    }
    let res = catch_unwind(AssertUnwindSafe(|| handle(event, &shared, t0, own_ms)));
    match res {
        Ok(Ok(Some(v))) => emit(&shared, &v),
        Ok(Ok(None)) => {}
        Ok(Err(e)) => note_fault(event, &shared, e.fault_kind(), &e.to_string()),
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            note_fault(event, &shared, "panic", &msg);
        }
    }
    0
}

/// Fault line plus, when the workspace store can be opened quickly, a fault row.
fn note_fault(event: &str, shared: &Shared, kind: &str, detail: &str) {
    let (home, dir, id) = shared.ws.lock().map(|g| g.clone()).unwrap_or_else(|_| (home_dir(), None, String::new()));
    let (dir, id) = match dir {
        Some(d) => (Some(d), id),
        None => match resolve_root(None).and_then(|r| Ws::open(&r, false)) {
            Ok(w) => (Some(w.dir.clone()), w.id.clone()),
            Err(_) => (None, String::new()),
        },
    };
    record_fault(&home, dir.as_deref(), &id, event, kind, detail);
}

#[cfg(feature = "fault-inject")]
fn inject_faults() {
    if std::env::var_os("TORPOR_TEST_PANIC").is_some() {
        panic!("injected panic");
    }
    if let Some(ms) = std::env::var("TORPOR_TEST_SLEEP_MS").ok().and_then(|s| s.parse::<u64>().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}
#[cfg(not(feature = "fault-inject"))]
fn inject_faults() {}

fn handle(event: &str, shared: &Shared, t0: Instant, own_ms: u64) -> Result<Option<Value>> {
    let p = read_payload()?;
    let session = p.s("session_id").unwrap_or_else(|| "unknown".to_string());
    let root = match resolve_root(p.s("cwd").as_deref()) {
        Ok(r) => r,
        Err(Error::Refused(why)) => {
            // Nothing is recorded for these. Only SessionStart says why, once.
            return Ok(if event == "SessionStart" && why != "store" { Some(sys_message(MSG_REFUSED_ROOT)) } else { None });
        }
        Err(e) => return Err(e),
    };
    let mut ws = match Ws::open(&root, true) {
        Ok(w) => w,
        Err(Error::Newer) => return Ok(if event == "SessionStart" { Some(sys_message(&format!("Torpor: {MSG_NEWER}"))) } else { None }),
        Err(e) => return Err(e),
    };
    if let Ok(mut g) = shared.ws.lock() {
        *g = (ws.home.clone(), Some(ws.dir.clone()), ws.id.clone());
    }
    inject_faults();
    ws.lock_shared(LOCK_WAIT_MS)?;
    let now = now_ns();
    let tx = Transaction::new_unchecked(&ws.conn, TransactionBehavior::Immediate)?;
    let (out, spawn_gc) = {
        let cx = Cx { ws: &ws, db: &tx, p: &p, event, now, session, t0, own_ms, spawn_gc: Cell::new(false) };
        let out = dispatch(&cx)?;
        (out, cx.spawn_gc.get())
    };
    tx.commit()?;
    drop(ws);
    if spawn_gc {
        spawn_detached_gc(&root);
    }
    Ok(out)
}

pub struct Cx<'a> {
    pub ws: &'a Ws,
    pub db: &'a Connection,
    pub p: &'a P,
    pub event: &'a str,
    pub now: i64,
    pub session: String,
    pub t0: Instant,
    pub own_ms: u64,
    pub spawn_gc: Cell<bool>,
}

impl Cx<'_> {
    /// Soft time for hashing: about 60% of the event's own deadline, minus what has passed.
    fn budget(&self) -> Duration {
        let total = Duration::from_millis(self.own_ms * 6 / 10);
        total.saturating_sub(self.t0.elapsed()).max(Duration::from_millis(100))
    }
}

fn off_switch(db: &Connection) -> bool {
    std::env::var("TORPOR_OFF").is_ok_and(|v| v == "1") || meta_get(db, "off").as_deref() == Some("1")
}

fn dispatch(cx: &Cx) -> Result<Option<Value>> {
    let db = cx.db;
    let tp = cx.p.s("transcript_path");
    let src = cx.p.s("source");
    let pm = cx.p.s("permission_mode");
    if off_switch(db) {
        turn::heartbeat(db, &cx.session, cx.event, cx.now)?;
        return Ok(None);
    }
    turn::touch_session(
        db,
        &SessionInfo { id: &cx.session, transcript_path: tp.as_deref(), source: src.as_deref(), permission_mode: pm.as_deref() },
        cx.now,
        cx.event == "SessionStart",
    )?;
    turn::heartbeat(db, &cx.session, cx.event, cx.now)?;
    match cx.event {
        "SessionStart" => session_start(cx),
        "UserPromptSubmit" => user_prompt(cx),
        "PreToolUse" => pre_tool(cx),
        "PostToolUse" => post_tool(cx, false),
        "PostToolUseFailure" => post_tool(cx, true),
        "Stop" => stop(cx, false),
        "StopFailure" => stop(cx, true),
        "SubagentStop" => subagent_stop(cx),
        "PreCompact" => pre_compact(cx),
        "SessionEnd" => session_end(cx),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------- capture wrapper

/// Run one capture inside a savepoint. A failure undoes its rows, is recorded as a fault, and the event goes on.
fn guarded<T>(cx: &Cx, f: impl FnOnce() -> Result<T>) -> Option<T> {
    let db = cx.db;
    if db.execute_batch("SAVEPOINT cap").is_err() {
        return None;
    }
    match f() {
        Ok(v) => {
            let _ = db.execute_batch("RELEASE cap");
            Some(v)
        }
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK TO cap; RELEASE cap");
            fault_tx(db, &cx.ws.home, &cx.ws.id, cx.event, e.fault_kind(), &e.to_string());
            None
        }
    }
}

fn guarded_capture(cx: &Cx, req: Req) -> Option<Captured> {
    guarded(cx, || capture::capture(cx.ws, cx.db, &req))
}

// ---------------------------------------------------------------- turns

/// The turn this event belongs to; starts one (full scan, `start` point) when the prompt_id is new.
/// An open turn of the same session with another prompt_id ends there: `interrupted` at a prompt, else `implied`.
fn begin(cx: &Cx, ups: bool) -> Result<Turn> {
    let db = cx.db;
    let sid = cx.session.as_str();
    let mut pid = cx.p.s("prompt_id");
    if pid.is_none() {
        if !ups {
            if let Some(t) = turn::open_of(db, sid)? {
                return Ok(t);
            }
        }
        pid = Some(format!("seq-{}", turn::count_of(db, sid)? + 1));
    }
    let pid = pid.unwrap_or_default();
    if let Some(t) = turn::find(db, sid, &pid)? {
        if t.end_kind != "open" {
            turn::reopen(db, t.id)?;
        }
        return Ok(Turn { end_kind: "open".into(), ..t });
    }
    let old = turn::open_of(db, sid)?;
    let prompt = if ups { cx.p.s("prompt") } else { None };
    let notification = prompt.as_deref().is_some_and(|p| p.trim_start().starts_with("<task-notification>"));
    let (origin, preview) = if notification { ("task_notification", None) } else { ("unknown", prompt.as_deref().and_then(mask::prompt_preview)) };
    let new = turn::create(db, sid, &pid, cx.now, preview.as_deref(), origin)?;
    let mut points = Vec::new();
    if let Some(o) = &old {
        points.push(PointSpec { kind: PointKind::End, turn_id: Some(o.id), note: None });
    }
    points.push(PointSpec { kind: PointKind::Start, turn_id: Some(new.id), note: None });
    let cap = guarded_capture(cx, Req { session: sid, points, turn: old.as_ref().map(|o| o.id), only_if_changed: false, budget: Some(cx.budget()) });
    let (end_pt, start_pt) = match (&cap, &old) {
        (Some(c), Some(_)) => (c.point_ids.first().copied(), c.point_ids.get(1).copied()),
        (Some(c), None) => (None, c.point_ids.first().copied()),
        _ => (None, None),
    };
    if let Some(o) = &old {
        turn::close(db, o.id, if ups { "interrupted" } else { "implied" }, end_pt, cx.now, None)?;
    }
    turn::set_start_point(db, new.id, start_pt)?;
    Ok(Turn { start_point: start_pt, ..new })
}

// ---------------------------------------------------------------- SessionStart

fn auto_init(cx: &Cx) -> Result<()> {
    let db = cx.db;
    let prev = meta_get_i64(db, "file_count").unwrap_or(0).max(0) as usize;
    // Unknown size: time it the way a large project would be scanned (parallel).
    let threads = scan::thread_count(if prev == 0 { usize::MAX } else { prev });
    let mut sc = scan::scan(&cx.ws.root, threads, scan::ENTRY_CAP)?;
    if sc.elapsed_ms > 40 {
        // The first walk warms the file system cache; the second is what later hooks will see.
        let again = scan::scan(&cx.ws.root, threads, scan::ENTRY_CAP)?;
        if again.elapsed_ms < sc.elapsed_ms {
            sc.elapsed_ms = again.elapsed_ms;
        }
    }
    capture::update_mode(db, sc.elapsed_ms, sc.file_count())?;
    let dc = scan::deep_check(&cx.ws.root, &sc.entries, Duration::from_secs(1));
    meta_set(db, "deep_check_json", &serde_json::to_string(&dc)?)?;
    Ok(())
}

fn session_start(cx: &Cx) -> Result<Option<Value>> {
    let db = cx.db;
    if meta_get(db, "mode").is_none() {
        guarded(cx, || auto_init(cx));
    }
    guarded(cx, || retention::thin_if_due(db, cx.now));
    let mut problems: Vec<String> = Vec::new();
    if let Err(Error::Disk(d)) = check_disk(&cx.ws.home) {
        problems.push(format!("save points are paused: the disk is almost full ({d})."));
    }
    if let Some(n) = lost_key_files(db) {
        problems.push(format!("{n} secret file{} saved earlier can't be restored: their key file is gone.", if n == 1 { "" } else { "s" }));
    }
    Ok(problems.first().map(|p| sys_message(&format!("Torpor: {p}"))))
}

/// Secret blobs whose key is not the current key file's: (count) or None when there are none.
pub fn lost_key_files(db: &Connection) -> Option<i64> {
    let cur = Key::load().ok().flatten().map(|k| k.id);
    let n: i64 = match &cur {
        Some(id) => db.query_row("SELECT COUNT(*) FROM blobs WHERE kind='secret' AND COALESCE(key_id,'')<>?1", [id], |r| r.get(0)).ok()?,
        None => db.query_row("SELECT COUNT(*) FROM blobs WHERE kind='secret'", [], |r| r.get(0)).ok()?,
    };
    if n > 0 { Some(n) } else { None }
}

// ---------------------------------------------------------------- UserPromptSubmit

fn restore_notice(cx: &Cx) -> Result<Option<String>> {
    let db = cx.db;
    let applying: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM restores WHERE status='applying')", [], |r| r.get(0))?;
    if applying {
        return Ok(Some(MSG_RESTORE_INTERRUPTED.to_string()));
    }
    let seen: i64 = db.query_row("SELECT notified_restore_id FROM sessions WHERE id=?1", [&cx.session], |r| r.get(0)).optional()?.unwrap_or(0);
    let row: Option<(i64, Option<i64>, Option<i64>, Option<String>)> = db
        .prepare_cached("SELECT id,target_point,finished_at,report_json FROM restores WHERE id>?1 AND status IN ('done','partial') ORDER BY id DESC LIMIT 1")?
        .query_row([seen], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .optional()?;
    let Some((rid, target, at, report)) = row else { return Ok(None) };
    db.execute("UPDATE sessions SET notified_restore_id=?2 WHERE id=?1", params![cx.session, rid])?;
    let what = match target {
        Some(pt) => db
            .query_row("SELECT p.kind,t.n FROM points p LEFT JOIN turns t ON t.id=p.turn_id WHERE p.id=?1", [pt], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)))
            .optional()?
            .map(|(kind, n)| {
                let tail = match (kind.as_str(), n) {
                    ("end", Some(n)) => format!(" (end of turn {n})"),
                    ("start", Some(n)) => format!(" (start of turn {n})"),
                    ("pre_bash", Some(n)) => format!(" (before a command in turn {n})"),
                    _ => String::new(),
                };
                format!("save point {pt}{tail}")
            })
            .unwrap_or_else(|| format!("save point {pt}")),
        None => "an earlier save point".to_string(),
    };
    let files: Vec<String> = report
        .and_then(|j| serde_json::from_str::<Value>(&j).ok())
        .and_then(|v| v.get("files").and_then(|f| f.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()))
        .unwrap_or_default();
    let when = at.map(|ns| format!(" at {}", local_hhmm(ns))).unwrap_or_default();
    let list = match files.len() {
        0 => String::new(),
        1 | 2 | 3 => format!(": {}", files.join(", ")),
        n => format!(": {}, {} and {} more", files[0], files[1], n - 2),
    };
    Ok(Some(format!("Torpor: files were restored to {what}{when}{list}. Re-read files before editing them.")))
}

fn user_prompt(cx: &Cx) -> Result<Option<Value>> {
    let notice = restore_notice(cx)?;
    begin(cx, true)?;
    Ok(notice.map(|t| ups_context(&t)))
}

// ---------------------------------------------------------------- tools

fn lexical(base: &Path, s: &str) -> PathBuf {
    let p = Path::new(s);
    let joined = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Where a write to `p` would land: the real path of the longest existing part (symlinks and `..` resolved the
/// way the kernel does), then the rest as written. A dangling symlink is followed: writing through it creates its target.
fn real(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    let mut hops = 0;
    loop {
        if let Ok(c) = std::fs::canonicalize(&cur) {
            let mut out = c;
            for t in tail.iter().rev() {
                if t == ".." {
                    out.pop();
                } else {
                    out.push(t);
                }
            }
            return out;
        }
        if hops < 16 && std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
            if let (Ok(t), Some(par)) = (std::fs::read_link(&cur), cur.parent()) {
                cur = par.join(t);
                hops += 1;
                continue;
            }
        }
        match cur.components().next_back() {
            Some(Component::Normal(n)) => tail.push(n.to_owned()),
            Some(Component::ParentDir) => tail.push("..".into()),
            _ => return p.to_path_buf(),
        }
        cur.pop();
    }
}

fn is_protected(abs: &Path) -> bool {
    let r = real(abs);
    r.starts_with(real(&home_dir())) || r == real(&key_path())
}

/// Paths named by an edit-family tool_input (file_path, notebook_path, path), joined to the working folder as written.
fn edit_raw(cx: &Cx) -> Vec<PathBuf> {
    let base = cx.p.s("cwd").map(PathBuf::from).unwrap_or_else(|| cx.ws.root.clone());
    ["file_path", "notebook_path", "path"].iter().filter_map(|k| cx.p.input_str(k)).filter(|s| !s.is_empty()).map(|s| base.join(s)).collect()
}

/// The same paths with `.` and `..` folded away without looking at the disk.
fn edit_paths(cx: &Cx) -> Vec<PathBuf> {
    edit_raw(cx).iter().map(|p| lexical(Path::new("/"), &p.to_string_lossy())).collect()
}

fn rel_of(root: &Path, abs: &Path) -> Option<String> {
    let r = match abs.strip_prefix(root) {
        Ok(r) => r.to_path_buf(),
        Err(_) => real(abs).strip_prefix(root).ok()?.to_path_buf(),
    };
    let s = r.to_str()?;
    if s.is_empty() { None } else { Some(s.to_string()) }
}

fn pre_tool(cx: &Cx) -> Result<Option<Value>> {
    let tool = cx.p.s("tool_name").unwrap_or_default();
    // Both readings of the path: with `link/../x` the kernel and a plain text fold land in different places.
    if EDIT_TOOLS.contains(&tool.as_str()) && edit_raw(cx).iter().chain(edit_paths(cx).iter()).any(|p| is_protected(p)) {
        return Ok(Some(pre_deny(MSG_PROTECTED)));
    }
    begin(cx, false)?;
    // S1b: Bash destructive rules (pre_bash point, ask/deny) go here.
    Ok(None)
}

fn exit_code_of(error: &str) -> Option<i64> {
    let rest = error.strip_prefix("Exit code ")?;
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn post_tool(cx: &Cx, failed: bool) -> Result<Option<Value>> {
    let db = cx.db;
    let tool = cx.p.s("tool_name").unwrap_or_default();
    let turn = begin(cx, false)?;
    let agent = cx.p.s("agent_id");
    let tuid = cx.p.s("tool_use_id");
    if EDIT_TOOLS.contains(&tool.as_str()) {
        let rels: Vec<String> = edit_paths(cx).iter().filter_map(|a| rel_of(&cx.ws.root, a)).collect();
        let pj = serde_json::to_string(&rels)?;
        turn::add_event(
            db,
            &EventRow { session: &cx.session, turn: Some(turn.id), tool_use_id: tuid.as_deref(), tool: &tool, agent: agent.as_deref(), paths_json: Some(&pj), cmd_preview: None, exit_code: None, failed, duration_ms: cx.p.i("duration_ms") },
            cx.now,
        )?;
        guarded(cx, || capture::capture_named(cx.ws, db, &rels));
    } else if tool == "Bash" {
        let cmd = cx.p.input_str("command").map(|c| mask::cmd_preview(&c));
        let exit = if failed { cx.p.v.get("error").and_then(|e| e.as_str()).and_then(exit_code_of) } else { Some(0) };
        turn::add_event(
            db,
            &EventRow { session: &cx.session, turn: Some(turn.id), tool_use_id: tuid.as_deref(), tool: "Bash", agent: agent.as_deref(), paths_json: None, cmd_preview: cmd.as_deref(), exit_code: exit, failed, duration_ms: cx.p.i("duration_ms") },
            cx.now,
        )?;
        turn::bump_bash(db, turn.id)?;
        if capture::mode(db) == Mode::Fast {
            guarded_capture(
                cx,
                Req { session: &cx.session, points: vec![PointSpec { kind: PointKind::Tool, turn_id: Some(turn.id), note: None }], turn: Some(turn.id), only_if_changed: true, budget: Some(cx.budget()) },
            );
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------- Stop

fn stop(cx: &Cx, failed: bool) -> Result<Option<Value>> {
    let db = cx.db;
    let sid = cx.session.as_str();
    let turn = begin(cx, false)?;
    let cap = guarded_capture(
        cx,
        Req { session: sid, points: vec![PointSpec { kind: PointKind::End, turn_id: Some(turn.id), note: None }], turn: Some(turn.id), only_if_changed: false, budget: Some(cx.budget()) },
    );
    let end_pt = cap.as_ref().and_then(|c| c.point_ids.first().copied());
    let bg = cx.p.v.get("background_tasks").filter(|v| v.as_array().is_some_and(|a| !a.is_empty())).map(|v| v.to_string());
    turn::close(db, turn.id, if failed { "failed" } else { "stop" }, end_pt, cx.now, bg.as_deref())?;
    guarded(cx, || read_origin(cx, &turn));
    if failed || end_pt.is_none() || meta_get(db, "quiet").as_deref() == Some("1") {
        return Ok(None);
    }
    let line = stop_line(db, &turn, end_pt.unwrap_or(0))?;
    Ok(line.map(|l| sys_message(&l)))
}

fn read_origin(cx: &Cx, turn: &Turn) -> Result<()> {
    let db = cx.db;
    let row: Option<(Option<String>, i64)> = db.query_row("SELECT transcript_path,transcript_offset FROM sessions WHERE id=?1", [&cx.session], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let Some((tp, off)) = row else { return Ok(()) };
    let Some(tp) = tp.or_else(|| cx.p.s("transcript_path")) else { return Ok(()) };
    let pid = turn.prompt_id.as_deref().filter(|p| !p.starts_with("seq-"));
    let r = transcript::read_origin(Path::new(&tp), off, pid);
    if let Some(f) = r.found {
        db.execute("UPDATE turns SET origin=?2, origin_by=?3 WHERE id=?1 AND origin IN ('unknown','none')", params![turn.id, f.origin, f.by])?;
    }
    db.execute("UPDATE sessions SET transcript_offset=?2 WHERE id=?1", params![cx.session, r.new_offset])?;
    Ok(())
}

/// The one-line summary printed at Stop (spec 3.3). None when the turn changed nothing.
pub fn stop_line(db: &Connection, turn: &Turn, end_point: i64) -> Result<Option<String>> {
    let (edits, shell, sub) = crate::turn::net_counts(db, turn.id)?;
    let total = edits + shell + sub;
    if total == 0 {
        return Ok(None);
    }
    let mut parts: Vec<String> = Vec::new();
    if edits > 0 {
        parts.push(format!("{edits} edit{}", if edits == 1 { "" } else { "s" }));
    }
    if shell > 0 {
        parts.push(format!("{shell} shell"));
    }
    if sub > 0 {
        parts.push(format!("{sub} subagent"));
    }
    let mut line = format!(
        "Torpor: save point {end_point} · turn {} changed {total} file{} ({}) · undo it: torpor undo {}",
        turn.n,
        if total == 1 { "" } else { "s" },
        parts.join(", "),
        turn.n
    );
    let before: Option<String> = db
        .query_row("SELECT note FROM points WHERE turn_id=?1 AND kind='pre_bash' AND note IS NOT NULL ORDER BY id LIMIT 1", [turn.id], |r| r.get(0))
        .optional()?;
    if let Some(n) = before {
        let cand = format!("{line} · saved before {n}");
        if cand.chars().count() <= STOP_LINE_MAX {
            line = cand;
        }
    }
    let start = turn.start_point.unwrap_or(0);
    let unsaved: i64 = db.query_row(
        "SELECT COUNT(*) FROM not_captured n WHERE n.point_id=?1 AND NOT EXISTS (SELECT 1 FROM not_captured m WHERE m.point_id=?2 AND m.path=n.path AND m.reason=n.reason)",
        params![end_point, start],
        |r| r.get(0),
    )?;
    if unsaved > 0 {
        let cand = format!("{line} · {unsaved} file{} not saved (torpor status)", if unsaved == 1 { "" } else { "s" });
        if cand.chars().count() <= STOP_LINE_MAX {
            line = cand;
        }
    }
    Ok(Some(trunc_chars(&line, STOP_LINE_MAX)))
}

// ---------------------------------------------------------------- the small events

fn subagent_stop(cx: &Cx) -> Result<Option<Value>> {
    let tp = cx.p.s("agent_transcript_path").map(|p| serde_json::to_string(&[p]).unwrap_or_default());
    let ty = cx.p.s("agent_type");
    let turn = cx.p.s("prompt_id").and_then(|p| turn::find(cx.db, &cx.session, &p).ok().flatten()).map(|t| t.id);
    turn::add_event(
        cx.db,
        &EventRow { session: &cx.session, turn, tool_use_id: None, tool: "SubagentStop", agent: cx.p.s("agent_id").as_deref(), paths_json: tp.as_deref(), cmd_preview: ty.as_deref(), exit_code: None, failed: false, duration_ms: None },
        cx.now,
    )?;
    Ok(None)
}

fn pre_compact(cx: &Cx) -> Result<Option<Value>> {
    cx.db.execute("UPDATE sessions SET compact_epoch=compact_epoch+1 WHERE id=?1", [&cx.session])?;
    Ok(None) // never any stdout: it would join the compaction prompt
}

fn session_end(cx: &Cx) -> Result<Option<Value>> {
    let db = cx.db;
    let sid = cx.session.as_str();
    if let Some(t) = turn::open_of(db, sid)? {
        let cap = guarded_capture(
            cx,
            Req { session: sid, points: vec![PointSpec { kind: PointKind::End, turn_id: Some(t.id), note: None }], turn: Some(t.id), only_if_changed: false, budget: Some(cx.budget()) },
        );
        turn::close(db, t.id, "session_end", cap.as_ref().and_then(|c| c.point_ids.first().copied()), cx.now, None)?;
    }
    let reason = cx.p.s("reason");
    db.execute("UPDATE sessions SET ended_at=?2, end_reason=?3 WHERE id=?1", params![sid, cx.now, reason])?;
    let last_gc = meta_get_i64(db, "last_gc").unwrap_or(0);
    if cx.now - last_gc > 24 * 3600 * NS_PER_SEC {
        cx.spawn_gc.set(true);
    }
    Ok(None)
}

/// `torpor gc --if-quiet` in its own session with all three stdio closed, so the hook can exit at once.
fn spawn_detached_gc(root: &Path) {
    #[cfg(feature = "fault-inject")]
    if std::env::var_os("TORPOR_TEST_NO_GC").is_some() {
        return;
    }
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else { return };
    let mut c = Command::new(exe);
    c.args(["gc", "--if-quiet"]).current_dir(root).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and only detaches the child from our session.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = c.spawn();
}
