//! One capture: scan, hash and store changed files, build trees, diff against the last point,
//! classify every change, write the rows (spec sections 5 and 5.1).
//! The caller holds the shared workspace lock and an open IMMEDIATE transaction; this module never commits.
use crate::scan::{self, EKind, FEntry};
use crate::store::*;
use crate::util::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const NORMAL_CAP: u64 = 2 << 20;
pub const PRECIOUS_CAP: u64 = 64 << 20;
pub const SECRET_CAP: u64 = 1 << 20;
pub const NEW_BYTES_CAP: u64 = 100 << 20;
/// A stat-cache row is trusted only when the file was last modified more than this before the row was written.
pub const RACY_NS: i64 = 2 * NS_PER_SEC;
const STALE_TURN_NS: i64 = 30 * 60 * NS_PER_SEC;
const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
const TRANSIENT: [&str; 4] = ["cap", "unstable", "db_busy", "unreadable"];
const NOT_CAPTURED_ROWS_MAX: usize = 10_000;

// ---------------------------------------------------------------- public types

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointKind {
    Start,
    End,
    Tool,
    PreBash,
    PreRestore,
    Restored,
}

impl PointKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PointKind::Start => "start",
            PointKind::End => "end",
            PointKind::Tool => "tool",
            PointKind::PreBash => "pre_bash",
            PointKind::PreRestore => "pre_restore",
            PointKind::Restored => "restored",
        }
    }
    /// `tool` points are internal; everything else is a save point the user sees.
    pub fn visible(self) -> bool {
        self != PointKind::Tool
    }
}

#[derive(Clone, Debug)]
pub struct PointSpec {
    pub kind: PointKind,
    pub turn_id: Option<i64>,
    pub note: Option<String>,
}

pub struct Req<'a> {
    /// Session that runs this scan.
    pub session: &'a str,
    /// Points to write, in order. All share the new tree; the first carries the changes.
    pub points: Vec<PointSpec>,
    /// Turn this scan belongs to for attribution; None means between turns.
    pub turn: Option<i64>,
    /// Write no point when nothing changed (tool points).
    pub only_if_changed: bool,
    /// Soft time limit for hashing; files left over become `x:cap` and the point is partial.
    pub budget: Option<Duration>,
}

#[derive(Debug, Default, Clone)]
pub struct Captured {
    pub point_ids: Vec<i64>,
    pub tree_id: [u8; 32],
    /// The tree differs from the previous point's tree (true for the first point).
    pub changed: bool,
    pub n_changes: usize,
    pub partial: bool,
    pub not_saved: usize,
    pub scan_ms: u64,
    pub file_count: usize,
}

#[derive(Clone, Debug)]
pub struct Leaf {
    pub path: String,
    pub e: TreeEntry,
}

#[derive(Clone, Debug)]
pub struct RawChange {
    pub path: String,
    pub old: Option<TreeEntry>,
    pub new: Option<TreeEntry>,
}

#[derive(Clone, Debug)]
pub struct ChangeRow {
    pub path: String,
    pub old: Option<TreeEntry>,
    pub new: Option<TreeEntry>,
    pub class: &'static str,
    pub turn_id: Option<i64>,
    pub tool_use_id: Option<String>,
    pub agent_id: Option<String>,
}

pub fn id_text(e: &TreeEntry) -> String {
    if e.kind == 'x' {
        format!("x:{}", e.id)
    } else {
        e.id.clone()
    }
}

fn name_of(path: &str) -> &str {
    path.rfind('/').map_or(path, |i| &path[i + 1..])
}
fn parent_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn exec_mode(mode: u32) -> u32 {
    if mode & 0o111 != 0 {
        755
    } else {
        644
    }
}

pub fn x_leaf(path: &str, reason: &str, size: u64, mode: u32) -> Leaf {
    Leaf { path: path.to_string(), e: TreeEntry { name: name_of(path).to_string(), kind: 'x', mode: exec_mode(mode), id: reason.to_string(), size } }
}

// ---------------------------------------------------------------- mode (spec 5.10)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Fast,
    Slow,
}

pub fn mode(db: &Connection) -> Mode {
    #[cfg(feature = "fault-inject")]
    match std::env::var("TORPOR_TEST_MODE").as_deref() {
        Ok("slow") => return Mode::Slow,
        Ok("fast") => return Mode::Fast,
        _ => {}
    }
    match meta_get(db, "mode").as_deref() {
        Some("slow") => Mode::Slow,
        _ => Mode::Fast,
    }
}

/// Record a scan time and file count; flips the mode on the first timing and when the count moves by over 50%.
pub fn update_mode(db: &Connection, scan_ms: u64, file_count: usize) -> Result<()> {
    let old = meta_get_i64(db, "file_count");
    let retime = match old {
        None => true,
        Some(c) => {
            let c = c.max(1) as f64;
            ((file_count as f64 - c).abs() / c) > 0.5
        }
    };
    if retime || meta_get(db, "mode").is_none() {
        meta_set(db, "scan_ms", &scan_ms.to_string())?;
        meta_set(db, "file_count", &file_count.to_string())?;
        meta_set(db, "mode", if scan_ms <= 40 { "fast" } else { "slow" })?;
    }
    Ok(())
}

// ---------------------------------------------------------------- stat cache

#[derive(Clone, Debug)]
pub struct CacheRow {
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: i64,
    pub mode: i64,
    pub blob_id: [u8; 32],
    pub kind: String,
    pub written_ns: i64,
}

fn load_cache(db: &Connection) -> Result<HashMap<String, CacheRow>> {
    let mut st = db.prepare_cached("SELECT path,size,mtime_ns,ctime_ns,ino,mode,blob_id,kind,written_ns FROM stat_cache")?;
    let mut rows = st.query([])?;
    let mut m: HashMap<String, CacheRow> = HashMap::new();
    while let Some(r) = rows.next()? {
        let blob: Vec<u8> = r.get(6)?;
        let blob_id = match blob_id_from_vec(&blob) {
            Some(b) => b,
            None => continue,
        };
        m.insert(
            r.get(0)?,
            CacheRow { size: r.get(1)?, mtime_ns: r.get(2)?, ctime_ns: r.get(3)?, ino: r.get(4)?, mode: r.get(5)?, blob_id, kind: r.get(7)?, written_ns: r.get(8)? },
        );
    }
    Ok(m)
}

fn cache_trusted(row: &CacheRow, e: &FEntry, want: &str) -> bool {
    row.size == e.size as i64
        && row.mtime_ns == e.mtime_ns
        && row.ctime_ns == e.ctime_ns
        && row.ino == e.ino as i64
        && row.mode == e.mode as i64
        && row.kind == want
        && row.mtime_ns.saturating_add(RACY_NS) < row.written_ns
}

fn upsert_cache(db: &Connection, path: &str, c: &CacheRow) -> Result<()> {
    db.prepare_cached("INSERT OR REPLACE INTO stat_cache(path,size,mtime_ns,ctime_ns,ino,mode,blob_id,kind,written_ns) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?
        .execute(params![path, c.size, c.mtime_ns, c.ctime_ns, c.ino, c.mode, &c.blob_id[..], c.kind, c.written_ns])?;
    Ok(())
}

// ---------------------------------------------------------------- hashing one file

struct StatSnap {
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    ino: u64,
    mode: u32,
}

impl StatSnap {
    fn of(md: &std::fs::Metadata) -> StatSnap {
        StatSnap {
            size: md.len(),
            mtime_ns: md.mtime().saturating_mul(NS_PER_SEC).saturating_add(md.mtime_nsec()),
            ctime_ns: md.ctime().saturating_mul(NS_PER_SEC).saturating_add(md.ctime_nsec()),
            ino: md.ino(),
            mode: md.mode() & 0o7777,
        }
    }
    fn same_content_stamp(&self, o: &StatSnap) -> bool {
        self.size == o.size && self.mtime_ns == o.mtime_ns && self.ctime_ns == o.ctime_ns
    }
}

enum Out {
    Blob { prep: Prepared, st: StatSnap, kind: char },
    X(&'static str, u64),
    Gone,
}

struct HashCtx<'a> {
    root: &'a Path,
    key: Option<&'a Key>,
    deadline: Option<Instant>,
    new_bytes: &'a AtomicU64,
}

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// open + fstat + read + fstat. Non-blocking open so a FIFO swapped in under us cannot hang the hook.
fn read_once(path: &Path, cap: u64) -> std::io::Result<(Vec<u8>, StatSnap, StatSnap, bool)> {
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let before = f.metadata()?;
    if !before.is_file() {
        return Ok((Vec::new(), StatSnap::of(&before), StatSnap::of(&before), false));
    }
    let mut buf = Vec::with_capacity((before.len().min(cap + 1)) as usize);
    f.by_ref().take(cap + 1).read_to_end(&mut buf)?;
    let after = f.metadata()?;
    Ok((buf, StatSnap::of(&before), StatSnap::of(&after), true))
}

fn db_is_busy(path: &Path) -> bool {
    let mut magic = [0u8; 16];
    let ok = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok();
    if !ok || &magic != SQLITE_MAGIC {
        return false;
    }
    let side = |suffix: &str| {
        let mut s = path.as_os_str().to_os_string();
        s.push(suffix);
        std::path::PathBuf::from(s)
    };
    let wal_live = std::fs::metadata(side("-wal")).is_ok_and(|m| m.len() > 0);
    wal_live || side("-journal").exists()
}

fn cap_for(e: &FEntry) -> u64 {
    if e.secret {
        SECRET_CAP
    } else if e.precious {
        PRECIOUS_CAP
    } else {
        NORMAL_CAP
    }
}

fn hash_one(ctx: &HashCtx, e: &FEntry) -> Out {
    if ctx.deadline.is_some_and(|d| Instant::now() > d) {
        return Out::X("cap", e.size);
    }
    if ctx.new_bytes.fetch_add(e.size, Ordering::Relaxed) >= NEW_BYTES_CAP {
        return Out::X("cap", e.size);
    }
    let abs = ctx.root.join(&e.path);
    if e.kind == EKind::Link {
        return match std::fs::read_link(&abs) {
            Ok(t) => match prepare_plain("link", t.as_os_str().as_bytes()) {
                Ok(prep) => Out::Blob { prep, st: StatSnap { size: e.size, mtime_ns: e.mtime_ns, ctime_ns: e.ctime_ns, ino: e.ino, mode: e.mode }, kind: 'l' },
                Err(_) => Out::X("unreadable", e.size),
            },
            Err(x) if x.kind() == std::io::ErrorKind::NotFound => Out::Gone,
            Err(_) => Out::X("unreadable", e.size),
        };
    }
    let cap = cap_for(e);
    if scan::is_db_name(e.name()) && db_is_busy(&abs) {
        return Out::X("db_busy", e.size);
    }
    for _ in 0..2 {
        let (bytes, before, after, regular) = match read_once(&abs, cap) {
            Ok(t) => t,
            Err(x) if x.kind() == std::io::ErrorKind::NotFound => return Out::Gone,
            Err(_) => return Out::X("unreadable", e.size),
        };
        if !regular {
            return Out::X("special", 0);
        }
        if bytes.len() as u64 > cap {
            return Out::X("too_big", after.size);
        }
        if before.same_content_stamp(&after) && bytes.len() as u64 == after.size {
            let size = after.size;
            let made = if e.secret {
                match ctx.key {
                    Some(k) => prepare_secret(&bytes, k),
                    None => return Out::X("unreadable", size),
                }
            } else {
                prepare_plain("file", &bytes)
            };
            return match made {
                Ok(prep) => Out::Blob { prep, st: after, kind: if e.secret { 's' } else { 'f' } },
                Err(_) => Out::X("unreadable", size),
            };
        }
    }
    Out::X("unstable", e.size)
}

/// Hash many files, in parallel when there are enough of them.
fn hash_all(ctx: &HashCtx, entries: &[&FEntry]) -> Vec<Out> {
    let n = entries.len();
    let threads = if n < 16 { 1 } else { std::thread::available_parallelism().map_or(2, |c| c.get()).clamp(1, 8) };
    if threads == 1 {
        return entries.iter().map(|e| hash_one(ctx, e)).collect();
    }
    // Big files go last and alone so that memory stays bounded.
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Out)>> = Mutex::new(Vec::with_capacity(n));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut mine: Vec<(usize, Out)> = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    mine.push((i, hash_one(ctx, entries[i])));
                }
                if let Ok(mut g) = results.lock() {
                    g.append(&mut mine);
                }
            });
        }
    });
    let mut all = results.into_inner().unwrap_or_default();
    all.sort_by_key(|(i, _)| *i);
    all.into_iter().map(|(_, o)| o).collect()
}

// ---------------------------------------------------------------- trees

pub struct Trees {
    pub dirs: HashMap<String, Vec<TreeEntry>>,
    pub ids: HashMap<String, [u8; 32]>,
    pub raw: HashMap<String, Vec<u8>>,
    pub root: [u8; 32],
}

fn depth(dir: &str) -> usize {
    if dir.is_empty() {
        0
    } else {
        dir.bytes().filter(|b| *b == b'/').count() + 1
    }
}

/// Build every directory's tree bottom-up from a flat list of leaves (`d` leaves only make sure a folder exists).
pub fn build_trees(leaves: Vec<Leaf>) -> Trees {
    let mut map: HashMap<String, Vec<TreeEntry>> = HashMap::new();
    map.entry(String::new()).or_default();
    for l in leaves {
        if l.e.kind == 'd' {
            map.entry(l.path).or_default();
        } else {
            let parent = parent_of(&l.path);
            // Look up first: most leaves land in a folder that exists, and `to_string` per leaf costs.
            match map.get_mut(parent) {
                Some(v) => v.push(l.e),
                None => {
                    map.insert(parent.to_string(), vec![l.e]);
                }
            }
        }
    }
    // Every ancestor folder must exist.
    let keys: Vec<String> = map.keys().cloned().collect();
    for k in keys {
        let mut cur = parent_of(&k).to_string();
        while !cur.is_empty() && !map.contains_key(&cur) {
            map.insert(cur.clone(), Vec::new());
            cur = parent_of(&cur).to_string();
        }
    }
    let mut order: Vec<String> = map.keys().cloned().collect();
    order.sort_by_key(|k| std::cmp::Reverse(depth(k)));
    let mut out = Trees { dirs: HashMap::new(), ids: HashMap::new(), raw: HashMap::new(), root: [0; 32] };
    for k in order {
        let mut v = map.remove(&k).unwrap_or_default();
        v.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        v.dedup_by(|a, b| a.name == b.name);
        let raw = encode_tree(&v);
        let id = *blake3::hash(&raw).as_bytes();
        if !k.is_empty() {
            map.entry(parent_of(&k).to_string()).or_default().push(TreeEntry { name: name_of(&k).to_string(), kind: 'd', mode: 755, id: hex(&id), size: 0 });
        } else {
            out.root = id;
        }
        out.ids.insert(k.clone(), id);
        out.raw.insert(k.clone(), raw);
        out.dirs.insert(k, v);
    }
    out
}

fn load_cached(ws: &Ws, db: &Connection, cache: &mut HashMap<[u8; 32], std::rc::Rc<Vec<TreeEntry>>>, id: &[u8; 32]) -> Result<std::rc::Rc<Vec<TreeEntry>>> {
    if let Some(t) = cache.get(id) {
        return Ok(t.clone());
    }
    let t = std::rc::Rc::new(ws.load_tree(db, id)?);
    cache.insert(*id, t.clone());
    Ok(t)
}

type TreeCache = HashMap<[u8; 32], std::rc::Rc<Vec<TreeEntry>>>;

fn entry_eq(a: &TreeEntry, b: &TreeEntry) -> bool {
    a.kind == b.kind && a.id == b.id && a.mode == b.mode && a.size == b.size
}

struct DiffOut {
    changes: Vec<RawChange>,
    /// Folders whose tree differs from the old one, and so must be stored.
    dirty: Vec<String>,
}

fn diff_dir(ws: &Ws, db: &Connection, tc: &mut TreeCache, old: Option<[u8; 32]>, dir: &str, new: &Trees, out: &mut DiffOut) -> Result<()> {
    let new_id = new.ids.get(dir).copied().unwrap_or([0; 32]);
    if old == Some(new_id) {
        return Ok(());
    }
    out.dirty.push(dir.to_string());
    let old_entries = match old {
        Some(id) => load_cached(ws, db, tc, &id)?,
        None => std::rc::Rc::new(Vec::new()),
    };
    let empty: Vec<TreeEntry> = Vec::new();
    let new_entries = new.dirs.get(dir).unwrap_or(&empty);
    let (mut i, mut j) = (0usize, 0usize);
    while i < old_entries.len() || j < new_entries.len() {
        let ord = match (old_entries.get(i), new_entries.get(j)) {
            (Some(o), Some(n)) => o.name.as_bytes().cmp(n.name.as_bytes()),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => break,
        };
        match ord {
            std::cmp::Ordering::Less => {
                let o = &old_entries[i];
                removed(ws, db, tc, &join(dir, &o.name), o, out)?;
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                let n = &new_entries[j];
                added(ws, db, tc, &join(dir, &n.name), n, new, out)?;
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let (o, n) = (&old_entries[i], &new_entries[j]);
                let p = join(dir, &o.name);
                match (o.kind == 'd', n.kind == 'd') {
                    (true, true) => diff_dir(ws, db, tc, unhex32(&o.id), &p, new, out)?,
                    (false, false) => {
                        if !entry_eq(o, n) {
                            out.changes.push(RawChange { path: p, old: Some(o.clone()), new: Some(n.clone()) });
                        }
                    }
                    _ => {
                        removed(ws, db, tc, &p, o, out)?;
                        added(ws, db, tc, &p, n, new, out)?;
                    }
                }
                i += 1;
                j += 1;
            }
        }
    }
    Ok(())
}

fn removed(ws: &Ws, db: &Connection, tc: &mut TreeCache, path: &str, o: &TreeEntry, out: &mut DiffOut) -> Result<()> {
    if o.kind != 'd' {
        out.changes.push(RawChange { path: path.to_string(), old: Some(o.clone()), new: None });
        return Ok(());
    }
    if let Some(id) = unhex32(&o.id) {
        let t = load_cached(ws, db, tc, &id)?;
        for e in t.iter() {
            removed(ws, db, tc, &join(path, &e.name), e, out)?;
        }
    }
    Ok(())
}

fn added(ws: &Ws, db: &Connection, tc: &mut TreeCache, path: &str, n: &TreeEntry, new: &Trees, out: &mut DiffOut) -> Result<()> {
    if n.kind != 'd' {
        out.changes.push(RawChange { path: path.to_string(), old: None, new: Some(n.clone()) });
        return Ok(());
    }
    diff_dir(ws, db, tc, None, path, new, out)
}

/// Every entry of a stored tree as (path, entry), folders included as `d` entries.
pub fn flatten_tree(ws: &Ws, db: &Connection, tree: &[u8; 32]) -> Result<Vec<(String, TreeEntry)>> {
    let mut out = Vec::new();
    fn walk(ws: &Ws, db: &Connection, id: &[u8; 32], dir: &str, out: &mut Vec<(String, TreeEntry)>) -> Result<()> {
        for e in ws.load_tree(db, id)? {
            let p = join(dir, &e.name);
            if e.kind == 'd' {
                if let Some(sub) = unhex32(&e.id) {
                    out.push((p.clone(), e.clone()));
                    walk(ws, db, &sub, &p, out)?;
                }
            } else {
                out.push((p, e));
            }
        }
        Ok(())
    }
    walk(ws, db, tree, "", &mut out)?;
    Ok(out)
}

/// The entry at `path` in a stored tree, if any.
pub fn lookup_path(ws: &Ws, db: &Connection, tc: &mut TreeCache, tree: &[u8; 32], path: &str) -> Result<Option<TreeEntry>> {
    let mut cur = *tree;
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        let t = load_cached(ws, db, tc, &cur)?;
        let found = t.binary_search_by(|e| e.name.as_bytes().cmp(part.as_bytes())).ok().map(|ix| t[ix].clone());
        match found {
            None => return Ok(None),
            Some(e) if i + 1 == parts.len() => return Ok(Some(e)),
            Some(e) if e.kind == 'd' => match unhex32(&e.id) {
                Some(id) => cur = id,
                None => return Ok(None),
            },
            Some(_) => return Ok(None),
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------- classification

struct Ev {
    session: String,
    turn: Option<i64>,
    tuid: Option<String>,
    tool: String,
    agent: Option<String>,
    paths: Vec<String>,
}

fn overlap(db: &Connection, session: &str, baseline_at: i64, now: i64) -> Result<bool> {
    let r = db
        .prepare_cached(
            "SELECT 1 FROM turns t JOIN sessions s ON s.id=t.session_id WHERE t.session_id<>?1 AND \
             ((t.end_kind='open' AND s.last_seen>?2) OR (t.ended_at IS NOT NULL AND t.ended_at>?3)) LIMIT 1",
        )?
        .query_row(params![session, now - STALE_TURN_NS, baseline_at], |_| Ok(()))
        .optional()?;
    Ok(r.is_some())
}

fn background_ids(db: &Connection) -> Result<Option<String>> {
    let j: Option<String> = db
        .prepare_cached("SELECT background_json FROM turns WHERE end_kind<>'open' ORDER BY ended_at DESC, id DESC LIMIT 1")?
        .query_row([], |r| r.get::<_, Option<String>>(0))
        .optional()?
        .flatten();
    let j = match j {
        Some(j) if !j.trim().is_empty() => j,
        _ => return Ok(None),
    };
    let v: serde_json::Value = match serde_json::from_str(&j) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let arr = match v.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return Ok(None),
    };
    let mut ids: Vec<String> = Vec::new();
    for (n, item) in arr.iter().enumerate().take(5) {
        let id = match item {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Object(o) => ["id", "task_id", "taskId", "shell_id", "name"].iter().find_map(|k| o.get(*k).and_then(|x| x.as_str())).map(|s| s.to_string()).unwrap_or_else(|| n.to_string()),
            _ => n.to_string(),
        };
        ids.push(trunc_chars(&id, 40));
    }
    Ok(Some(ids.join(",")))
}

fn same_state(a: &TreeEntry, b: &Option<TreeEntry>) -> bool {
    match b {
        Some(b) => a.kind == b.kind && a.id == b.id && a.mode == b.mode,
        None => false,
    }
}

/// Did the unexplained changes put every path back to its state at one earlier visible save point?
fn detect_rewind(ws: &Ws, db: &Connection, unexplained: &[&RawChange]) -> Result<Option<i64>> {
    if unexplained.is_empty() || unexplained.len() > 200 {
        return Ok(None);
    }
    let mut st = db.prepare_cached("SELECT id,tree_id FROM points WHERE visible=1 AND dropped=0 ORDER BY id DESC LIMIT 200")?;
    let pts: Vec<(i64, Vec<u8>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
    let mut tc: TreeCache = HashMap::new();
    'points: for (pid, t) in pts {
        let tid = match blob_id_from_vec(&t) {
            Some(t) => t,
            None => continue,
        };
        for c in unexplained {
            let there = lookup_path(ws, db, &mut tc, &tid, &c.path)?;
            let ok = match (&c.new, &there) {
                (None, None) => true,
                (Some(n), t) => same_state(n, t),
                _ => false,
            };
            if !ok {
                continue 'points;
            }
        }
        return Ok(Some(pid));
    }
    Ok(None)
}

fn classify(ws: &Ws, db: &Connection, req: &Req, baseline_at: i64, wm: i64, raw: Vec<RawChange>) -> Result<(Vec<ChangeRow>, Option<String>)> {
    let evs: Vec<Ev> = db
        .prepare_cached("SELECT session_id,turn_id,tool_use_id,tool_name,agent_id,paths_json FROM tool_events WHERE id>?1 ORDER BY id")?
        .query_map([wm], |r| {
            let pj: Option<String> = r.get(5)?;
            Ok(Ev {
                session: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                turn: r.get(1)?,
                tuid: r.get(2)?,
                tool: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                agent: r.get(4)?,
                paths: pj.and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok()).unwrap_or_default(),
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut named: HashMap<&str, &Ev> = HashMap::new();
    let mut bash_main: Vec<&Ev> = Vec::new();
    let mut bash_sub: Vec<&Ev> = Vec::new();
    for e in &evs {
        if EDIT_TOOLS.contains(&e.tool.as_str()) {
            for p in &e.paths {
                named.insert(p.as_str(), e);
            }
        } else if e.tool == "Bash" && e.session == req.session {
            if e.agent.is_some() {
                bash_sub.push(e);
            } else {
                bash_main.push(e);
            }
        }
    }
    let now = now_ns();
    let gap = req.turn.is_none();
    let mut rows: Vec<ChangeRow> = Vec::with_capacity(raw.len());
    let mut unexplained: Vec<usize> = Vec::new();
    for c in raw {
        let mut row = ChangeRow { path: c.path.clone(), old: c.old, new: c.new, class: "outside", turn_id: req.turn, tool_use_id: None, agent_id: None };
        if let Some(e) = named.get(c.path.as_str()) {
            row.class = if e.agent.is_some() { "subagent" } else { "edit" };
            row.turn_id = e.turn;
            row.tool_use_id = e.tuid.clone();
            row.agent_id = e.agent.clone();
        } else {
            unexplained.push(rows.len());
        }
        rows.push(row);
    }
    if unexplained.is_empty() {
        return Ok((rows, None));
    }
    let mut note: Option<String> = None;
    let shared = overlap(db, req.session, baseline_at, now)?;
    let transient_old = |r: &ChangeRow| r.old.as_ref().is_some_and(|o| o.kind == 'x' && TRANSIENT.contains(&o.id.as_str())) && r.new.as_ref().is_some_and(|n| n.kind != 'x');
    // Gap-only judgments are made for all unexplained paths together.
    let mut gap_class: Option<&'static str> = None;
    if gap && !shared && bash_main.is_empty() && bash_sub.is_empty() {
        let probe: Vec<RawChange> = unexplained.iter().filter(|&&i| !transient_old(&rows[i])).map(|&i| RawChange { path: rows[i].path.clone(), old: rows[i].old.clone(), new: rows[i].new.clone() }).collect();
        let refs: Vec<&RawChange> = probe.iter().collect();
        if let Some(k) = detect_rewind(ws, db, &refs)? {
            gap_class = Some("rewind");
            note = Some(format!("rewind:{k}"));
        } else if let Some(ids) = background_ids(db)? {
            gap_class = Some("background");
            note = Some(format!("background:{ids}"));
        }
    }
    let uniq = |v: &[&Ev], f: fn(&Ev) -> Option<&String>| -> Option<String> {
        let mut s: Vec<&String> = v.iter().filter_map(|e| f(e)).collect();
        s.sort();
        s.dedup();
        if s.len() == 1 { Some(s[0].clone()) } else { None }
    };
    for i in unexplained {
        let r = &mut rows[i];
        if transient_old(r) {
            r.class = "outside";
        } else if shared {
            r.class = "shared";
        } else if !bash_sub.is_empty() {
            r.class = "subagent";
            r.agent_id = uniq(&bash_sub, |e| e.agent.as_ref());
            if bash_sub.len() == 1 {
                r.tool_use_id = bash_sub[0].tuid.clone();
            }
        } else if !bash_main.is_empty() {
            r.class = "shell";
            if bash_main.len() == 1 {
                r.tool_use_id = bash_main[0].tuid.clone();
                r.turn_id = bash_main[0].turn.or(req.turn);
            }
        } else if let Some(c) = gap_class {
            r.class = c;
        } else {
            r.class = "outside";
        }
    }
    Ok((rows, note))
}

// ---------------------------------------------------------------- the capture

/// Time one scan without hashing or storing: file count and milliseconds (auto-init, spec 5.10).
pub fn timing_scan(ws: &Ws, db: &Connection) -> Result<(usize, u64)> {
    let prev = meta_get_i64(db, "file_count").unwrap_or(0).max(0) as usize;
    let s = scan::scan(&ws.root, scan::thread_count(prev), scan::ENTRY_CAP)?;
    let n = s.file_count();
    update_mode(db, s.elapsed_ms, n)?;
    Ok((n, s.elapsed_ms))
}

pub fn capture(ws: &Ws, db: &Connection, req: &Req) -> Result<Captured> {
    check_disk(&ws.home)?;
    let prev = last_point(db)?;
    let prev_count = meta_get_i64(db, "file_count").unwrap_or(0).max(0) as usize;
    // The walk never touches the connection, so it runs while the stat cache is read (about 9 ms off the critical path).
    let root = &ws.root; // Ws holds the connection (not Sync): hand the thread only the path
    let (sc, cache) = std::thread::scope(|s| {
        let h = s.spawn(|| scan::scan(root, scan::thread_count(prev_count), scan::ENTRY_CAP));
        let cache = load_cache(db);
        // A panic in the walk is re-raised here, inside the hook's catch_unwind, so fail-open holds.
        let sc = h.join().unwrap_or_else(|p| std::panic::resume_unwind(p));
        (sc, cache)
    });
    let (sc, cache) = (sc?, cache?);
    let file_count = sc.file_count();

    // Key: loaded when a secret file exists; created at the first secret that must be stored.
    let any_secret = sc.entries.iter().any(|e| e.secret && e.kind == EKind::File && e.reason.is_none());
    let mut key: Option<Key> = if any_secret { Key::load().unwrap_or(None) } else { None };
    let key_tag = |k: &Option<Key>| k.as_ref().map(|k| format!("s:{}", k.id)).unwrap_or_default();

    enum Dec {
        Dir,
        X(&'static str),
        Cached([u8; 32], char),
        Job,
    }
    let mut decs: Vec<Dec> = Vec::with_capacity(sc.entries.len());
    let mut need_key = false;
    let mut tag = key_tag(&key);
    for e in &sc.entries {
        let d = match e.kind {
            EKind::Dir => Dec::Dir,
            EKind::Special => Dec::X("special"),
            _ => {
                if let Some(r) = e.reason {
                    Dec::X(r)
                } else if e.kind == EKind::File && e.size > cap_for(e) {
                    Dec::X("too_big")
                } else {
                    let (c, want) = if e.kind == EKind::Link {
                        ('l', "l")
                    } else if e.secret {
                        ('s', tag.as_str())
                    } else {
                        ('f', "f")
                    };
                    match cache.get(&e.path) {
                        Some(row) if !want.is_empty() && cache_trusted(row, e, want) => Dec::Cached(row.blob_id, c),
                        _ => {
                            if e.secret && e.kind == EKind::File {
                                need_key = true;
                            }
                            Dec::Job
                        }
                    }
                }
            }
        };
        decs.push(d);
    }
    let mut key_failed = false;
    if need_key && key.is_none() {
        match Key::create() {
            Ok(k) => key = Some(k),
            Err(e) => {
                key_failed = true;
                fault_tx(db, &ws.home, &ws.id, "capture", "key", &e.to_string());
            }
        }
        tag = key_tag(&key);
    }
    let jobs: Vec<&FEntry> = sc.entries.iter().zip(decs.iter()).filter(|(_, d)| matches!(d, Dec::Job)).map(|(e, _)| e).collect();
    let new_bytes = AtomicU64::new(0);
    let hctx = HashCtx { root: &ws.root, key: key.as_ref(), deadline: req.budget.map(|b| Instant::now() + b), new_bytes: &new_bytes };
    let outs = hash_all(&hctx, &jobs);
    let mut outs = outs.into_iter();
    let written_ns = now_ns();

    let mut leaves: Vec<Leaf> = Vec::with_capacity(sc.entries.len());
    let mut prepared: Vec<Prepared> = Vec::new();
    let mut cache_rows: Vec<(String, CacheRow)> = Vec::new();
    let mut capped = false;
    for (e, d) in sc.entries.iter().zip(decs.iter()) {
        let name = e.name().to_string();
        match d {
            Dec::Dir => leaves.push(Leaf { path: e.path.clone(), e: TreeEntry { name, kind: 'd', mode: 755, id: String::new(), size: 0 } }),
            Dec::X(r) => leaves.push(x_leaf(&e.path, r, e.size, e.mode)),
            Dec::Cached(id, c) => leaves.push(Leaf {
                path: e.path.clone(),
                e: TreeEntry { name, kind: *c, mode: if *c == 'l' { 644 } else { exec_mode(e.mode) }, id: hex(id), size: e.size },
            }),
            Dec::Job => match outs.next() {
                Some(Out::Blob { prep, st, kind }) => {
                    let row_kind = match kind {
                        'l' => "l".to_string(),
                        's' => tag.clone(),
                        _ => "f".to_string(),
                    };
                    cache_rows.push((
                        e.path.clone(),
                        CacheRow { size: st.size as i64, mtime_ns: st.mtime_ns, ctime_ns: st.ctime_ns, ino: st.ino as i64, mode: st.mode as i64, blob_id: prep.id, kind: row_kind, written_ns },
                    ));
                    leaves.push(Leaf {
                        path: e.path.clone(),
                        e: TreeEntry { name, kind, mode: if kind == 'l' { 644 } else { exec_mode(st.mode) }, id: hex(&prep.id), size: st.size },
                    });
                    prepared.push(prep);
                }
                Some(Out::X(r, size)) => {
                    if r == "cap" {
                        capped = true;
                    }
                    let r = if key_failed && e.secret && r == "unreadable" { "unreadable" } else { r };
                    leaves.push(x_leaf(&e.path, r, size, e.mode));
                }
                Some(Out::Gone) | None => {}
            },
        }
    }

    // Carry forward what the walk could not see, so that nothing looks deleted.
    let incomplete = sc.cap_hit || !sc.unreadable.is_empty();
    if incomplete {
        if let Some(p) = &prev {
            let visited: HashSet<&str> = sc.entries.iter().map(|e| e.path.as_str()).collect();
            for (path, e) in flatten_tree(ws, db, &p.tree_id)? {
                if visited.contains(path.as_str()) {
                    continue;
                }
                let under = sc.unreadable.iter().any(|u| u.is_empty() || (path.len() > u.len() && path.starts_with(u.as_str()) && path.as_bytes()[u.len()] == b'/'));
                if sc.cap_hit || under {
                    leaves.push(Leaf { path, e });
                }
            }
        }
    }
    let partial = incomplete || capped;
    let not_saved: Vec<(String, String, u64)> = leaves.iter().filter(|l| l.e.kind == 'x').map(|l| (l.path.clone(), l.e.id.clone(), l.e.size)).collect();

    let trees = build_trees(leaves);
    let mut tc: TreeCache = HashMap::new();
    let mut diff = DiffOut { changes: Vec::new(), dirty: Vec::new() };
    diff_dir(ws, db, &mut tc, prev.as_ref().map(|p| p.tree_id), "", &trees, &mut diff)?;
    let changed = prev.as_ref().is_none_or(|p| p.tree_id != trees.root);

    // Event consumption watermark moves at every full scan, even when no point is written.
    // The old value is what classification reads: it explains changes with the events since the last scan.
    let wm = meta_get_i64(db, "consumed_event_id").unwrap_or(0);
    let max_ev: i64 = db.query_row("SELECT COALESCE(MAX(id),0) FROM tool_events", [], |r| r.get(0))?;
    let baseline_at = prev.as_ref().map_or(0, |p| p.created_at);
    let mut out = Captured { tree_id: trees.root, changed, partial, not_saved: not_saved.len(), scan_ms: sc.elapsed_ms, file_count, ..Default::default() };

    let skip_point = req.only_if_changed && prev.is_some() && !changed;
    if !skip_point {
        // Store: trees first, then new file blobs (loose files are written before the rows that name them).
        for dir in &diff.dirty {
            if let (Some(raw), Some(id)) = (trees.raw.get(dir), trees.ids.get(dir)) {
                let p = prepare_plain_tree(raw, *id)?;
                ws.insert_blob(db, &p)?;
            }
        }
        for p in &prepared {
            ws.insert_blob(db, p)?;
        }
    } else {
        // Nothing changed in the tree; file blobs for touched-but-equal files already exist.
        for p in &prepared {
            if !ws.blob_exists(db, &p.id) {
                ws.insert_blob(db, p)?;
            }
        }
    }
    for (path, row) in &cache_rows {
        upsert_cache(db, path, row)?;
    }
    if !incomplete {
        // Forget cache rows of files that no longer exist.
        let present: HashSet<&str> = sc.entries.iter().map(|e| e.path.as_str()).collect();
        let gone: Vec<&String> = cache.keys().filter(|k| !present.contains(k.as_str())).collect();
        if !gone.is_empty() {
            let mut del = db.prepare_cached("DELETE FROM stat_cache WHERE path=?1")?;
            for g in gone {
                del.execute([g])?;
            }
        }
    }
    meta_set(db, "consumed_event_id", &max_ev.to_string())?;
    update_mode(db, sc.elapsed_ms, file_count)?;

    if skip_point {
        return Ok(out);
    }

    let (rows, note) = match &prev {
        Some(_) => classify(ws, db, req, baseline_at, wm, diff.changes.into_iter().collect())?,
        None => (Vec::new(), None),
    };
    out.n_changes = rows.len();
    let now = now_ns();
    let mut parent = prev.as_ref().map(|p| p.id);
    for (i, spec) in req.points.iter().enumerate() {
        let note_text = match (&spec.note, i == 0, &note) {
            (Some(a), true, Some(b)) => Some(format!("{a} {b}")),
            (Some(a), _, _) => Some(a.clone()),
            (None, true, Some(b)) => Some(b.clone()),
            _ => None,
        };
        db.prepare_cached("INSERT INTO points(kind,visible,session_id,turn_id,tree_id,parent_id,created_at,partial,pinned,dropped,note) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,0,?10)")?.execute(params![
            spec.kind.as_str(),
            spec.kind.visible() as i64,
            req.session,
            spec.turn_id,
            &trees.root[..],
            parent,
            now,
            partial as i64,
            (spec.kind == PointKind::PreRestore) as i64,
            note_text
        ])?;
        let pid = db.last_insert_rowid();
        out.point_ids.push(pid);
        parent = Some(pid);
        if i == 0 {
            let mut ins = db.prepare_cached("INSERT OR REPLACE INTO changes(point_id,path,old_id,new_id,old_mode,new_mode,class,turn_id,tool_use_id,agent_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)")?;
            for r in &rows {
                ins.execute(params![
                    pid,
                    r.path,
                    r.old.as_ref().map(id_text),
                    r.new.as_ref().map(id_text),
                    r.old.as_ref().map(|e| e.mode as i64),
                    r.new.as_ref().map(|e| e.mode as i64),
                    r.class,
                    r.turn_id,
                    r.tool_use_id,
                    r.agent_id
                ])?;
            }
        }
        let mut nc = db.prepare_cached("INSERT OR REPLACE INTO not_captured(point_id,path,reason,size) VALUES(?1,?2,?3,?4)")?;
        for (path, reason, size) in not_saved.iter().take(NOT_CAPTURED_ROWS_MAX) {
            nc.execute(params![pid, path, reason, *size as i64])?;
        }
    }
    Ok(out)
}

fn prepare_plain_tree(raw: &[u8], id: [u8; 32]) -> Result<Prepared> {
    let stored = zstd::bulk::compress(raw, 3)?;
    let loose = stored.len() > INLINE_MAX;
    Ok(Prepared { id, kind: "tree", size: raw.len() as u64, stored, key_id: None, loose })
}

// ---------------------------------------------------------------- named-path capture (PostToolUse edit family)

/// Hash and store exactly the named files (root-relative) and refresh their stat-cache rows. No walk, no point.
pub fn capture_named(ws: &Ws, db: &Connection, rels: &[String]) -> Result<usize> {
    let mut key: Option<Key> = None;
    let mut done = 0;
    let new_bytes = AtomicU64::new(0);
    for rel in rels {
        if scan::rel_hard_excluded(&ws.root, rel) {
            continue;
        }
        let abs = ws.root.join(rel);
        let md = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !md.is_file() && !md.file_type().is_symlink() {
            continue;
        }
        let st = StatSnap::of(&md);
        let name = name_of(rel);
        let e = FEntry {
            path: rel.clone(),
            kind: if md.is_file() { EKind::File } else { EKind::Link },
            size: st.size,
            mode: st.mode,
            mtime_ns: st.mtime_ns,
            ctime_ns: st.ctime_ns,
            ino: st.ino,
            precious: scan::is_precious_name(name),
            secret: scan::is_secret(rel),
            reason: None,
        };
        if e.kind == EKind::File && e.size > cap_for(&e) {
            continue;
        }
        if e.secret && key.is_none() {
            key = Key::load_or_create().ok();
        }
        let ctx = HashCtx { root: &ws.root, key: key.as_ref(), deadline: None, new_bytes: &new_bytes };
        if let Out::Blob { prep, st, kind } = hash_one(&ctx, &e) {
            ws.insert_blob(db, &prep)?;
            let kind_tag = match kind {
                'l' => "l".to_string(),
                's' => key.as_ref().map(|k| format!("s:{}", k.id)).unwrap_or_default(),
                _ => "f".to_string(),
            };
            upsert_cache(
                db,
                rel,
                &CacheRow { size: st.size as i64, mtime_ns: st.mtime_ns, ctime_ns: st.ctime_ns, ino: st.ino as i64, mode: st.mode as i64, blob_id: prep.id, kind: kind_tag, written_ns: now_ns() },
            )?;
            done += 1;
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(path: &str, kind: char, id: &str) -> Leaf {
        Leaf { path: path.into(), e: TreeEntry { name: name_of(path).into(), kind, mode: 644, id: id.into(), size: 1 } }
    }

    #[test]
    fn trees_are_canonical_and_order_independent() {
        let a = vec![leaf("a", 'd', ""), leaf("a/x", 'f', "11"), leaf("a-b", 'f', "22"), leaf("a/y/z", 'f', "33"), leaf("e", 'd', "")];
        let mut b = a.clone();
        b.reverse();
        let (ta, tb) = (build_trees(a), build_trees(b));
        assert_eq!(ta.root, tb.root);
        // sorted by name bytes: "a" (dir) < "a-b" < "e" ('-' sorts before nothing here, "a" is a prefix of "a-b")
        let names: Vec<&str> = ta.dirs[""].iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a", "a-b", "e"]);
        assert!(ta.dirs.contains_key("a/y"));
        // changing one deep leaf changes the chain of ids up to the root only
        let c = build_trees(vec![leaf("a", 'd', ""), leaf("a/x", 'f', "11"), leaf("a-b", 'f', "22"), leaf("a/y/z", 'f', "34"), leaf("e", 'd', "")]);
        assert_ne!(ta.root, c.root);
        assert_eq!(ta.ids["e"], c.ids["e"]);
        assert_ne!(ta.ids["a/y"], c.ids["a/y"]);
    }
}
