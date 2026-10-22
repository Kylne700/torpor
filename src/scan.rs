//! Walk the project (spec section 5): hard excludes, ignore files, the precious pass and the secret rules.
//! The walk only lists and stats; hashing and storing happen in `capture`.
use crate::util::*;
use ignore::{WalkBuilder, WalkState};
use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Entries one scan will list before it stops and the point is marked partial.
pub const ENTRY_CAP: usize = 200_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EKind {
    File,
    Link,
    Dir,
    Special,
}

#[derive(Clone, Debug)]
pub struct FEntry {
    /// Path relative to the root, `/`-separated. Lossy when the name is not UTF-8 (then `reason` is `name`).
    pub path: String,
    pub kind: EKind,
    pub size: u64,
    /// Permission bits of st_mode.
    pub mode: u32,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: u64,
    pub precious: bool,
    pub secret: bool,
    /// A not-saved reason the scan already knows (`name`, `special`, `unreadable`).
    pub reason: Option<&'static str>,
}

impl FEntry {
    pub fn name(&self) -> &str {
        match self.path.rfind('/') {
            Some(i) => &self.path[i + 1..],
            None => &self.path,
        }
    }
    pub fn parent(&self) -> &str {
        match self.path.rfind('/') {
            Some(i) => &self.path[..i],
            None => "",
        }
    }
}

#[derive(Debug, Default)]
pub struct Scan {
    pub entries: Vec<FEntry>,
    /// The entry cap stopped the walk early.
    pub cap_hit: bool,
    /// Directories that could not be read (relative paths; "" is the root itself).
    pub unreadable: Vec<String>,
    pub elapsed_ms: u64,
}

impl Scan {
    pub fn file_count(&self) -> usize {
        self.entries.iter().filter(|e| e.kind != EKind::Dir).count()
    }
}

// ---------------------------------------------------------------- name rules

fn ends_ci(n: &[u8], suf: &[u8]) -> bool {
    n.len() >= suf.len() && n[n.len() - suf.len()..].eq_ignore_ascii_case(suf)
}

/// .env, .env.*, *.db, *.sqlite, *.sqlite3, *.sql: saved even when git ignores them.
pub fn is_precious_name(name: &str) -> bool {
    let n = name.as_bytes();
    n.eq_ignore_ascii_case(b".env")
        || (n.len() > 5 && n[..5].eq_ignore_ascii_case(b".env."))
        || ends_ci(n, b".db")
        || ends_ci(n, b".sqlite")
        || ends_ci(n, b".sqlite3")
        || ends_ci(n, b".sql")
}

/// Names that may be SQLite files (the magic number is checked when the file is read).
pub fn is_db_name(name: &str) -> bool {
    let n = name.as_bytes();
    ends_ci(n, b".db") || ends_ci(n, b".sqlite") || ends_ci(n, b".sqlite3")
}

/// Path rules for files that are stored encrypted (spec 5.4).
pub fn is_secret(rel: &str) -> bool {
    let (dir, base) = match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => ("", rel),
    };
    let l = base.to_ascii_lowercase();
    if l == ".env" || l.starts_with(".env.") {
        return !(l.ends_with(".example") || l.ends_with(".sample") || l.ends_with(".template"));
    }
    if l.ends_with(".pem")
        || l.ends_with(".key")
        || l.ends_with(".p12")
        || l.starts_with("id_")
        || l == ".npmrc"
        || l == ".netrc"
        || l == ".pypirc"
        || l.starts_with("credentials")
        || l.contains(".tfstate")
        || l.ends_with(".kdbx")
        || (l.starts_with("service-account") && l.ends_with(".json"))
    {
        return true;
    }
    if !dir.is_empty() {
        let mut prev = "";
        for c in dir.split('/') {
            match c {
                ".aws" | ".ssh" | ".gnupg" | ".kube" => return true,
                "gcloud" if prev == ".config" => return true,
                _ => {}
            }
            prev = c;
        }
        if prev == ".docker" && l == "config.json" {
            return true;
        }
    }
    false
}

const EXCLUDED_DIRS: &[&[u8]] = &[
    b"node_modules",
    b".next",
    b".nuxt",
    b".svelte-kit",
    b".output",
    b".vercel",
    b".turbo",
    b".parcel-cache",
    b".cache",
    b"dist",
    b"build",
    b"__pycache__",
    b".pytest_cache",
    b".mypy_cache",
    b".gradle",
    b"coverage",
    b".terraform",
    b"Pods",
    b"DerivedData",
    b".dart_tool",
    b".torpor-stage",
];

/// Folders and files that are never saved (spec 5.3), whatever the ignore files say.
pub fn hard_excluded(e: &ignore::DirEntry) -> bool {
    let n = e.file_name().as_bytes();
    if n == b".git" {
        return true; // a folder, or the pointer file of a worktree or submodule
    }
    let is_dir = e.file_type().is_some_and(|t| t.is_dir());
    if is_dir {
        if EXCLUDED_DIRS.contains(&n) {
            return true;
        }
        if n == b"target" {
            return e.path().parent().is_some_and(|p| p.join("Cargo.toml").exists());
        }
        if n == b"venv" || n == b".venv" {
            return e.path().join("pyvenv.cfg").exists();
        }
        return false;
    }
    (n.starts_with(b".torpor-") && n.ends_with(b".tmp")) || n.ends_with(b"-wal") || n.ends_with(b"-shm") || n.ends_with(b"-journal")
}

/// Same rules as `hard_excluded`, for one root-relative path named by an event (no walk).
pub fn rel_hard_excluded(root: &Path, rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let mut dir = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        let n = part.as_bytes();
        let last = i + 1 == parts.len();
        if n == b".git" {
            return true;
        }
        if !last {
            if EXCLUDED_DIRS.contains(&n) {
                return true;
            }
            if (n == b"target" && dir.join("Cargo.toml").exists()) || ((n == b"venv" || n == b".venv") && dir.join(part).join("pyvenv.cfg").exists()) {
                return true;
            }
        } else if (n.starts_with(b".torpor-") && n.ends_with(b".tmp")) || n.ends_with(b"-wal") || n.ends_with(b"-shm") || n.ends_with(b"-journal") {
            return true;
        }
        dir.push(part);
    }
    false
}

// ---------------------------------------------------------------- the walk

#[derive(Default)]
struct Acc {
    entries: Vec<FEntry>,
    extras: Vec<FEntry>,
    walked_precious: Vec<String>,
    unreadable: Vec<String>,
}

struct Visitor {
    root: PathBuf,
    cap: usize,
    counter: Arc<AtomicUsize>,
    cap_hit: Arc<AtomicBool>,
    shared: Arc<Mutex<Vec<Acc>>>,
    /// Lines of the top-level .torporignore: they also drop precious files (.env, *.db ...) that the other ignore files cannot.
    optout: Arc<ignore::gitignore::Gitignore>,
    acc: Acc,
}

impl Drop for Visitor {
    fn drop(&mut self) {
        let acc = std::mem::take(&mut self.acc);
        if let Ok(mut g) = self.shared.lock() {
            g.push(acc);
        }
    }
}

fn unwrap_err(e: &ignore::Error) -> (Option<PathBuf>, Option<std::io::ErrorKind>) {
    match e {
        ignore::Error::WithPath { path, err } => {
            let (_, k) = unwrap_err(err);
            (Some(path.clone()), k)
        }
        ignore::Error::WithDepth { err, .. } => unwrap_err(err),
        ignore::Error::Io(io) => (None, Some(io.kind())),
        _ => (None, None),
    }
}

fn nanos(sec: i64, nsec: i64) -> i64 {
    sec.saturating_mul(NS_PER_SEC).saturating_add(nsec)
}

fn entry_from_md(rel: String, md: &std::fs::Metadata, reason: Option<&'static str>) -> FEntry {
    let ft = md.file_type();
    let kind = if ft.is_dir() {
        EKind::Dir
    } else if ft.is_symlink() {
        EKind::Link
    } else if ft.is_file() {
        EKind::File
    } else {
        EKind::Special
    };
    let name_start = rel.rfind('/').map_or(0, |i| i + 1);
    let precious = kind == EKind::File && is_precious_name(&rel[name_start..]);
    let secret = kind == EKind::File && is_secret(&rel);
    let reason = if kind == EKind::Special && reason.is_none() { Some("special") } else { reason };
    FEntry {
        path: rel,
        kind,
        size: if kind == EKind::File || kind == EKind::Link { md.len() } else { 0 },
        mode: md.mode() & 0o7777,
        mtime_ns: nanos(md.mtime(), md.mtime_nsec()),
        ctime_ns: nanos(md.ctime(), md.ctime_nsec()),
        ino: md.ino(),
        precious,
        secret,
        reason,
    }
}

impl Visitor {
    fn rel_of(&self, p: &Path) -> Option<(String, bool)> {
        let rel = p.strip_prefix(&self.root).ok()?;
        match rel.to_str() {
            Some(s) => Some((s.to_string(), true)),
            None => Some((String::from_utf8_lossy(rel.as_os_str().as_bytes()).into_owned(), false)),
        }
    }

    /// The precious pass: list one directory again and keep the precious files the ignore rules hid.
    fn precious_pass(&mut self, dir: &Path, dir_rel: &str) {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return,
        };
        for de in rd.flatten() {
            let name = de.file_name();
            let n = match name.to_str() {
                Some(n) => n,
                None => continue,
            };
            if !is_precious_name(n) {
                continue;
            }
            // Files only (a symlink or folder with such a name is left to the normal walk).
            if !de.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let rel = if dir_rel.is_empty() { n.to_string() } else { format!("{dir_rel}/{n}") };
            if self.optout.matched_path_or_any_parents(self.root.join(&rel), false).is_ignore() {
                continue;
            }
            if let Ok(md) = de.metadata() {
                self.acc.extras.push(entry_from_md(rel, &md, None));
            }
        }
    }

    fn visit(&mut self, res: std::result::Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        let ent = match res {
            Ok(e) => e,
            Err(err) => {
                let (path, kind) = unwrap_err(&err);
                if let (Some(p), Some(k)) = (path, kind) {
                    if k != std::io::ErrorKind::NotFound {
                        if let Some((rel, _)) = self.rel_of(&p) {
                            self.acc.unreadable.push(rel);
                        }
                    }
                }
                return WalkState::Continue;
            }
        };
        let ft = match ent.file_type() {
            Some(t) => t,
            None => return WalkState::Continue,
        };
        if ent.depth() == 0 {
            let root = self.root.clone();
            self.precious_pass(&root, "");
            return WalkState::Continue;
        }
        if self.counter.fetch_add(1, Ordering::Relaxed) >= self.cap {
            self.cap_hit.store(true, Ordering::Relaxed);
            return WalkState::Quit;
        }
        let (rel, utf8) = match self.rel_of(ent.path()) {
            Some(r) => r,
            None => return WalkState::Continue,
        };
        let md = match ent.metadata() {
            Ok(m) => m,
            Err(e) => {
                // Vanished between listing and stat: not an entry. Anything else: keep it as not saved.
                let gone = e.io_error().is_some_and(|i| i.kind() == std::io::ErrorKind::NotFound);
                if gone {
                    return WalkState::Continue;
                }
                let mut fe = FEntry { path: rel, kind: if ft.is_dir() { EKind::Dir } else { EKind::File }, size: 0, mode: 0o644, mtime_ns: 0, ctime_ns: 0, ino: 0, precious: false, secret: false, reason: Some("unreadable") };
                if !utf8 {
                    fe.reason = Some("name");
                }
                self.acc.entries.push(fe);
                return if ft.is_dir() { WalkState::Skip } else { WalkState::Continue };
            }
        };
        let fe = entry_from_md(rel, &md, if utf8 { None } else { Some("name") });
        let is_dir = fe.kind == EKind::Dir;
        if fe.precious && fe.reason.is_none() {
            self.acc.walked_precious.push(fe.path.clone());
        }
        let skip_children = is_dir && !utf8;
        if is_dir && utf8 {
            let (p, r) = (ent.path().to_path_buf(), fe.path.clone());
            self.acc.entries.push(fe);
            self.precious_pass(&p, &r);
            return WalkState::Continue;
        }
        self.acc.entries.push(fe);
        if skip_children {
            WalkState::Skip
        } else {
            WalkState::Continue
        }
    }
}

/// Threads for the walk: one for small trees, more past 5,000 files (spec 5.1).
pub fn thread_count(last_file_count: usize) -> usize {
    if last_file_count > 5000 {
        std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 4)
    } else {
        1
    }
}

pub fn scan(root: &Path, threads: usize, entry_cap: usize) -> Result<Scan> {
    let t0 = Instant::now();
    if !root.is_dir() {
        return Err(Error::NotFound("project folder".into()));
    }
    let mut b = WalkBuilder::new(root);
    b.hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .same_file_system(true)
        .threads(threads.max(1))
        .add_custom_ignore_filename(".torporignore")
        .filter_entry(|e| !hard_excluded(e));
    let shared: Arc<Mutex<Vec<Acc>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = Arc::new(AtomicUsize::new(0));
    let cap_hit = Arc::new(AtomicBool::new(false));
    // shortcut: only the top-level .torporignore can opt a precious file out; nested ones cannot, add them if somebody asks.
    let mut ob = ignore::gitignore::GitignoreBuilder::new(root);
    let _ = ob.add(root.join(".torporignore"));
    let optout = Arc::new(ob.build().unwrap_or_else(|_| ignore::gitignore::Gitignore::empty()));
    b.build_parallel().run(|| {
        let mut v = Visitor {
            root: root.to_path_buf(),
            cap: entry_cap,
            counter: counter.clone(),
            cap_hit: cap_hit.clone(),
            shared: shared.clone(),
            optout: optout.clone(),
            acc: Acc::default(),
        };
        Box::new(move |res| v.visit(res))
    });
    let mut accs = std::mem::take(&mut *shared.lock().map_err(|_| Error::msg("scan lock"))?);
    let walked: HashSet<String> = accs.iter_mut().flat_map(|a| std::mem::take(&mut a.walked_precious)).collect();
    let mut out = Scan { cap_hit: cap_hit.load(Ordering::Relaxed), ..Default::default() };
    out.entries.reserve(accs.iter().map(|a| a.entries.len() + a.extras.len()).sum());
    let mut unreadable: Vec<String> = Vec::new();
    for a in accs.iter_mut() {
        out.entries.append(&mut a.entries);
        // A precious file the walker already listed is not an extra.
        out.entries.extend(std::mem::take(&mut a.extras).into_iter().filter(|x| !walked.contains(&x.path)));
        unreadable.append(&mut a.unreadable);
    }
    unreadable.sort();
    unreadable.dedup();
    out.unreadable = unreadable;
    out.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(out)
}

// ---------------------------------------------------------------- deep check (spec 5.3)

/// Precious files that a walk with the ignore files switched off finds but the normal scan did not.
/// Returns (file, shallowest skipped folder). Stops after `budget`.
pub fn deep_check(root: &Path, normal: &[FEntry], budget: Duration) -> Vec<(String, String)> {
    let t0 = Instant::now();
    let have: HashSet<&str> = normal.iter().map(|e| e.path.as_str()).collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut b = WalkBuilder::new(root);
    b.standard_filters(false).hidden(false).follow_links(false).same_file_system(true).filter_entry(|e| !hard_excluded(e));
    let mut seen = 0usize;
    for res in b.build() {
        seen += 1;
        if seen > ENTRY_CAP || t0.elapsed() > budget || out.len() >= 20 {
            break;
        }
        let ent = match res {
            Ok(e) => e,
            Err(_) => continue,
        };
        if ent.depth() == 0 || !ent.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = match ent.path().strip_prefix(root).ok().and_then(|p| p.to_str()) {
            Some(r) => r.to_string(),
            None => continue,
        };
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        if !is_precious_name(name) || have.contains(rel.as_str()) {
            continue;
        }
        // Shallowest folder the normal scan did not list.
        let mut folder = String::new();
        let mut acc = String::new();
        let parts: Vec<&str> = rel.split('/').collect();
        for p in &parts[..parts.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(p);
            if !have.contains(acc.as_str()) {
                folder = format!("{acc}/");
                break;
            }
        }
        if !folder.is_empty() {
            out.push((rel, folder));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn paths(s: &Scan) -> Vec<String> {
        let mut v: Vec<String> = s.entries.iter().map(|e| e.path.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn name_rules() {
        assert!(is_precious_name(".env") && is_precious_name(".env.local") && is_precious_name("a.SQLITE3") && is_precious_name("x.sql"));
        assert!(!is_precious_name("environment.ts") && !is_precious_name("db.rs"));
        assert!(is_secret(".env") && is_secret("a/.env.production") && !is_secret(".env.example") && !is_secret("x/.env.sample"));
        assert!(is_secret("k/server.pem") && is_secret("id_ed25519") && is_secret("a/.npmrc") && is_secret("terraform.tfstate.backup"));
        assert!(is_secret("service-account-prod.json") && !is_secret("service.json") && is_secret("deep/.aws/config") && is_secret("h/.config/gcloud/x"));
        assert!(is_secret(".docker/config.json") && !is_secret(".docker/other.json") && !is_secret("config/gcloud/x"));
        assert!(!is_secret("src/main.rs"));
    }

    #[test]
    fn walk_excludes_and_precious_pass() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        w(r, "src/a.ts", "a");
        w(r, "node_modules/x/i.js", "x");
        w(r, ".git/HEAD", "ref");
        w(r, "dist/o.js", "o");
        w(r, "target/debug/b", "b"); // no Cargo.toml: kept
        w(r, "rust/Cargo.toml", "[package]");
        w(r, "rust/target/debug/b", "b"); // Cargo.toml beside it: excluded
        w(r, ".venv/pyvenv.cfg", "x");
        w(r, ".venv/lib/z.py", "z");
        w(r, "tool/venv/lib/y.py", "y"); // venv without pyvenv.cfg: kept
        w(r, "app.db-wal", "w");
        w(r, "app.db", "SQLite format 3\0");
        w(r, "notes.txt-journal", "j");
        w(r, ".gitignore", "data/\n*.log\n.env\n");
        w(r, "data/app.sqlite", "d");
        w(r, "data/other.txt", "o");
        w(r, "x.log", "l");
        w(r, ".env", "SECRET=1");
        w(r, "sub/.env.local", "S=2");
        let s = scan(r, 2, ENTRY_CAP).unwrap();
        let p = paths(&s);
        for want in ["src", "src/a.ts", "target/debug/b", "rust/Cargo.toml", "tool/venv/lib/y.py", "app.db", ".gitignore", ".env", "sub/.env.local"] {
            assert!(p.contains(&want.to_string()), "missing {want}: {p:?}");
        }
        for no in ["node_modules", ".git", "dist", "rust/target", ".venv", "app.db-wal", "notes.txt-journal", "data/other.txt", "x.log", "data/app.sqlite"] {
            assert!(!p.iter().any(|x| x == no || x.starts_with(&format!("{no}/"))), "unexpected {no}: {p:?}");
        }
        let env = s.entries.iter().find(|e| e.path == ".env").unwrap();
        assert!(env.precious && env.secret);
        // The deep check finds data/app.sqlite inside the ignored folder.
        let dc = deep_check(r, &s.entries, Duration::from_secs(1));
        assert_eq!(dc, vec![("data/app.sqlite".to_string(), "data/".to_string())]);
    }

    #[test]
    fn torporignore_and_empty_dirs_and_cap() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        w(r, ".torporignore", "big/\n");
        w(r, "big/x", "x");
        w(r, "keep/y", "y");
        std::fs::create_dir_all(r.join("empty")).unwrap();
        let s = scan(r, 1, ENTRY_CAP).unwrap();
        let p = paths(&s);
        assert!(p.contains(&"empty".to_string()) && p.contains(&"keep/y".to_string()) && !p.iter().any(|x| x.starts_with("big")));
        for i in 0..50 {
            w(r, &format!("many/f{i}"), "z");
        }
        let c = scan(r, 1, 20).unwrap();
        assert!(c.cap_hit && c.entries.len() <= 20);
    }

    #[test]
    fn torporignore_can_opt_a_precious_file_out_but_only_from_the_top() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        w(r, ".torporignore", "dev.db\n*.sql\n!keep.sql\n");
        for f in [".env", "dev.db", "dump.sql", "keep.sql", "data/app.sqlite", "data/dump.sql", "sub/x.db"] {
            w(r, f, "x");
        }
        w(r, "sub/.torporignore", "x.db\n");
        let p = paths(&scan(r, 1, ENTRY_CAP).unwrap());
        assert!(p.contains(&".env".to_string()) && p.contains(&"data/app.sqlite".to_string()) && p.contains(&"keep.sql".to_string()));
        assert!(!p.contains(&"dev.db".to_string()) && !p.contains(&"dump.sql".to_string()) && !p.contains(&"data/dump.sql".to_string()));
        assert!(p.contains(&"sub/x.db".to_string()), "a nested .torporignore does not opt precious files out");
    }

    #[test]
    fn non_utf8_and_special() {
        use std::os::unix::ffi::OsStrExt;
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let bad = std::ffi::OsStr::from_bytes(b"bad\xffname");
        std::fs::write(r.join(bad), "x").unwrap();
        std::fs::create_dir(r.join(std::ffi::OsStr::from_bytes(b"dir\xfe"))).unwrap();
        std::fs::write(r.join(std::ffi::OsStr::from_bytes(b"dir\xfe")).join("in"), "x").unwrap();
        let fifo = std::ffi::CString::new(r.join("pipe").as_os_str().as_bytes()).unwrap();
        unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
        std::os::unix::fs::symlink("nowhere", r.join("lnk")).unwrap();
        let s = scan(r, 1, ENTRY_CAP).unwrap();
        let by = |n: &str| s.entries.iter().find(|e| e.path.starts_with(n)).unwrap();
        assert_eq!(by("bad").reason, Some("name"));
        assert_eq!(by("dir").reason, Some("name"));
        assert!(!s.entries.iter().any(|e| e.path.ends_with("/in")));
        assert_eq!(by("pipe").reason, Some("special"));
        assert_eq!(by("lnk").kind, EKind::Link);
    }
}
