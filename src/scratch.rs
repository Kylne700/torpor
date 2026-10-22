//! Scratch copies and the app process (spec 16): a saved version runs in its own folder, on its own port, with its own HOME.
//! Only `run.lock` is held while the app runs. The store lock and database transactions are never held across a run.
use crate::capture::{self, Req, flatten_tree};
use crate::runner::{self, Client, Judged, ReqErr, Verdict};
use crate::spec::{self, Spec};
use crate::store::{TreeEntry, Ws, mkdir_private, user_home};
use crate::util::{Error, Result, hex, now_ns, trunc_chars, unhex32};
use rusqlite::{Transaction, TransactionBehavior};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{PipeReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a run waits for another run in the same workspace (spec 16: one at a time).
pub const RUN_LOCK_WAIT_MS: u64 = 120_000;
const LOG_CAP: usize = 1 << 20;
const TAIL_KEEP: usize = 2048;
const STALE_SCRATCH_SECS: u64 = 3600;
const GRACE_MS: u64 = 3000;

const NODE_LOCKS: [&str; 5] = ["package-lock.json", "pnpm-lock.yaml", "yarn.lock", "bun.lock", "bun.lockb"];
const PY_LOCKS: [&str; 2] = ["poetry.lock", "uv.lock"];
/// Folders the scan leaves out of every tree and the lockfiles that say what is in them.
const DEP_DIRS: [(&str, &[&str]); 3] = [("node_modules", &NODE_LOCKS), (".venv", &PY_LOCKS), ("venv", &PY_LOCKS)];

fn unr(reason: impl Into<String>) -> Judged {
    Judged { verdict: Verdict::Unrunnable, failed_step: None, reason: trunc_chars(&reason.into(), 240), assertions: 0, token_ok: false }
}

// ---------------------------------------------------------------- the live project as a tree

/// Save the live project as a tree without writing a save point. A shared store lock and one short transaction; nothing else is held.
/// The tree is referenced by nothing, so GC sweeps it after the grace period (a run only needs its id for that long).
pub fn snapshot(ws: &mut Ws, budget: Duration) -> Result<[u8; 32]> {
    ws.lock_shared(crate::restore::LOCK_WAIT_MS)?;
    let r = (|| -> Result<capture::Captured> {
        let tx = Transaction::new_unchecked(&ws.conn, TransactionBehavior::Immediate)?;
        let c = capture::capture(ws, &tx, &Req { session: "", points: Vec::new(), turn: None, only_if_changed: false, budget: Some(budget) })?;
        tx.commit()?;
        Ok(c)
    })();
    ws.unlock();
    let c = r?;
    if c.partial {
        return Err(Error::msg("the project could not be saved in time"));
    }
    Ok(c.tree_id)
}

// ---------------------------------------------------------------- one run at a time

thread_local! {
    static HELD: Cell<bool> = const { Cell::new(false) };
}

/// `<ws>/run.lock`: held for a whole run (and never together with the store lock). Released when dropped.
pub struct RunLock {
    _f: Option<File>,
}

impl RunLock {
    /// Waits at most `wait_ms`, and never past this thread's deadline. Inside `hold_run_lock` the lock is already ours: a no-op.
    pub fn acquire(ws: &Ws, wait_ms: u64) -> Result<RunLock> {
        if HELD.with(|h| h.get()) {
            return Ok(RunLock { _f: None });
        }
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(ws.dir.join("run.lock"))?;
        let t = Instant::now();
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(RunLock { _f: Some(f) }),
                Err(fs::TryLockError::WouldBlock) => {
                    if t.elapsed() >= Duration::from_millis(wait_ms) || runner::out_of_time() {
                        return Err(Error::Busy);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }
}

/// The run lock taken once for a series of runs (the Stop gate: the 3 repeats and the control run belong together, spec 18).
/// While it lives, `RunLock::acquire` on this thread hands out a no-op. The flag is reset before the real lock is released.
pub struct HeldLock {
    _lock: RunLock,
}

pub fn hold_run_lock(ws: &Ws, wait_ms: u64) -> Result<HeldLock> {
    let _lock = RunLock::acquire(ws, wait_ms)?;
    HELD.with(|h| h.set(true)); // shortcut: a thread-wide flag, not tied to one workspace; the gate holds exactly one workspace, upgrade if a second can run at once
    Ok(HeldLock { _lock })
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        HELD.with(|h| h.set(false));
    }
}

// ---------------------------------------------------------------- the scratch folder

/// `<ws>/scratch/<run>/`: `app/` (the project copy and the app's cwd), `home/`, `tmp/`, `server.log`. Removed when dropped.
pub struct Scratch {
    pub dir: PathBuf,
    pub app: PathBuf,
    base: PathBuf,
}

impl Scratch {
    pub fn create(ws: &Ws) -> Result<Scratch> {
        let base = ws.dir.join("scratch");
        mkdir_private(&base)?;
        let dir = base.join(format!("{}-{}", now_ns(), runner::new_rand()));
        let app = dir.join("app");
        for d in [&app, &dir.join("home"), &dir.join("tmp")] {
            mkdir_private(d)?;
        }
        Ok(Scratch { dir, app, base })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        remove_scratch(&self.base, &self.dir);
    }
}

/// Delete `dir` only when it sits directly in `base` (symlinks inside are unlinked, never followed).
fn remove_scratch(base: &Path, dir: &Path) {
    if dir.parent() == Some(base) && dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        let _ = fs::remove_dir_all(dir);
    }
}

/// Leftovers of runs that were killed (older than 1 h).
fn gc_scratch(ws: &Ws) {
    let base = ws.dir.join("scratch");
    let Ok(rd) = fs::read_dir(&base) else { return };
    for e in rd.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d.as_secs() > STALE_SCRATCH_SECS);
        if old {
            remove_scratch(&base, &e.path());
        }
    }
}

/// A tree path that stays inside the folder: plain names only.
fn safe_rel(p: &str) -> bool {
    !p.is_empty() && Path::new(p).components().all(|c| matches!(c, Component::Normal(_))) && !p.contains('\0')
}

/// A link target that would leave the copy (absolute, or `..` past the root) is not recreated.
fn link_escapes(link_path: &str, target: &[u8]) -> bool {
    if target.is_empty() || target[0] == b'/' || target.contains(&0) {
        return true;
    }
    let mut depth = link_path.matches('/').count() as i64; // folders above the link inside the copy
    for part in target.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => depth += 1,
        }
    }
    false
}

fn is_dotenv(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(|n| n.starts_with(".env"))
}

/// Both lockfile sets equal (an absent file equals an absent file).
fn locks_equal(entries: &[(String, TreeEntry)], live: &Path, names: &[&str]) -> bool {
    names.iter().all(|n| {
        let in_tree = entries.iter().find(|(p, e)| p == n && e.kind == 'f').map(|(_, e)| e.id.clone());
        let on_disk = fs::read(live.join(n)).ok().map(|b| hex(blake3::hash(&b).as_bytes()));
        in_tree == on_disk
    })
}

pub struct Made {
    /// A dependency folder whose lockfile differs from the live project's: the run is UNRUNNABLE "needs-install".
    pub needs_install: Option<String>,
}

/// Write the tree into `app`, byte-exact: no secret files, no `.env*`, no files that were not saved, no links out of the copy.
/// Dependency folders become links to the live project's when their lockfiles are equal.
pub fn materialize(ws: &Ws, tree: &[u8; 32], app: &Path) -> Result<Made> {
    // No store lock here: a blob that vanished or changed is an error (the read re-hashes), which makes the run UNRUNNABLE.
    let entries = flatten_tree(ws, &ws.conn, tree)?;
    for (path, e) in &entries {
        if !safe_rel(path) {
            continue;
        }
        let dest = app.join(path);
        match e.kind {
            'd' => fs::create_dir_all(&dest)?,
            'f' if !is_dotenv(path) => {
                let Some(id) = unhex32(&e.id) else { continue };
                let bytes = ws.read_blob(&ws.conn, &id)?;
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mode = if e.mode == 755 { 0o755 } else { 0o644 };
                OpenOptions::new().write(true).create_new(true).mode(mode).open(&dest)?.write_all(&bytes)?;
            }
            'l' if !is_dotenv(path) => {
                let Some(id) = unhex32(&e.id) else { continue };
                let target = ws.read_blob(&ws.conn, &id)?;
                if link_escapes(path, &target) {
                    continue; // shortcut: absolute and outward links are left out; upgrade if a project needs them
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                symlink(std::ffi::OsStr::from_bytes(&target), &dest)?;
            }
            _ => {} // 's' (secret), 'x' (not saved), and .env* files
        }
    }
    let mut needs_install = None;
    for (dep, locks) in DEP_DIRS {
        let live = ws.root.join(dep);
        if !live.is_dir() {
            continue;
        }
        if locks_equal(&entries, &ws.root, locks) {
            let dest = app.join(dep);
            if dest.symlink_metadata().is_err() {
                symlink(&live, &dest)?;
            }
        } else if needs_install.is_none() {
            needs_install = Some(dep.to_string());
        }
    }
    Ok(Made { needs_install })
}

// ---------------------------------------------------------------- the app's environment

fn quote(v: &str) -> String {
    if v.bytes().all(|c| c.is_ascii_alphanumeric() || b"_./:@%+,-".contains(&c)) {
        v.to_string()
    } else {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

pub fn dotenv(vars: &[(String, String)]) -> String {
    vars.iter().map(|(k, v)| format!("{k}={}\n", quote(v))).collect()
}

/// The app's whole environment (spec 16): empty, then the toolchain variables, the spec's env, PORT; Torpor's own values win.
/// Returns (everything the process gets, the part that also goes into `.env`).
pub fn app_env(ws_dir: &Path, run: &Path, spec_env: &BTreeMap<String, String>, port: u16) -> (Vec<(String, String)>, Vec<(String, String)>) {
    let mut m: BTreeMap<String, String> = BTreeMap::new();
    for k in ["PATH", "LANG", "LC_ALL", "TERM", "SHELL", "USER"] {
        if let Some(v) = std::env::var_os(k).filter(|v| !v.is_empty()) {
            m.insert(k.to_string(), v.to_string_lossy().into_owned());
        }
    }
    m.entry("PATH".into()).or_insert_with(|| "/usr/local/bin:/usr/bin:/bin".into());
    let home = user_home();
    let defaults: [(&str, &[&str]); 7] = [
        ("CARGO_HOME", &[".cargo"]),
        ("RUSTUP_HOME", &[".rustup"]),
        ("NVM_DIR", &[".nvm"]),
        ("VOLTA_HOME", &[".volta"]),
        ("PNPM_HOME", &[".local/share/pnpm", "Library/pnpm"]),
        ("BUN_INSTALL", &[".bun"]),
        ("PYENV_ROOT", &[".pyenv"]),
    ];
    for (k, rels) in defaults {
        let real = std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let found = real.or_else(|| rels.iter().map(|r| home.join(r)).find(|d| d.is_dir()));
        if let Some(d) = found {
            m.insert(k.to_string(), d.to_string_lossy().into_owned());
        }
    }
    let mut own: Vec<(String, String)> = spec_env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    own.push(("PORT".into(), port.to_string()));
    own.push(("HOME".into(), run.join("home").to_string_lossy().into_owned()));
    own.push(("TMPDIR".into(), run.join("tmp").to_string_lossy().into_owned()));
    for (k, v) in &own {
        m.insert(k.clone(), v.clone());
    }
    m.insert("CARGO_TARGET_DIR".into(), ws_dir.join("cargo-target").to_string_lossy().into_owned());
    (m.into_iter().collect(), own)
}

// ---------------------------------------------------------------- the app process

#[derive(Default)]
struct Tail {
    buf: Vec<u8>,
    eof: bool,
}

/// Process groups of the apps this process started and has not stopped yet. Only the hook watchdog reads it (`kill_live_groups`),
/// so a hook that ends at its deadline does not leave its own apps running. Never a group id saved by an earlier process.
static LIVE: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// SIGKILL every app group this process started. For the watchdog thread right before `_exit`.
pub fn kill_live_groups() {
    if let Ok(g) = LIVE.try_lock() {
        for &pg in g.iter().filter(|p| **p > 1) {
            // SAFETY: the ids are groups this process created and has not yet reaped through App::drop.
            unsafe { libc::kill(-pg, libc::SIGKILL) };
        }
    }
}

/// The server in its own process group. Dropping it stops the whole group (SIGTERM, then SIGKILL after 3 s).
pub struct App {
    child: Child,
    pgid: i32,
    tail: Arc<Mutex<Tail>>,
}

fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only tests for existence of the group we created.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

impl Drop for App {
    fn drop(&mut self) {
        if self.pgid > 1 {
            // SAFETY: pgid is the process group this struct started (pgid > 1 is checked), nothing else is signalled.
            unsafe { libc::kill(-self.pgid, libc::SIGTERM) };
            let end = Instant::now() + Duration::from_millis(GRACE_MS);
            loop {
                let _ = self.child.try_wait(); // reaps the leader, so the group id can empty out
                if !group_alive(self.pgid) || Instant::now() >= end {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if group_alive(self.pgid) {
                // SAFETY: same group as above.
                unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Ok(mut g) = LIVE.lock() {
            g.retain(|p| *p != self.pgid); // only now: until the group is gone the watchdog may still need to kill it
        }
    }
}

fn pump(mut r: PipeReader, log: PathBuf, tail: Arc<Mutex<Tail>>) {
    let mut f = File::create(log).ok();
    let (mut written, mut buf) = (0usize, [0u8; 8192]);
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if let Some(f) = f.as_mut() {
            let take = n.min(LOG_CAP.saturating_sub(written));
            if take > 0 {
                let _ = f.write_all(&buf[..take]);
                written += take;
            }
        }
        if let Ok(mut t) = tail.lock() {
            t.buf.extend_from_slice(&buf[..n]);
            if t.buf.len() > TAIL_KEEP * 2 {
                let cut = t.buf.len() - TAIL_KEEP;
                t.buf.drain(..cut);
            }
        }
    }
    if let Ok(mut t) = tail.lock() {
        t.eof = true;
    }
}

impl App {
    /// `sh -c cmd` in `cwd` with exactly `env`, stdin closed, stdout and stderr into `log` (first 1 MiB) and a short tail.
    pub fn spawn(cmd: &str, cwd: &Path, env: &[(String, String)], log: PathBuf) -> Result<App> {
        let (rd, wr) = std::io::pipe()?;
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd).current_dir(cwd).env_clear().envs(env.iter().map(|(k, v)| (k, v))).stdin(Stdio::null()).stdout(wr.try_clone()?).stderr(wr).process_group(0);
        let child = c.spawn()?;
        drop(c); // closes our copies of the write end, so the reader sees the end when the group is gone
        let tail = Arc::new(Mutex::new(Tail::default()));
        let t2 = tail.clone();
        std::thread::spawn(move || pump(rd, log, t2));
        let pgid = child.id() as i32;
        if let Ok(mut g) = LIVE.lock() {
            g.push(pgid);
        }
        Ok(App { child, pgid, tail })
    }

    /// The exit text if the leader has ended.
    fn ended(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(st)) => Some(match st.code() {
                Some(c) => format!("exited {c}"),
                None => "was stopped by a signal".to_string(),
            }),
            _ => None,
        }
    }

    /// What the app printed last, on one line, without secrets or long runs. Waits a moment for the reader after an exit.
    pub fn tail(&self) -> String {
        for _ in 0..30 {
            if self.tail.lock().map(|t| t.eof).unwrap_or(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let raw = self.tail.lock().map(|t| t.buf.clone()).unwrap_or_default();
        let s = String::from_utf8_lossy(&raw[raw.len().saturating_sub(400)..]).into_owned();
        let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let (red, _) = crate::texts::redact(&one);
        crate::mask::mask_runs(&red)
    }
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

// ---------------------------------------------------------------- a run

pub struct Stopped {
    pub judged: Judged,
    pub tail: String,
}

fn stop(reason: impl Into<String>) -> Stopped {
    Stopped { judged: unr(reason), tail: String::new() }
}

/// A started app in a scratch copy. Field order is drop order: the app is stopped, then the folder goes, then the lock is released.
pub struct Run {
    pub port: u16,
    pub rand: String,
    pub nonce: String,
    pub app_dir: PathBuf,
    app: App,
    _scratch: Scratch,
    _lock: RunLock,
}

impl Run {
    pub fn tail(&self) -> String {
        self.app.tail()
    }

    /// Everything up to "the app answers its ready path" (spec 16). Err = UNRUNNABLE with the reason.
    pub fn start(ws: &Ws, spec: &Spec, tree: &[u8; 32]) -> std::result::Result<Run, Stopped> {
        let rand = runner::new_rand();
        let port = free_port().map_err(|e| stop(format!("no free port ({e})")))?;
        for (k, v) in &spec.env {
            if let Some(p) = spec::env_problem(k, &spec::subst(v, &rand, port)) {
                return Err(stop(format!("refused at run time: {p}")));
            }
        }
        if !spec.http_only() {
            return Err(stop("runner not installed: torpor runner install"));
        }
        let lock = RunLock::acquire(ws, RUN_LOCK_WAIT_MS).map_err(|_| stop(if runner::out_of_time() { runner::OUT_OF_TIME } else { "another check is still running in this project" }))?;
        gc_scratch(ws);
        let scratch = Scratch::create(ws).map_err(|e| stop(format!("no scratch folder ({e})")))?;
        let made = materialize(ws, tree, &scratch.app).map_err(|e| stop(format!("the saved version could not be copied ({e})")))?;
        if let Some(dep) = made.needs_install {
            return Err(stop(format!("needs-install: the saved version's lockfile differs from the one in the project, so {dep} cannot be reused")));
        }
        let spec_env: BTreeMap<String, String> = spec.env.iter().map(|(k, v)| (k.clone(), spec::subst(v, &rand, port))).collect();
        let (env, dotenv_part) = app_env(&ws.dir, &scratch.dir, &spec_env, port);
        let text = dotenv(&dotenv_part);
        for name in [".env.torpor", ".env"] {
            fs::write(scratch.app.join(name), &text).map_err(|e| stop(format!("no .env in the scratch copy ({e})")))?;
        }
        let cmd = spec::subst(&spec.start.cmd, &rand, port);
        let mut app = App::spawn(&cmd, &scratch.app, &env, scratch.dir.join("server.log")).map_err(|e| stop(format!("the app could not be started ({e})")))?;
        let shown = trunc_chars(&cmd, 40);
        let ready = spec::subst(&spec.start.ready, &rand, port);
        let deadline = Instant::now() + Duration::from_secs(spec.start.timeout_s);
        let mut client = Client::new(port);
        let mut last = "no answer".to_string();
        loop {
            if let Some(end) = app.ended() {
                let tail = app.tail();
                return Err(Stopped { judged: unr(format!("the app did not start ({shown} {end})")), tail });
            }
            match client.request("GET", &ready, None) {
                Ok(r) if (200..300).contains(&r.status) => break,
                Ok(r) => last = format!("GET {ready} gave {}", r.status),
                Err(ReqErr::Unrunnable(m)) => return Err(Stopped { judged: unr(m), tail: app.tail() }),
                Err(ReqErr::Down(_)) => {}
            }
            if Instant::now() >= deadline {
                let tail = app.tail();
                return Err(Stopped { judged: unr(format!("the app did not start ({shown}: {last} for {} s)", spec.start.timeout_s)), tail });
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let nonce = runner::new_nonce();
        Ok(Run { port, rand, nonce, app_dir: scratch.app.clone(), app, _scratch: scratch, _lock: lock })
    }
}

pub struct RunResult {
    pub judged: Judged,
    pub ms: u64,
    /// The app's last output (one line, redacted); only for a human reading `promise show`, never for a hook message.
    pub tail: String,
}

/// One run of a check against one saved tree, in a fresh scratch copy. `min_n` = the assertion count the promise had when it was saved.
pub fn run_once(ws: &Ws, spec: &Spec, tree: &[u8; 32], min_n: Option<usize>) -> RunResult {
    let t0 = Instant::now();
    let ms = |t: Instant| t.elapsed().as_millis() as u64;
    match Run::start(ws, spec, tree) {
        Err(s) => RunResult { judged: s.judged, ms: ms(t0), tail: s.tail },
        Ok(run) => {
            let mut lines = vec![serde_json::json!({"p": "SETUP_OK"}).to_string()];
            lines.extend(runner::run_http_steps(spec, run.port, &run.nonce, &run.rand));
            let judged = runner::judge(&lines, &run.nonce, min_n);
            let tail = run.tail();
            drop(run);
            RunResult { judged, ms: ms(t0), tail }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_names_that_leave_the_copy_are_refused() {
        assert!(safe_rel("a/b.txt") && !safe_rel("../x") && !safe_rel("a/../../x") && !safe_rel("/etc/passwd") && !safe_rel(""));
        assert!(link_escapes("l", b"/etc/passwd"));
        assert!(link_escapes("l", b"../x"));
        assert!(link_escapes("a/l", b"../../x"));
        assert!(!link_escapes("a/l", b"../x"));
        assert!(!link_escapes("a/l", b"b/c"));
        assert!(is_dotenv(".env") && is_dotenv("sub/.env.local") && is_dotenv(".env.example") && is_dotenv(".envrc"));
        assert!(!is_dotenv("environment.ts") && !is_dotenv("a.env") && !is_dotenv("sub/env"));
    }

    #[test]
    fn the_env_is_empty_plus_the_allowed_names_and_torpors_values_win() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spec_env = BTreeMap::new();
        spec_env.insert("APP_MODE".to_string(), "test mode".to_string());
        spec_env.insert("CARGO_TARGET_DIR".to_string(), "/elsewhere".to_string());
        let (all, own) = app_env(&tmp.path().join("ws"), &tmp.path().join("run"), &spec_env, 4321);
        let get = |k: &str| all.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("PORT").as_deref(), Some("4321"));
        assert_eq!(get("HOME"), Some(tmp.path().join("run/home").to_string_lossy().into_owned()));
        assert_eq!(get("TMPDIR"), Some(tmp.path().join("run/tmp").to_string_lossy().into_owned()));
        assert_eq!(get("CARGO_TARGET_DIR"), Some(tmp.path().join("ws/cargo-target").to_string_lossy().into_owned()), "the spec cannot move the build folder");
        assert!(get("PATH").is_some());
        let allowed = ["PATH", "LANG", "LC_ALL", "TERM", "SHELL", "USER", "CARGO_HOME", "RUSTUP_HOME", "NVM_DIR", "VOLTA_HOME", "PNPM_HOME", "BUN_INSTALL", "PYENV_ROOT", "APP_MODE", "PORT", "HOME", "TMPDIR", "CARGO_TARGET_DIR"];
        for (k, _) in &all {
            assert!(allowed.contains(&k.as_str()), "unexpected variable {k}");
        }
        let text = dotenv(&own);
        assert!(text.contains("APP_MODE=\"test mode\"\n") && text.contains("PORT=4321\n") && !text.contains("PATH="), "{text}");
        assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn the_scratch_folder_is_removed_only_inside_its_own_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("scratch");
        let inside = base.join("run1");
        fs::create_dir_all(inside.join("app")).unwrap();
        let outside = tmp.path().join("keep");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("f"), "x").unwrap();
        symlink(&outside, inside.join("app/node_modules")).unwrap();
        remove_scratch(&base, &outside); // not directly in the base: refused
        assert!(outside.join("f").exists());
        remove_scratch(&base, &inside);
        assert!(!inside.exists());
        assert!(outside.join("f").exists(), "a link inside the scratch folder is unlinked, never followed");
    }

    fn alive(pid: &str) -> bool {
        let o = Command::new("ps").args(["-o", "pid,comm", "-p", pid]).output().unwrap();
        let t = String::from_utf8_lossy(&o.stdout);
        t.lines().skip(1).any(|l| l.trim_start().starts_with(pid) && !l.contains("defunct"))
    }

    #[test]
    fn dropping_the_app_stops_the_whole_group() {
        let tmp = tempfile::tempdir().unwrap();
        let env = vec![("PATH".to_string(), std::env::var("PATH").unwrap_or_default())];
        // The shell stays alive and starts a grandchild that ignores SIGTERM, so only the SIGKILL after the grace period ends it.
        let app = App::spawn("(trap '' TERM; sleep 120) & echo child=$!; wait", tmp.path(), &env, tmp.path().join("log")).unwrap();
        let mut pid = String::new();
        for _ in 0..100 {
            let t = app.tail();
            if let Some(p) = t.strip_prefix("child=") {
                pid = p.trim().to_string();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!pid.is_empty(), "the grandchild reported its id");
        assert!(alive(&pid));
        let leader = app.pgid;
        drop(app);
        assert!(!alive(&pid), "the grandchild is gone");
        assert!(!group_alive(leader), "the group is empty");
        assert!(fs::read_to_string(tmp.path().join("log")).unwrap().contains("child="));
    }

    #[test]
    fn a_second_run_waits_for_the_first_and_then_gives_up() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Ws::open_in(&tmp.path().join("home"), &tmp.path().join("proj"), true).unwrap();
        let first = RunLock::acquire(&ws, 100).unwrap();
        let t = Instant::now();
        assert!(matches!(RunLock::acquire(&ws, 200), Err(Error::Busy)));
        assert!(t.elapsed() >= Duration::from_millis(200));
        drop(first);
        assert!(RunLock::acquire(&ws, 100).is_ok());
    }
}
