//! Hook mode (spec section 3): `torpor hook <Event>`. Fail-open: never stderr, never exit 2, never `allow`.
use crate::capture::{self, Captured, Mode, PointKind, PointSpec, Req};
use crate::store::*;
use crate::turn::{self, EventRow, SessionInfo, Turn};
use crate::util::*;
use crate::destructive::{self, Verdict};
use crate::{constraints, errkey, gate, mask, promise, push, rebuild, restore, retention, rewind, scan, transcript, vault};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
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
/// The most characters of additionalContext one UserPromptSubmit answer carries (Claude Code cuts at 10,000).
const UPS_MAX: usize = 9_900;

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
        "Stop" => 30000, // the promise gate (spec 18) may use up to 20 s of it
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
fn post_context(text: &str) -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": "PostToolUse", "additionalContext": text } })
}
fn pre_deny(reason: &str) -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason } })
}
fn pre_ask(reason: &str) -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "ask", "permissionDecisionReason": reason } })
}

// ---------------------------------------------------------------- run

struct Shared {
    out_started: AtomicBool,
    /// (store home, workspace folder if known, workspace id if known)
    ws: Mutex<(PathBuf, Option<PathBuf>, String)>,
    /// Printed when everything else fails (error, panic, no time left): the question or deny for a command that reaches outside.
    fallback: Mutex<Option<Value>>,
}

fn write_out(v: &Value) {
    let mut s = serde_json::to_string(v).unwrap_or_default();
    s.push('\n');
    let out = std::io::stdout();
    let mut l = out.lock();
    let _ = l.write_all(s.as_bytes());
    let _ = l.flush();
}

fn emit(shared: &Shared, v: &Value) {
    if shared.out_started.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        // The watchdog owns the exit; wait for it.
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    write_out(v);
}

fn emit_fallback(shared: &Shared) {
    let v = shared.fallback.lock().ok().and_then(|g| g.clone());
    if let Some(v) = v {
        emit(shared, &v);
    }
}

/// Entry point for `torpor hook <Event>`. Always returns 0.
pub fn run(event: &str) -> i32 {
    std::panic::set_hook(Box::new(|_| {}));
    let Some(own_ms) = own_deadline_ms(event) else { return 0 };
    let t0 = Instant::now();
    let shared = Arc::new(Shared { out_started: AtomicBool::new(false), ws: Mutex::new((home_dir(), None, String::new())), fallback: Mutex::new(None) });
    {
        let sh = shared.clone();
        let ev = event.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(own_ms));
            if sh.out_started.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                if let Ok(g) = sh.ws.lock() {
                    log_fault_line(&g.0, &g.2, &ev, "timeout", &format!("no answer within {own_ms} ms"));
                }
                if let Some(v) = sh.fallback.try_lock().ok().and_then(|g| g.clone()) {
                    write_out(&v);
                }
                // A check app that the gate started must not outlive the hook.
                crate::scratch::kill_live_groups();
                // SAFETY: _exit ends the process at once; nothing was written to stdout.
                unsafe { libc::_exit(0) }
            }
        });
    }
    let res = catch_unwind(AssertUnwindSafe(|| handle(event, &shared, t0, own_ms)));
    match res {
        Ok(Ok(Some(v))) => emit(&shared, &v),
        Ok(Ok(None)) => {}
        Ok(Err(e)) => {
            note_fault(event, &shared, e.fault_kind(), &e.to_string());
            emit_fallback(&shared);
        }
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            note_fault(event, &shared, "panic", &msg);
            emit_fallback(&shared);
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
    let verdict = bash_verdict(&p, event);
    // Torpor's own files and the user-only commands are protected whatever fails below.
    if let Some(deny) = verdict_deny(&verdict) {
        return Ok(Some(deny));
    }
    // A command that reaches outside is asked about (or denied) even if saving fails.
    if let (Verdict::Remote(rule), Ok(mut g)) = (&verdict, shared.fallback.lock()) {
        *g = Some(remote_answer(rule, p.s("permission_mode").as_deref()));
    }
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
    // A crashed restore is finished first (spec 6.3 step 7). On Busy the normal path goes on; the hook deadline may cut this short, the journal makes that safe.
    if matches!(event, "SessionStart" | "UserPromptSubmit") && restore::has_pending(&ws) && ws.lock_exclusive(LOCK_WAIT_MS).is_ok() {
        let _ = restore::recover_locked(&ws);
        ws.unlock();
    }
    ws.lock_shared(LOCK_WAIT_MS)?;
    let now = now_ns();
    let tx = Transaction::new_unchecked(&ws.conn, TransactionBehavior::Immediate)?;
    let (out, spawn_gc, stop_info) = {
        let cx = Cx { ws: &ws, db: &tx, p: &p, event, now, session, t0, own_ms, spawn_gc: Cell::new(false), stop: RefCell::new(None), verdict };
        let out = dispatch(&cx)?;
        (out, cx.spawn_gc.get(), cx.stop.take())
    };
    tx.commit()?;
    drop(ws);
    if spawn_gc {
        spawn_detached_gc(&root);
    }
    match stop_info {
        Some(info) => Ok(gate_phase(event, shared, &root, t0, own_ms, now, info, out)),
        None => Ok(out),
    }
}

/// What phase 2 of Stop (the promise gate, spec 18) needs from phase 1.
pub struct StopInfo {
    session: String,
    turn: Turn,
    end_pt: i64,
    tree: [u8; 32],
    /// Phase 1's message lines, and the ones among them that are told only once (a block would lose them).
    lines: Vec<String>,
    notices: Vec<String>,
}

/// Phase 2: the gate runs after phase 1 is saved. If it fails, panics or runs out of time, the answer is phase 1's: the Stop is allowed.
#[allow(clippy::too_many_arguments)]
fn gate_phase(event: &str, shared: &Shared, root: &Path, t0: Instant, own_ms: u64, now: i64, info: StopInfo, phase1: Option<Value>) -> Option<Value> {
    if let Ok(mut g) = shared.fallback.lock() {
        *g = phase1.clone();
    }
    // 20 s at most, and always 7 s before the hook's own deadline for writing the answer.
    let until = (Instant::now() + Duration::from_secs(20)).min(t0 + Duration::from_millis(own_ms.saturating_sub(7000)));
    let inp = gate::Input {
        session: info.session.clone(),
        turn_id: info.turn.id,
        turn_n: info.turn.n,
        prompt_id: info.turn.prompt_id.clone().unwrap_or_default(),
        end_pt: info.end_pt,
        tree: info.tree,
        now,
        until,
        notices: info.notices.clone(),
    };
    match catch_unwind(AssertUnwindSafe(|| gate::run(root, &inp))) {
        Ok(Ok(gate::Out::Block(reason))) => Some(json!({ "decision": "block", "reason": reason })),
        Ok(Ok(gate::Out::Lines(more))) => {
            let all: Vec<String> = info.lines.iter().cloned().chain(more).collect();
            Some(sys_message(&all.join("\n")))
        }
        Ok(Ok(gate::Out::Nothing)) => phase1,
        Ok(Err(e)) => {
            note_fault(event, shared, e.fault_kind(), &e.to_string());
            phase1
        }
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            note_fault(event, shared, "panic", &msg);
            phase1
        }
    }
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
    /// Set by Stop when the promise gate should run after the transaction.
    pub stop: RefCell<Option<StopInfo>>,
    /// What spec section 7 says about this call (Clear unless it is a Bash PreToolUse).
    pub verdict: Verdict,
}

impl Cx<'_> {
    /// Soft time for hashing: about 60% of the event's own deadline, minus what has passed.
    fn budget(&self) -> Duration {
        // Stop keeps an 8 s basis for hashing: the promise gate after it has its own clock (spec 18).
        let basis = if self.event == "Stop" { self.own_ms.min(8000) } else { self.own_ms };
        let total = Duration::from_millis(basis * 6 / 10);
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
        // Paused: no save points and no questions, but Torpor's own files stay protected.
        return Ok(if cx.event == "PreToolUse" { protected_edit(cx) } else { None });
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
            // No prompt_id on an event that is not a prompt: it belongs to the turn that is open, else to the newest one (a Stop ended it and the
            // AI went on). A new `seq-N` turn here would start a new block chain every time and the 2-block cap would never trip.
            if let Some(t) = turn::open_of(db, sid)? {
                return Ok(t);
            }
            if let Some(t) = turn::newest_of(db, sid)? {
                turn::reopen(db, t.id)?;
                return Ok(Turn { end_kind: "open".into(), ..t });
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
    if let (true, Some(p)) = (ups && !notification, prompt.as_deref()) {
        // A failed text copy must never stop the turn from starting.
        let _ = crate::texts::store_prompt(db, new.id, p);
    }
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

/// First scan of a folder (spec 5.10): time it, choose the checking mode, look for ignored folders that hold real files.
/// The scan is returned for `torpor init`, which describes what it found.
pub fn first_scan(ws: &Ws, db: &Connection) -> Result<scan::Scan> {
    let prev = meta_get_i64(db, "file_count").unwrap_or(0).max(0) as usize;
    // Unknown size: time it the way a large project would be scanned (parallel).
    let threads = scan::thread_count(if prev == 0 { usize::MAX } else { prev });
    let mut sc = scan::scan(&ws.root, threads, scan::ENTRY_CAP)?;
    if sc.elapsed_ms > 40 {
        // The first walk warms the file system cache; the second is what later hooks will see.
        let again = scan::scan(&ws.root, threads, scan::ENTRY_CAP)?;
        if again.elapsed_ms < sc.elapsed_ms {
            sc.elapsed_ms = again.elapsed_ms;
        }
    }
    capture::update_mode(db, sc.elapsed_ms, sc.file_count())?;
    let dc = scan::deep_check(&ws.root, &sc.entries, Duration::from_secs(1));
    meta_set(db, "deep_check_json", &serde_json::to_string(&dc)?)?;
    Ok(sc)
}

fn session_start(cx: &Cx) -> Result<Option<Value>> {
    let mut out = start_notice(cx)?;
    // Spec 21: after a compaction (or a clear, or a resume) Torpor's records go back to the AI. A subagent's compaction fires this event with
    // the parent's ids and an agent_id, so only a payload without one is the conversation's own.
    if cx.p.s("agent_id").is_none() {
        let src = cx.p.s("source").unwrap_or_default();
        let tp = cx.p.s("transcript_path");
        if src == "compact" {
            // A turn still open has not had its origin read (Stop does that): read it now, so the track can say who asked.
            guarded(cx, || match turn::open_of(cx.db, &cx.session)? {
                Some(t) => read_origin_for(cx.db, &cx.session, tp.clone(), &t, false),
                None => Ok(()),
            });
        }
        if let Some(text) = guarded(cx, || rebuild::at_start(cx.db, &cx.session, &src, tp.as_deref().map(Path::new))).flatten() {
            let mut v = out.take().unwrap_or_else(|| json!({}));
            v["hookSpecificOutput"] = json!({ "hookEventName": "SessionStart", "additionalContext": text });
            out = Some(v);
        }
    }
    Ok(out)
}

/// The line for the person at SessionStart: a first scan, a full disk, a lost key, a changed vault, a gate that has not run.
fn start_notice(cx: &Cx) -> Result<Option<Value>> {
    let db = cx.db;
    if meta_get(db, "mode").is_none() {
        guarded(cx, || first_scan(cx.ws, cx.db).map(|_| ()));
    }
    guarded(cx, || retention::thin_if_due(db, cx.now));
    let mut problems: Vec<String> = Vec::new();
    if let Err(Error::Disk(d)) = check_disk(&cx.ws.home) {
        problems.push(format!("save points are paused: the disk is almost full ({d})."));
    }
    if let Some(n) = lost_key_files(db) {
        problems.push(format!("{n} secret file{} saved earlier can't be restored: their key file is gone.", if n == 1 { "" } else { "s" }));
    }
    // A changed vault is the more serious notice (spec 13); it never blocks.
    if let Some(n) = vault_notice(cx, false) {
        return Ok(Some(sys_message(&n)));
    }
    if let Some(p) = problems.first() {
        return Ok(Some(sys_message(&format!("Torpor: {p}"))));
    }
    Ok(gate::silent(db).then(|| sys_message(gate::SILENT_NOTICE)))
}

/// Spec 13: the promise vault against its chain. `once`: a mismatch is told one time (Stop); SessionStart tells it every time.
fn vault_notice(cx: &Cx, once: bool) -> Option<String> {
    let found = guarded(cx, || Ok(vault::verify(cx.ws, Key::load()?.as_ref()).notice()))?;
    let db = cx.db;
    let Some(n) = found else {
        guarded(cx, || Ok(db.execute("DELETE FROM meta WHERE key='vault_notice'", [])?));
        return None;
    };
    if once {
        if meta_get(db, "vault_notice").as_deref() == Some(n.as_str()) {
            return None;
        }
        guarded(cx, || meta_set(db, "vault_notice", &n));
    }
    Some(n)
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
    let mut notes: Vec<String> = restore_notice(cx)?.into_iter().collect();
    // Spec 21: a compaction the transcript had not shown at SessionStart is looked for once more here. This comes before `begin`, so the
    // new prompt's turn is not in the text. It goes first: the notes after it are the ones dropped if the whole is too long.
    let tp = cx.p.s("transcript_path");
    if let Some(text) = guarded(cx, || rebuild::at_prompt(cx.db, &cx.session, tp.as_deref().map(Path::new))).flatten() {
        notes.insert(0, text);
    }
    let turn = begin(cx, true)?;
    // A rewind that left shell or subagent changes standing (spec 22): the person sees the line too.
    let rewind = guarded(cx, || rewind::line(cx.ws, cx.db, &cx.session, &turn)).flatten();
    notes.extend(rewind.clone());
    notes.extend(gate_notes(cx, &turn));
    // One string must stay under 10,000 characters (the rebuild alone is at most 8,000).
    while notes.len() > 1 && notes.iter().map(|n| n.chars().count() + 1).sum::<usize>() > UPS_MAX {
        notes.pop();
    }
    let mut out = (!notes.is_empty()).then(|| ups_context(&notes.join("\n")));
    if let (Some(v), Some(l)) = (out.as_mut(), rewind) {
        v["systemMessage"] = json!(l);
    }
    Ok(out)
}

/// The model-visible lines about promises (spec 18, 23): the failure the gate found, and a warning when the gate has not run for 3 turns.
/// Each goes through the push budget; the same line is not repeated within 10 turns.
fn gate_notes(cx: &Cx, turn: &Turn) -> Vec<String> {
    let db = cx.db;
    let mut out: Vec<String> = Vec::new();
    let mut push_one = |kind: &str, text: &str| {
        let p = push::Push { session: &cx.session, turn_id: turn.id, turn_n: turn.n, kind, text, window: push::REPEAT_TURNS, must: false };
        if text.starts_with("Torpor") && guarded(cx, || push::admit(db, &p)) == Some(true) {
            out.push(text.to_string());
        }
    };
    if let Some(j) = meta_get(db, "ups_line") {
        guarded(cx, || Ok(db.execute("DELETE FROM meta WHERE key='ups_line'", [])?));
        if let Ok(v) = serde_json::from_str::<Value>(&j) {
            if let (Some(k), Some(t)) = (v["kind"].as_str(), v["text"].as_str()) {
                push_one(k, t);
            }
        }
    }
    if gate::silent(db) {
        push_one("silent", "Torpor: the promise checks did not run at the last 3 turn ends. Run torpor check --all.");
    }
    out
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

/// What spec section 7 says about this Bash command (Clear for any other call).
fn bash_verdict(p: &P, event: &str) -> Verdict {
    if event != "PreToolUse" || p.s("tool_name").as_deref() != Some("Bash") {
        return Verdict::Clear;
    }
    match p.input_str("command") {
        Some(cmd) => destructive::judge(&cmd, &destructive::script_dirs(p.s("cwd").as_deref())),
        None => Verdict::Clear,
    }
}

/// The deny-only protection for Bash: Torpor's files and the user-only commands.
fn verdict_deny(v: &Verdict) -> Option<Value> {
    match v {
        Verdict::Store => Some(pre_deny(MSG_PROTECTED)),
        Verdict::UserOnly(c) => Some(pre_deny(&format!("Torpor: only the user can do this, in their own terminal ({c})."))),
        _ => None,
    }
}

/// The deny-only protection for the edit tools: Torpor's files.
fn protected_edit(cx: &Cx) -> Option<Value> {
    let tool = cx.p.s("tool_name").unwrap_or_default();
    // Both readings of the path: with `link/../x` the kernel and a plain text fold land in different places.
    let hit = EDIT_TOOLS.contains(&tool.as_str()) && edit_raw(cx).iter().chain(edit_paths(cx).iter()).any(|p| is_protected(p));
    hit.then(|| pre_deny(MSG_PROTECTED))
}

/// Sessions where the user is asked before commands run. Anything else (or no mode at all) cannot ask.
fn can_ask(mode: Option<&str>) -> bool {
    matches!(mode, Some("default" | "acceptEdits" | "plan"))
}

/// A command that reaches outside gets a question where somebody can answer it, else a deny. Never an allow.
fn remote_answer(rule: &str, mode: Option<&str>) -> Value {
    if can_ask(mode) {
        pre_ask(&format!("Torpor: this command can delete data outside this computer ({rule}). Save points cannot bring that back. Approve only if you meant it."))
    } else {
        pre_deny(&format!(
            "Torpor stopped this because this session skips permission prompts and the command can delete data outside this computer ({rule}). Save points cannot undo it. If the user wants it, they can run it in their own terminal."
        ))
    }
}

/// A save point before the command, noted with the command.
fn pre_bash_point(cx: &Cx, turn: &Turn) {
    let note = cx.p.input_str("command").map(|c| mask::cmd_preview(&c));
    guarded_capture(
        cx,
        Req { session: &cx.session, points: vec![PointSpec { kind: PointKind::PreBash, turn_id: Some(turn.id), note }], turn: Some(turn.id), only_if_changed: false, budget: Some(cx.budget()) },
    );
}

fn pre_tool(cx: &Cx) -> Result<Option<Value>> {
    if let Some(deny) = protected_edit(cx) {
        return Ok(Some(deny));
    }
    if let Verdict::Remote(rule) = &cx.verdict {
        let mode = cx.p.s("permission_mode");
        // Where nobody can be asked the command is denied, so nothing changes and nothing is saved.
        if can_ask(mode.as_deref()) {
            // The question is the point: a save point that fails must not lose it.
            if let Some(turn) = guarded(cx, || begin(cx, false)) {
                pre_bash_point(cx, &turn);
            }
        }
        return Ok(Some(remote_answer(rule, mode.as_deref())));
    }
    let turn = begin(cx, false)?;
    if cx.verdict == Verdict::Save {
        pre_bash_point(cx, &turn);
    }
    if cx.p.s("tool_name").as_deref() == Some("Bash") {
        if let Some(cmd) = cx.p.input_str("command") {
            guarded(cx, || note_cli_calls(cx.db, &cx.session, &turn, &cmd, cx.now));
        }
    }
    Ok(None)
}

/// Spec 12, "Who ran a CLI command": the `torpor` commands the AI is about to run, so the CLI can tell the AI from the user (10 s window).
fn note_cli_calls(db: &Connection, session: &str, turn: &Turn, cmd: &str, now: i64) -> Result<()> {
    for sub in destructive::torpor_calls(cmd) {
        db.execute("INSERT INTO cli_calls(session_id,prompt_id,subcommand,at) VALUES(?1,?2,?3,?4)", params![session, turn.prompt_id, sub, now])?;
    }
    db.execute("DELETE FROM cli_calls WHERE at<?1", [now - 3600 * NS_PER_SEC])?;
    Ok(())
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
        if !failed {
            // Spec 18: tell the AI which promises this edit touches (the push log keeps it to once per promise per turn).
            if let Some(line) = gate::touched(db, &cx.session, turn.id, turn.n, &rels) {
                return Ok(Some(post_context(&line)));
            }
        }
    } else if tool == "Bash" {
        let cmd = cx.p.input_str("command").map(|c| mask::cmd_preview(&c));
        let exit = if failed { cx.p.v.get("error").and_then(|e| e.as_str()).and_then(exit_code_of) } else { Some(0) };
        let ev = turn::add_event(
            db,
            &EventRow { session: &cx.session, turn: Some(turn.id), tool_use_id: tuid.as_deref(), tool: "Bash", agent: agent.as_deref(), paths_json: None, cmd_preview: cmd.as_deref(), exit_code: exit, failed, duration_ms: cx.p.i("duration_ms") },
            cx.now,
        )?;
        // Spec 21: the error key of a failed command (the key only, never its output). The output is what follows the first line of `error`.
        if let (true, Some(code), Some(err)) = (failed, exit, cx.p.v.get("error").and_then(|e| e.as_str())) {
            let output = err.split_once('\n').map_or("", |x| x.1);
            let key = errkey::key(&cx.p.input_str("command").unwrap_or_default(), code, output, &cx.ws.root.to_string_lossy(), &user_home().to_string_lossy());
            guarded(cx, || Ok(db.execute("UPDATE tool_events SET ek1=?2 WHERE id=?1", params![ev, key])?));
        }
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
    guarded(cx, || constraints::record(db, turn.id)); // spec 21: the user's always/never/must sentences
    if !failed {
        // Spec 21: did the last rebuild reach the AI? The transcript says so.
        let tp = cx.p.s("transcript_path");
        guarded(cx, || rebuild::verify(db, sid, tp.as_deref().map(Path::new)));
    }
    guarded(cx, || promise::upgrade_sources(db, &turn));
    // Promise lines (spec 14) are about the AI's promises, so `quiet` does not hide them and a turn that changed nothing still gets them.
    let mut lines: Vec<String> = Vec::new();
    if !failed {
        lines.extend(carried(cx));
        lines.extend(vault_notice(cx, true));
        lines.extend(guarded(cx, || promise::stop_notes(db, cx.now)).unwrap_or_default());
        for l in &lines {
            let chars = l.chars().count() as i64;
            guarded(cx, || Ok(db.execute("INSERT INTO push_log(session_id,turn_id,kind,chars,tokens,delivered) VALUES(?1,?2,'notice',?3,?4,1)", params![sid, turn.id, chars, (chars + 3) / 4])?));
        }
    }
    let notices = lines.clone();
    if !failed && end_pt.is_some() && meta_get(db, "quiet").as_deref() != Some("1") {
        if let Some(l) = stop_line(db, &turn, end_pt.unwrap_or(0))? {
            lines.insert(0, l);
        }
    }
    // Phase 2 (spec 18): the promise gate. A capture that was cut short is not the tree of the folder, so nothing is checked on it.
    if let (false, Some(c), Some(ep)) = (failed, &cap, end_pt) {
        if !c.partial {
            if gate::eligible_count(db) == 0 {
                guarded(cx, || gate::receipt_none(db, sid, turn.id, cx.now));
            } else {
                *cx.stop.borrow_mut() = Some(StopInfo { session: sid.to_string(), turn: turn.clone(), end_pt: ep, tree: c.tree_id, lines: lines.clone(), notices });
            }
        }
    }
    Ok((!lines.is_empty()).then(|| sys_message(&lines.join("\n"))))
}

/// Notices that the last Stop's block could not print (spec 18): told now, once.
fn carried(cx: &Cx) -> Vec<String> {
    let Some(j) = meta_get(cx.db, "stop_carry") else { return Vec::new() };
    guarded(cx, || Ok(cx.db.execute("DELETE FROM meta WHERE key='stop_carry'", [])?));
    serde_json::from_str::<Vec<String>>(&j).unwrap_or_default()
}

fn read_origin(cx: &Cx, turn: &Turn) -> Result<()> {
    read_origin_for(cx.db, &cx.session, cx.p.s("transcript_path"), turn, true)
}

/// Spec 4.4: who started `turn`, read from the session's own transcript. The CLI runs it mid-turn too (`save_offset` false: only Stop moves the offset).
pub fn read_origin_for(db: &Connection, session: &str, hook_tp: Option<String>, turn: &Turn, save_offset: bool) -> Result<()> {
    let row: Option<(Option<String>, i64)> = db.query_row("SELECT transcript_path,transcript_offset FROM sessions WHERE id=?1", [session], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let Some((tp, off)) = row else { return Ok(()) };
    let Some(tp) = tp.or(hook_tp) else { return Ok(()) };
    let pid = turn.prompt_id.as_deref().filter(|p| !p.starts_with("seq-"));
    let r = transcript::read_origin(Path::new(&tp), off, pid);
    if let Some(f) = r.found {
        db.execute("UPDATE turns SET origin=?2, origin_by=?3 WHERE id=?1 AND origin IN ('unknown','none')", params![turn.id, f.origin, f.by])?;
        crate::texts::set_origin(db, turn.id, &f.origin)?;
    }
    if save_offset {
        db.execute("UPDATE sessions SET transcript_offset=?2 WHERE id=?1", params![session, r.new_offset])?;
    }
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
    // A subagent's compaction carries the parent's ids and an agent_id: it does not start a new cycle of the conversation (spec 21).
    if cx.p.s("agent_id").is_none() {
        cx.db.execute("UPDATE sessions SET compact_epoch=compact_epoch+1 WHERE id=?1", [&cx.session])?;
    }
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
