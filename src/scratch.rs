//! Scratch copies and the app process (spec 16): a saved version runs in its own folder, on its own port, with its own HOME.
//! Only `run.lock` is held while the app runs. The store lock and database transactions are never held across a run (`torpor undo` plans under its
//! lock, lets go, runs its checks, and plans again, see merge.rs).
use crate::capture::{self, Req, flatten_tree};
use crate::dom;
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
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
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
        let dir = base.join(format!("{}-{}", now_ns(), runner::new_rand()?));
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

/// Leftovers of runs that were killed. Called with the run lock held, so no app of this project is alive: a folder with a `*.pgid` file (written
/// when an app started) is a leftover, and its groups are killed, but only for a folder younger than 1 h, so a group id that the system gave to
/// someone else since is not hit. A folder without one may be held by this very process (a saved copy, the coverage run): only old ones go.
/// Old temp folders of the browser go too.
fn gc_scratch(ws: &Ws) {
    reap_dir(&ws.dir.join("scratch"));
    dom::reap_tmp(Duration::from_secs(STALE_SCRATCH_SECS));
}

fn reap_dir(base: &Path) {
    let Ok(rd) = fs::read_dir(base) else { return };
    for e in rd.flatten() {
        let age = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map_or(u64::MAX, |d| d.as_secs());
        let mut left_by_a_run = false;
        for f in fs::read_dir(e.path()).into_iter().flatten().flatten() {
            if !f.file_name().to_string_lossy().ends_with(".pgid") {
                continue;
            }
            left_by_a_run = true;
            if let Some(pg) = fs::read_to_string(f.path()).ok().and_then(|t| t.trim().parse::<i32>().ok()).filter(|pg| *pg > 1 && age <= STALE_SCRATCH_SECS) {
                // SAFETY: the group id was written by the process that started this group; it is killed only while the folder is young.
                unsafe { libc::kill(-pg, libc::SIGKILL) };
            }
        }
        if left_by_a_run || age > STALE_SCRATCH_SECS {
            remove_scratch(base, &e.path());
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
    let mut links: Vec<&str> = Vec::new();
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
                links.push(path);
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
    drop_outward_links(app, &links)?;
    Ok(Made { needs_install })
}

/// Removes the saved links that resolve outside the copy or to the copy's own folder. The text check in `link_escapes` cannot see chains: `d/b -> ..`
/// and `d/c -> b/..` each look inward, yet `d/c` leads out through `d/b`. A link that ends inside a linked dependency folder stays (those point out on purpose).
/// shortcut: a link that cannot be resolved now (dangling or a loop) is kept, and one that starts to resolve only after the app creates something is not seen.
fn drop_outward_links(app: &Path, links: &[&str]) -> Result<()> {
    let root = fs::canonicalize(app)?;
    let deps: Vec<PathBuf> = DEP_DIRS.iter().filter_map(|(d, _)| fs::canonicalize(app.join(d)).ok()).collect();
    // All are judged before any is removed, so the answer does not depend on the order of the links.
    let out: Vec<&str> = links
        .iter()
        .copied()
        .filter(|l| fs::canonicalize(app.join(l)).is_ok_and(|t| t == root || !(t.starts_with(&root) || deps.iter().any(|d| t.starts_with(d)))))
        .collect();
    for l in out {
        fs::remove_file(app.join(l))?;
    }
    Ok(())
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
    /// The first `LOG_CAP` bytes, the same as the log file gets. A page check is judged from these and never from the file: the app under
    /// test runs next to that file and could rewrite it.
    head: Vec<u8>,
    eof: bool,
}

/// Process groups of the apps this process started and has not stopped yet (0 = free slot). Only the hook watchdog reads it
/// (`kill_live_groups`), so a hook that ends at its deadline does not leave its own apps running. Never a group id saved by an
/// earlier process. No lock on purpose: the watchdog must reach every id whatever another thread holds, or died holding.
/// shortcut: 64 apps at once, far above what one hook starts (a full table stops the new app and fails the run), raise it if that changes.
const LIVE_SLOTS: usize = 64;
static LIVE: [AtomicI32; LIVE_SLOTS] = [const { AtomicI32::new(0) }; LIVE_SLOTS];
/// Spawns in flight: the group of an app that is being started is not in `LIVE` until `Command::spawn` has returned.
static SPAWNING: AtomicUsize = AtomicUsize::new(0);
/// The watchdog waits at most this long for a spawn in flight (it fires at 30 s of a 45 s hook timeout), then kills what it has.
const SPAWN_WAIT_MS: u64 = 1000;

/// Counts one spawn in flight until it is dropped.
struct Spawning<'a>(&'a AtomicUsize);

impl<'a> Spawning<'a> {
    fn begin(n: &'a AtomicUsize) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        Spawning(n)
    }
}

impl Drop for Spawning<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn live_add(table: &[AtomicI32], pg: i32) -> bool {
    table.iter().any(|s| s.compare_exchange(0, pg, Ordering::SeqCst, Ordering::SeqCst).is_ok())
}

fn live_del(table: &[AtomicI32], pg: i32) {
    for s in table {
        let _ = s.compare_exchange(pg, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// SIGKILL every group in `table`, after waiting up to `wait` for the spawns in flight to register theirs. No lock, no unbounded wait.
fn kill_groups(table: &[AtomicI32], spawning: &AtomicUsize, wait: Duration) {
    let end = Instant::now() + wait;
    while spawning.load(Ordering::SeqCst) > 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
    }
    for pg in table.iter().map(|s| s.load(Ordering::SeqCst)).filter(|p| *p > 1) {
        // SAFETY: the ids are groups this process created and has not yet reaped through App::drop.
        unsafe { libc::kill(-pg, libc::SIGKILL) };
    }
}

/// SIGKILL every app group this process started. For the watchdog thread right before `_exit`.
pub fn kill_live_groups() {
    kill_groups(&LIVE, &SPAWNING, Duration::from_millis(SPAWN_WAIT_MS));
}

/// The server in its own process group. Dropping it stops the whole group (SIGTERM, then SIGKILL after 3 s).
pub struct App {
    child: Child,
    pgid: i32,
    tail: Arc<Mutex<Tail>>,
    stopped: bool,
}

fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only tests for existence of the group we created.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
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
            let room = LOG_CAP.saturating_sub(t.head.len());
            t.head.extend_from_slice(&buf[..n.min(room)]);
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
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        App::launch(c, cwd, env, log, None)
    }

    /// `c` in `cwd` with exactly `env`, in its own process group, stdout and stderr into `log` (first 1 MiB; the same bytes stay in memory, see `head`) and a short tail.
    /// stdin is closed, or carries `input` (the browser runner reads its spec there, never from argv or the environment).
    pub fn launch(mut c: Command, cwd: &Path, env: &[(String, String)], log: PathBuf, input: Option<Vec<u8>>) -> Result<App> {
        let (rd, wr) = std::io::pipe()?;
        let stdin = if input.is_some() { Stdio::piped() } else { Stdio::null() };
        c.current_dir(cwd).env_clear().envs(env.iter().map(|(k, v)| (k, v))).stdin(stdin).stdout(wr.try_clone()?).stderr(wr).process_group(0);
        let spawning = Spawning::begin(&SPAWNING);
        let mut child = c.spawn()?;
        let pgid = child.id() as i32;
        let registered = live_add(&LIVE, pgid);
        let _ = fs::write(format!("{}.pgid", log.display()), pgid.to_string()); // for the next run's reaping if this process is killed
        drop(spawning);
        let to_child = child.stdin.take();
        // From here on `app` owns the group: every way out below (an error, a panic) drops it, and that stops the group.
        let app = App { child, pgid, tail: Arc::new(Mutex::new(Tail::default())), stopped: false };
        if !registered {
            return Err(Error::msg("too many apps are running at once"));
        }
        drop(c); // closes our copies of the write end, so the reader sees the end when the group is gone
        let t2 = app.tail.clone();
        std::thread::Builder::new().spawn(move || pump(rd, log, t2))?;
        if let (Some(mut w), Some(bytes)) = (to_child, input) {
            // A thread, so a spec bigger than the pipe cannot block us; it ends when the child has read it (or is gone).
            std::thread::Builder::new().spawn(move || {
                let _ = w.write_all(&bytes);
            })?;
        }
        Ok(app)
    }

    /// SIGTERM to the group, SIGKILL after 3 s, and the leader reaped. Safe to call twice.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
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
        live_del(&LIVE, self.pgid); // only now: until the group is gone the watchdog may still need to kill it
    }

    /// Wait for the leader to end. False when `limit` or the deadline of this thread passes first (the caller then stops the group).
    pub fn wait_end(&mut self, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        loop {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            if Instant::now() >= end || runner::out_of_time() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    /// Wait up to 2 s for the reader to have written everything the group printed.
    pub fn wait_eof(&self) {
        for _ in 0..200 {
            if self.tail.lock().map(|t| t.eof).unwrap_or(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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

    /// The first 1 MiB the group printed, as the reader took it off the pipe (call `wait_eof` first).
    pub fn head(&self) -> Vec<u8> {
        self.tail.lock().map(|t| t.head.clone()).unwrap_or_default()
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

/// A free port for the `attempt`th start of an app (0, or 1 for the retry).
#[cfg_attr(not(feature = "fault-inject"), allow(unused_variables))]
fn free_port(attempt: usize) -> Result<u16> {
    // Tests: the ports of the 1st and the 2nd start, comma separated, to put the app on a port somebody else holds (fault-inject builds only).
    #[cfg(feature = "fault-inject")]
    if let Some(p) = std::env::var("TORPOR_TEST_PORT").ok().and_then(|s| s.split(',').nth(attempt)?.trim().parse::<u16>().ok()) {
        return Ok(p);
    }
    // Tests: one window of ports per test, so two tests starting an app in the same moment never pick the same port (fault-inject builds only).
    #[cfg(feature = "fault-inject")]
    if let Some((lo, hi)) = std::env::var("TORPOR_TEST_PORTS").ok().and_then(|s| {
        let (a, b) = s.split_once('-')?;
        Some((a.parse::<u16>().ok()?, b.parse::<u16>().ok()?))
    }) {
        let (lo, n, mut r) = (u32::from(lo), u32::from(hi.saturating_sub(lo).max(1)), [0u8; 4]);
        let _ = getrandom::fill(&mut r);
        for i in 0..n {
            let p = (lo + (u32::from_le_bytes(r) % n + i) % n) as u16; // below hi, so it fits
            if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
                return Ok(p);
            }
        }
    }
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

/// Who holds the socket that listens on a port. Only the Linux `holder_of` finds an App or an Other.
#[derive(Debug, PartialEq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Holder {
    /// A process of the app (its group, or started by one of its processes).
    App,
    /// Another process (its id): whatever answered on the port was not the app.
    Other(u32),
    /// Nobody Torpor can see: other users' processes, no /proc (macOS). The answer is trusted then.
    Unknown,
}

/// The sockets that listen on `port` in the text of /proc/net/tcp or tcp6, as the "socket:[inode]" text a link in /proc/<pid>/fd shows.
/// A line reads `sl local_address rem_address st tx:rx tr:tm retrnsmt uid timeout inode ...`; state 0A is LISTEN, address and port are in hex.
/// Only the addresses that a client at 127.0.0.1 reaches count: 127.0.0.1, any IPv4 address, any IPv6 address (it takes IPv4 too) and
/// ::ffff:127.0.0.1. A listener on ::1 or 127.0.0.2 can sit beside another process on 127.0.0.1:port and never answers that client.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn listening_sockets(table: &str, port: u16) -> Vec<String> {
    const REACHED: [&str; 4] = ["0100007F", "00000000", "00000000000000000000000000000000", "0000000000000000FFFF00000100007F"];
    let rows = table.lines().skip(1).map(|l| l.split_whitespace().collect::<Vec<_>>());
    rows.filter(|f| f.get(3) == Some(&"0A") && f.get(1).and_then(|a| a.split_once(':')).is_some_and(|(addr, p)| REACHED.contains(&addr) && u16::from_str_radix(p, 16) == Ok(port)))
        .filter_map(|f| f.get(9).map(|i| format!("socket:[{i}]")))
        .collect()
}

/// The processes of an app: its group and everything started below them, from (pid, parent, group) of every process.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn app_pids(procs: &[(u32, u32, i32)], pgid: i32) -> std::collections::BTreeSet<u32> {
    let mut ours: std::collections::BTreeSet<u32> = procs.iter().filter(|p| p.2 == pgid).map(|p| p.0).collect();
    while let Some(add) = procs.iter().find(|p| ours.contains(&p.1) && !ours.contains(&p.0)).map(|p| p.0) {
        ours.insert(add);
    }
    ours
}

/// Who holds the listening socket on `port`: a process of the app in group `pgid`, another process, or nobody we can see. Linux reads /proc;
/// elsewhere it is Unknown, so a port taken by someone else goes unnoticed there. shortcut: no lsof for macOS, upgrade when that matters.
#[cfg(target_os = "linux")]
fn holder_of(pgid: i32, port: u16) -> Holder {
    let sockets: Vec<String> = ["/proc/net/tcp", "/proc/net/tcp6"].iter().flat_map(|t| listening_sockets(&fs::read_to_string(t).unwrap_or_default(), port)).collect();
    if sockets.is_empty() {
        return Holder::Unknown;
    }
    let holds = |pid: u32| {
        let fds = fs::read_dir(format!("/proc/{pid}/fd"));
        fds.is_ok_and(|d| d.flatten().any(|e| fs::read_link(e.path()).is_ok_and(|t| sockets.iter().any(|s| t.to_str() == Some(s.as_str())))))
    };
    let procs: Vec<(u32, u32, i32)> = fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            // "pid (name) S parent group ...": the name may hold spaces and brackets, so the fields count from its last ")".
            let stat = fs::read_to_string(e.path().join("stat")).ok()?;
            let mut f = stat.rsplit_once(')')?.1.split_whitespace().skip(1);
            Some((pid, f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .collect();
    let ours = app_pids(&procs, pgid);
    if ours.iter().any(|p| holds(*p)) {
        return Holder::App;
    }
    procs.iter().map(|p| p.0).find(|p| holds(*p)).map_or(Holder::Unknown, Holder::Other)
}

#[cfg(not(target_os = "linux"))]
fn holder_of(_pgid: i32, _port: u16) -> Holder {
    Holder::Unknown
}

// ---------------------------------------------------------------- a run

pub struct Stopped {
    pub judged: Judged,
    pub tail: String,
}

fn stop(reason: impl Into<String>) -> Stopped {
    Stopped { judged: unr(reason), tail: String::new() }
}

/// What differs from a plain run: files written over the copy after it is made (`None` deletes the file) and variables added to the app's
/// environment for this run only (a value for `NODE_OPTIONS` is appended to the spec's).
#[derive(Default, Clone)]
pub struct Opts {
    pub overlay: Vec<(String, Option<Vec<u8>>)>,
    /// Overlay paths that end up executable even when no executable file was there before (a script an undo brings back).
    pub exec: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Tests only: a script run by node in place of the installed `run.mjs` (a stand-in that prints protocol lines). No install is needed then.
    pub runner: Option<PathBuf>,
}

/// May a changed copy write this path? Not one that leaves the copy, not an `.env*` file, not inside a dependency folder.
pub fn overlay_ok(rel: &str) -> bool {
    safe_rel(rel) && !is_dotenv(rel) && !rel.split('/').any(|c| DEP_DIRS.iter().any(|(d, _)| *d == c))
}

/// Each path is removed and written new, so a link in its place is replaced, never followed; a path that leaves the copy, a dependency
/// folder (linked to the live project) or an `.env*` file is refused.
fn apply_overlay(app: &Path, overlay: &[(String, Option<Vec<u8>>)], exec: &[String]) -> Result<()> {
    let root = fs::canonicalize(app)?;
    for (rel, content) in overlay {
        if !overlay_ok(rel) {
            return Err(Error::Msg(format!("{rel} is not a file Torpor changes")));
        }
        let dest = app.join(rel);
        let parent = dest.parent().unwrap_or(app);
        if content.is_some() {
            // The missing folders are made below the nearest one that exists: a link in the way must not lead out of the copy before anything is made.
            if !parent.ancestors().find_map(|a| fs::canonicalize(a).ok()).is_some_and(|p| p.starts_with(&root)) {
                return Err(Error::Msg(format!("{rel} leaves the copy")));
            }
            fs::create_dir_all(parent)?;
        }
        if !fs::canonicalize(parent).is_ok_and(|p| p.starts_with(&root)) {
            return Err(Error::Msg(format!("{rel} leaves the copy")));
        }
        let mode = match fs::symlink_metadata(&dest) {
            Ok(m) if m.is_file() && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0 => 0o755,
            _ if exec.contains(rel) => 0o755,
            _ => 0o644,
        };
        match fs::remove_file(&dest) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        if let Some(bytes) = content {
            OpenOptions::new().write(true).create_new(true).mode(mode).open(&dest)?.write_all(bytes)?;
        }
    }
    Ok(())
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

    /// The run's own folder (the app's copy, home and temp folders are inside it).
    pub fn dir(&self) -> &Path {
        &self._scratch.dir
    }

    /// Everything up to "the app answers its ready path" (spec 16). Err = UNRUNNABLE with the reason.
    pub fn start(ws: &Ws, spec: &Spec, tree: &[u8; 32]) -> std::result::Result<Run, Stopped> {
        Run::start_with(ws, spec, tree, &Opts::default())
    }

    /// `start`, with files written over the copy and variables added for this run only (a mutant, a dependency-recording run).
    pub fn start_with(ws: &Ws, spec: &Spec, tree: &[u8; 32], opts: &Opts) -> std::result::Result<Run, Stopped> {
        // Before anything starts: without random numbers the check cannot run (the run token must not be a fixed one).
        let no_random = |e: std::io::Error| stop(format!("the system gave no random numbers ({e})"));
        let rand = runner::new_rand().map_err(no_random)?;
        let nonce = runner::new_nonce().map_err(no_random)?;
        for (k, v) in &spec.env {
            // The port is picked later, right before the app starts. No rule here depends on its digits, only on there being two or more.
            if let Some(p) = spec::env_problem(k, &spec::subst(v, &rand, PROVISIONAL_PORT)) {
                return Err(stop(format!("refused at run time: {p}")));
            }
        }
        if !spec.http_only() {
            dom::preflight(&ws.home, opts).map_err(stop)?;
        }
        let lock = RunLock::acquire(ws, RUN_LOCK_WAIT_MS).map_err(|_| stop(if runner::out_of_time() { runner::OUT_OF_TIME } else { "another check is still running in this project" }))?;
        gc_scratch(ws);
        let scratch = Scratch::create(ws).map_err(|e| stop(format!("no scratch folder ({e})")))?;
        let made = materialize(ws, tree, &scratch.app).map_err(|e| stop(format!("the saved version could not be copied ({e})")))?;
        if let Some(dep) = made.needs_install {
            return Err(stop(format!("needs-install: the saved version's lockfile differs from the one in the project, so {dep} cannot be reused")));
        }
        apply_overlay(&scratch.app, &opts.overlay, &opts.exec).map_err(|e| stop(format!("the changed copy could not be made ({e})")))?;
        // The port is picked here, as late as it can be: another process may take it between the pick and the app's bind. When the app ends at
        // once, or a process that is not the app answers on the port, the app is started once more on another port.
        let mut again = false;
        loop {
            let port = free_port(usize::from(again)).map_err(|e| stop(format!("no free port ({e})")))?;
            match start_app(ws, spec, &scratch, opts, &rand, port) {
                Ok(app) => {
                    return Ok(Run { port, rand, nonce, app_dir: scratch.app.clone(), app, _scratch: scratch, _lock: lock });
                }
                Err(n) if n.retry && !again && !runner::out_of_time() => again = true,
                Err(n) => return Err(n.stopped),
            }
        }
    }
}

/// A port with two or more digits, for the one rule of the spec's env values that counts them, before the real port is picked.
const PROVISIONAL_PORT: u16 = 40000;
/// An app that ends within this long after its start has "ended at once".
const AT_ONCE: Duration = Duration::from_secs(3);

/// An app that did not come up: why, and whether one more start on another port may work.
struct NoStart {
    stopped: Stopped,
    retry: bool,
}

/// One start of the app on `port`, up to "its ready path answers" (spec 16). The answer must come from the app itself: on Linux, a 2xx that
/// a process outside the app gives on that port is not trusted. Torpor never stops such a process, only its own app.
fn start_app(ws: &Ws, spec: &Spec, scratch: &Scratch, opts: &Opts, rand: &str, port: u16) -> std::result::Result<App, NoStart> {
    let plain = |stopped: Stopped| NoStart { stopped, retry: false };
    let spec_env: BTreeMap<String, String> = spec.env.iter().map(|(k, v)| (k.clone(), spec::subst(v, rand, port))).collect();
    let (mut env, dotenv_part) = app_env(&ws.dir, &scratch.dir, &spec_env, port);
    for (k, v) in &opts.env {
        match env.iter_mut().find(|(n, _)| n == k) {
            Some((_, old)) if k == "NODE_OPTIONS" => *old = format!("{old} {v}"),
            Some((_, old)) => *old = v.clone(),
            None => env.push((k.clone(), v.clone())),
        }
    }
    let text = dotenv(&dotenv_part);
    for name in [".env.torpor", ".env"] {
        fs::write(scratch.app.join(name), &text).map_err(|e| plain(stop(format!("no .env in the scratch copy ({e})"))))?;
    }
    let cmd = spec::subst(&spec.start.cmd, rand, port);
    let started = Instant::now();
    let mut app = App::spawn(&cmd, &scratch.app, &env, scratch.dir.join("server.log")).map_err(|e| plain(stop(format!("the app could not be started ({e})"))))?;
    let shown = trunc_chars(&cmd, 40);
    let ready = spec::subst(&spec.start.ready, rand, port);
    let deadline = started + Duration::from_secs(spec.start.timeout_s);
    let mut client = Client::new(port);
    let mut last = "no answer".to_string();
    loop {
        if let Some(end) = app.ended() {
            let tail = app.tail();
            return Err(NoStart { stopped: Stopped { judged: unr(format!("the app did not start ({shown} {end})")), tail }, retry: started.elapsed() < AT_ONCE });
        }
        match client.request("GET", &ready, None) {
            Ok(r) if (200..300).contains(&r.status) => match holder_of(app.pgid, port) {
                Holder::Other(pid) => return Err(NoStart { stopped: stop(format!("port {port} is used by another process (pid {pid}), not by the app")), retry: true }),
                _ => return Ok(app),
            },
            Ok(r) => last = format!("GET {ready} gave {}", r.status),
            Err(ReqErr::Unrunnable(m)) => return Err(plain(Stopped { judged: unr(m), tail: app.tail() })),
            Err(ReqErr::Down(_)) => {}
        }
        if Instant::now() >= deadline {
            let tail = app.tail();
            return Err(plain(Stopped { judged: unr(format!("the app did not start ({shown}: {last} for {} s)", spec.start.timeout_s)), tail }));
        }
        std::thread::sleep(Duration::from_millis(250));
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
    run_once_with(ws, spec, tree, min_n, &Opts::default())
}

pub fn run_once_with(ws: &Ws, spec: &Spec, tree: &[u8; 32], min_n: Option<usize>, opts: &Opts) -> RunResult {
    let t0 = Instant::now();
    let ms = |t: Instant| t.elapsed().as_millis() as u64;
    match Run::start_with(ws, spec, tree, opts) {
        Err(s) => RunResult { judged: s.judged, ms: ms(t0), tail: s.tail },
        Ok(run) => {
            let lines = if spec.http_only() {
                let mut l = vec![serde_json::json!({"p": "SETUP_OK"}).to_string()];
                l.extend(runner::run_http_steps(spec, run.port, &run.nonce, &run.rand));
                l
            } else {
                dom::run_steps(&ws.home, spec, &run, opts)
            };
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
    fn an_overlay_file_is_executable_when_asked_or_when_the_file_it_replaces_was() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path();
        fs::write(app.join("old.sh"), "old").unwrap();
        fs::set_permissions(app.join("old.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let put = |p: &str| (p.to_string(), Some(b"x".to_vec()));
        apply_overlay(app, &[put("old.sh"), put("bin/new.sh"), put("plain.txt")], &["bin/new.sh".to_string()]).unwrap();
        let exec = |p: &str| fs::metadata(app.join(p)).unwrap().permissions().mode() & 0o111 != 0;
        assert_eq!([exec("old.sh"), exec("bin/new.sh"), exec("plain.txt")], [true, true, false]);
    }

    #[test]
    fn the_listening_sockets_of_a_port_are_read_from_the_kernel_table() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
            0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0000000000000000 100 0 0 10 0\n\
            1: 0100007F:1F90 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 222 1 0000000000000000 100 0 0 10 0\n\
            2: 00000000:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 333 1 0000000000000000 100 0 0 10 0\n\
            3: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 444 1 0000000000000000 100 0 0 10 0\n\
            4: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 555 1 0000000000000000 100 0 0 10 0\n\
            5: 0200007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 666 1 0000000000000000 100 0 0 10 0\n\
            6: 0000000000000000FFFF00000100007F:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777 1 0000000000000000 100 0 0 10 0\n";
        // 0x1F90 is 8080. A connected socket (state 01) is no listener, the IPv6 forms count, and so do not the listeners on ::1 (555) and
        // 127.0.0.2 (666): a client at 127.0.0.1 never reaches them.
        assert_eq!(listening_sockets(table, 8080), ["socket:[111]", "socket:[444]", "socket:[777]"]);
        assert_eq!(listening_sockets(table, 8081), ["socket:[333]"]);
        assert!(listening_sockets(table, 9).is_empty() && listening_sockets("", 8080).is_empty());
    }

    #[test]
    fn the_app_is_its_group_and_everything_started_below_it() {
        // (pid, parent, group): 10 leads group 10 with a child 11; 12 was started by 11 in a group of its own and 13 by 12; 20 and 21 are unrelated.
        let procs = [(10, 1, 10), (11, 10, 10), (12, 11, 12), (13, 12, 13), (20, 1, 20), (21, 20, 20)];
        assert_eq!(app_pids(&procs, 10).into_iter().collect::<Vec<_>>(), [10, 11, 12, 13]);
        assert!(app_pids(&procs, 99).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_listening_socket_is_told_to_belong_to_its_holder() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        // SAFETY: getpgrp has no arguments and cannot fail.
        let mine = unsafe { libc::getpgrp() };
        assert_eq!(holder_of(mine, port), Holder::App, "this process listens, and it is in its own group");
        let mut other = group(); // a sleeping process in a group of its own
        assert_eq!(holder_of(other.id() as i32, port), Holder::Other(std::process::id()));
        drop(l);
        assert_eq!(holder_of(mine, port), Holder::Unknown, "nothing listens now");
        let _ = other.kill();
        let _ = other.wait();
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

    #[test]
    fn chained_links_that_lead_out_are_dropped_and_no_folder_is_made_through_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, out, live) = (tmp.path().join("app"), tmp.path().join("out"), tmp.path().join("live"));
        for d in ["app/d", "app/src", "app/bin", "out", "live/node_modules/.bin"] {
            fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        fs::write(app.join("src/a.txt"), "A").unwrap();
        fs::write(live.join("node_modules/.bin/tool"), "T").unwrap();
        symlink(live.join("node_modules"), app.join("node_modules")).unwrap(); // a dependency link, made on purpose
        // Every one of these passes the text check; `d/c` leads out only through `d/b` (which is the copy's own folder).
        let links = [("d/b", ".."), ("d/c", "b/.."), ("d/in", "../src/a.txt"), ("d/dangling", "../nothing"), ("bin/tool", "../node_modules/.bin/tool"), ("bin/up", "../node_modules/..")];
        for (l, t) in links {
            assert!(!link_escapes(l, t.as_bytes()), "{l}");
            symlink(t, app.join(l)).unwrap();
        }
        drop_outward_links(&app, &links.map(|(l, _)| l)).unwrap();
        let present = |l: &str| app.join(l).symlink_metadata().is_ok();
        assert_eq!(links.map(|(l, _)| present(l)), [false, false, true, true, true, false], "d/b, d/c and bin/up are dropped; the rest stays");
        // Nothing is made below a link that leads out, even one that got through.
        symlink("../../out", app.join("d/o")).unwrap();
        let r = apply_overlay(&app, &[("d/o/e/f.txt".to_string(), Some(b"x".to_vec()))], &[]);
        assert!(matches!(&r, Err(Error::Msg(m)) if m.contains("leaves the copy")), "{r:?}");
        assert!(!out.join("e").exists(), "a folder was made outside the copy");
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

    /// A sleeping process in a group of its own, the way an app is started.
    fn group() -> Child {
        Command::new("sleep").arg("120").process_group(0).spawn().unwrap()
    }

    #[test]
    fn the_kill_takes_no_lock_waits_for_spawns_in_flight_but_only_so_long() {
        use std::os::unix::process::ExitStatusExt;
        let table: [AtomicI32; 2] = Default::default();
        let none = AtomicUsize::new(0);
        // Slots: a full table says so, a freed slot is used again, and a freed group is not signalled.
        let (mut a, mut b) = (group(), group());
        let (ia, ib) = (a.id() as i32, b.id() as i32);
        assert!(live_add(&table, ia) && live_add(&table, ib) && !live_add(&table, 99_999));
        live_del(&table, ib);
        kill_groups(&table, &none, Duration::from_millis(50));
        assert_eq!(a.wait().unwrap().signal(), Some(9));
        assert!(b.try_wait().unwrap().is_none(), "b was freed first, so it was left alone");
        b.kill().unwrap();
        b.wait().unwrap();
        // A spawn in flight registers its group while the watchdog waits: that group is killed too.
        live_del(&table, ia);
        let in_flight = AtomicUsize::new(0);
        let (t, f) = (&table, &in_flight);
        let mut c = std::thread::scope(|s| {
            let guard = Spawning::begin(f);
            let h = s.spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                let c = group();
                assert!(live_add(t, c.id() as i32));
                drop(guard);
                c
            });
            kill_groups(t, f, Duration::from_secs(10));
            h.join().unwrap()
        });
        assert_eq!(c.wait().unwrap().signal(), Some(9));
        // A spawn that never ends holds the watchdog up for the limit and no longer.
        let stuck = AtomicUsize::new(1);
        let mut d = group();
        live_del(&table, c.id() as i32);
        assert!(live_add(&table, d.id() as i32));
        let t0 = Instant::now();
        kill_groups(&table, &stuck, Duration::from_millis(200));
        assert!((200..5000).contains(&(t0.elapsed().as_millis() as u64)), "{:?}", t0.elapsed());
        assert_eq!(d.wait().unwrap().signal(), Some(9));
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

    #[test]
    fn a_killed_run_leaves_a_group_that_the_next_run_reaps() {
        use std::os::unix::process::CommandExt;
        let tmp = tempfile::tempdir().unwrap();
        let (base, run) = (tmp.path().join("scratch"), tmp.path().join("scratch/run1"));
        fs::create_dir_all(&run).unwrap();
        let mut c = std::process::Command::new("sleep").arg("120").process_group(0).spawn().unwrap();
        let pg = c.id() as i32;
        fs::write(run.join("server.log.pgid"), pg.to_string()).unwrap();
        assert!(group_alive(pg));
        reap_dir(&base);
        c.wait().unwrap(); // reaps the killed leader, so the group id empties
        assert!(!group_alive(pg) && !run.exists(), "the group is gone and so is the folder");
    }
}
