//! The store (spec section 2): layout, SQLite schema v1, content store, trees, key, locks, faults.
use crate::util::*;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const FORMAT_LINE: &str = "torpor-store 1";
pub const SCHEMA_VERSION: i64 = 1;
/// Compressed blobs up to this size live inline in `blobs.data`; bigger ones are loose files.
pub const INLINE_MAX: usize = 64 * 1024;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, transcript_path TEXT, first_seen INTEGER, last_seen INTEGER, source TEXT, permission_mode TEXT, ended_at INTEGER, end_reason TEXT, transcript_offset INTEGER NOT NULL DEFAULT 0, compact_epoch INTEGER NOT NULL DEFAULT 0, notified_restore_id INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS turns(id INTEGER PRIMARY KEY, n INTEGER NOT NULL UNIQUE, session_id TEXT NOT NULL, prompt_id TEXT, started_at INTEGER, ended_at INTEGER, start_point INTEGER, end_point INTEGER, end_kind TEXT NOT NULL DEFAULT 'open', origin TEXT NOT NULL DEFAULT 'unknown', origin_by TEXT NOT NULL DEFAULT 'none', preview TEXT, bash_count INTEGER NOT NULL DEFAULT 0, background_json TEXT);
CREATE TABLE IF NOT EXISTS points(id INTEGER PRIMARY KEY, kind TEXT NOT NULL, visible INTEGER NOT NULL DEFAULT 1, session_id TEXT, turn_id INTEGER, tree_id BLOB NOT NULL, parent_id INTEGER, created_at INTEGER NOT NULL, partial INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0, dropped INTEGER NOT NULL DEFAULT 0, note TEXT);
CREATE TABLE IF NOT EXISTS changes(point_id INTEGER NOT NULL, path TEXT NOT NULL, old_id TEXT, new_id TEXT, old_mode INTEGER, new_mode INTEGER, class TEXT NOT NULL, turn_id INTEGER, tool_use_id TEXT, agent_id TEXT, PRIMARY KEY(point_id, path));
CREATE TABLE IF NOT EXISTS tool_events(id INTEGER PRIMARY KEY, session_id TEXT, turn_id INTEGER, tool_use_id TEXT, tool_name TEXT, agent_id TEXT, paths_json TEXT, cmd_preview TEXT, exit_code INTEGER, failed INTEGER NOT NULL DEFAULT 0, duration_ms INTEGER, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS not_captured(point_id INTEGER NOT NULL, path TEXT NOT NULL, reason TEXT NOT NULL, size INTEGER, PRIMARY KEY(point_id, path));
CREATE TABLE IF NOT EXISTS blobs(id BLOB PRIMARY KEY, kind TEXT NOT NULL, size INTEGER NOT NULL, stored_size INTEGER NOT NULL, key_id TEXT, data BLOB, loose INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS stat_cache(path TEXT PRIMARY KEY, size INTEGER, mtime_ns INTEGER, ctime_ns INTEGER, ino INTEGER, mode INTEGER, blob_id BLOB, kind TEXT, written_ns INTEGER);
CREATE TABLE IF NOT EXISTS heartbeat(session_id TEXT NOT NULL, event TEXT NOT NULL, last_at INTEGER NOT NULL, count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(session_id, event));
CREATE TABLE IF NOT EXISTS faults(id INTEGER PRIMARY KEY, at INTEGER NOT NULL, event TEXT, kind TEXT NOT NULL, detail TEXT, seen INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS restores(id INTEGER PRIMARY KEY, kind TEXT, target_point INTEGER, pre_point INTEGER, result_point INTEGER, started_at INTEGER, finished_at INTEGER, status TEXT, report_json TEXT);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
CREATE INDEX IF NOT EXISTS changes_turn ON changes(turn_id);
CREATE INDEX IF NOT EXISTS changes_path ON changes(path);
CREATE INDEX IF NOT EXISTS points_created ON points(created_at);
CREATE INDEX IF NOT EXISTS turns_session_prompt ON turns(session_id, prompt_id);
CREATE INDEX IF NOT EXISTS tool_events_turn ON tool_events(turn_id);
"#;

// ---------------------------------------------------------------- paths

pub fn user_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| PathBuf::from("/"))
}

/// Claude Code's own folder (`~/.claude`, or CLAUDE_CONFIG_DIR). Torpor only ever reads from it.
pub fn claude_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => user_home().join(".claude"),
    }
}

/// `~/.torpor`, or TORPOR_HOME.
pub fn home_dir() -> PathBuf {
    match std::env::var_os("TORPOR_HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => user_home().join(".torpor"),
    }
}

/// `~/.config/torpor/key`, or TORPOR_KEY_FILE.
pub fn key_path() -> PathBuf {
    match std::env::var_os("TORPOR_KEY_FILE") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => user_home().join(".config").join("torpor").join("key"),
    }
}

/// Workspace root: CLAUDE_PROJECT_DIR, else the payload cwd, else the process cwd; canonicalized.
/// Refuses `/`, the home folder itself and anything inside the store.
pub fn resolve_root(cwd_hint: Option<&str>) -> Result<PathBuf> {
    let raw: PathBuf = match std::env::var_os("CLAUDE_PROJECT_DIR") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => match cwd_hint {
            Some(c) if !c.is_empty() => PathBuf::from(c),
            _ => std::env::current_dir()?,
        },
    };
    let root = fs::canonicalize(&raw)?;
    if root == Path::new("/") {
        return Err(Error::Refused("root".into()));
    }
    if let Ok(h) = fs::canonicalize(user_home()) {
        if root == h {
            return Err(Error::Refused("home".into()));
        }
    }
    let th = home_dir();
    let th = fs::canonicalize(&th).unwrap_or(th);
    if root.starts_with(&th) {
        return Err(Error::Refused("store".into()));
    }
    Ok(root)
}

fn ws_hash(root: &Path, hex_len: usize) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"torpor-ws-v1\0");
    h.update(root.as_os_str().as_bytes());
    hex(&h.finalize().as_bytes()[..hex_len / 2])
}

pub fn mkdir_private(p: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(p)
}

fn ensure_home(home: &Path, create: bool) -> Result<()> {
    let format = home.join("FORMAT");
    if !home.exists() {
        if !create {
            return Err(Error::NotFound("store".into()));
        }
        mkdir_private(home)?;
    }
    match fs::read_to_string(&format) {
        Ok(s) => {
            let s = s.trim();
            if s == FORMAT_LINE {
                Ok(())
            } else {
                // Anything else is a store this build does not understand: do nothing.
                Err(Error::Newer)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if create {
                write_new_private(&format, format!("{FORMAT_LINE}\n").as_bytes())?;
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn write_new_private(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match OpenOptions::new().write(true).create_new(true).mode(0o600).open(p) {
        Ok(mut f) => f.write_all(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Folder of this workspace inside the store: 16 hex of the root hash, 20 on a collision.
fn pick_ws_dir(home: &Path, root: &Path, create: bool) -> Result<(PathBuf, String)> {
    for len in [16usize, 20] {
        let id = ws_hash(root, len);
        let dir = home.join(&id);
        let root_file = dir.join("root");
        match fs::read(&root_file) {
            Ok(b) if b == root.as_os_str().as_bytes() => return Ok((dir, id)),
            Ok(_) => continue, // another path owns this id
            Err(_) => {
                if !dir.exists() && !create {
                    return Err(Error::NotFound("workspace".into()));
                }
                if create {
                    mkdir_private(&dir)?;
                    for sub in ["objects", "journal", "tmp"] {
                        mkdir_private(&dir.join(sub))?;
                    }
                    // `root` is written last; a half-made folder is simply redone by the next hook.
                    write_new_private(&root_file, root.as_os_str().as_bytes())?;
                    // Re-read in case of a race with another creator.
                    if fs::read(&root_file).map(|b| b == root.as_os_str().as_bytes()).unwrap_or(false) {
                        return Ok((dir, id));
                    }
                    continue;
                }
                return Err(Error::NotFound("workspace".into()));
            }
        }
    }
    Err(Error::msg("workspace id collision"))
}

// ---------------------------------------------------------------- workspace handle

pub struct Ws {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub id: String,
    pub home: PathBuf,
    pub conn: Connection,
    lock: Option<File>,
}

impl Ws {
    /// Open the workspace store for `root`. With `create`, make it (auto-init).
    pub fn open(root: &Path, create: bool) -> Result<Ws> {
        Ws::open_in(&home_dir(), root, create)
    }

    /// Same, with the store folder given (tests open stores of other processes without touching the environment).
    pub fn open_in(home: &Path, root: &Path, create: bool) -> Result<Ws> {
        let home = home.to_path_buf();
        ensure_home(&home, create)?;
        let (dir, id) = pick_ws_dir(&home, root, create)?;
        for sub in ["objects", "journal", "tmp"] {
            if create && !dir.join(sub).exists() {
                mkdir_private(&dir.join(sub))?;
            }
        }
        let conn = open_db(&dir.join("torpor.db"), create)?;
        Ok(Ws { root: root.to_path_buf(), dir, id, home, conn, lock: None })
    }

    fn lock_file(&mut self) -> Result<&File> {
        if self.lock.is_none() {
            let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(self.dir.join("lock"))?;
            self.lock = Some(f);
        }
        Ok(self.lock.as_ref().unwrap())
    }

    /// Shared lock (hooks). Waits at most `wait_ms`, then Err(Busy).
    pub fn lock_shared(&mut self, wait_ms: u64) -> Result<()> {
        let f = self.lock_file()?;
        let start = Instant::now();
        loop {
            match f.try_lock_shared() {
                Ok(()) => return Ok(()),
                Err(fs::TryLockError::WouldBlock) => {
                    if start.elapsed() >= Duration::from_millis(wait_ms) {
                        return Err(Error::Busy);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }

    /// Exclusive lock (restore, recover, gc). Waits at most `wait_ms`, then Err(Busy).
    pub fn lock_exclusive(&mut self, wait_ms: u64) -> Result<()> {
        let f = self.lock_file()?;
        let start = Instant::now();
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(()),
                Err(fs::TryLockError::WouldBlock) => {
                    if start.elapsed() >= Duration::from_millis(wait_ms) {
                        return Err(Error::Busy);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.dir.join("objects")
    }

    pub fn loose_path(&self, id: &[u8; 32]) -> PathBuf {
        let h = hex(id);
        self.objects_dir().join(&h[..2]).join(&h[2..])
    }
}

fn open_db(path: &Path, create: bool) -> Result<Connection> {
    if !path.exists() {
        if !create {
            return Err(Error::NotFound("database".into()));
        }
        write_new_private(path, b"")?;
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(Duration::from_millis(1000))?;
    // journal_mode answers with a row, so it goes through query_row.
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v > SCHEMA_VERSION {
        return Err(Error::Newer);
    }
    if v < SCHEMA_VERSION {
        if !create {
            return Err(Error::NotFound("schema".into()));
        }
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let v2: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let res = if v2 == 0 {
            conn.execute_batch(SCHEMA_SQL).and_then(|_| conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}")))
        } else {
            Ok(())
        };
        match res {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
        }
        // Files are 0600 even if umask or SQLite said otherwise.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(conn)
}

// ---------------------------------------------------------------- meta

pub fn meta_get(db: &Connection, key: &str) -> Option<String> {
    db.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get::<_, Option<String>>(0)).optional().ok().flatten().flatten()
}

pub fn meta_get_i64(db: &Connection, key: &str) -> Option<i64> {
    meta_get(db, key).and_then(|s| s.parse().ok())
}

pub fn meta_set(db: &Connection, key: &str, value: &str) -> Result<()> {
    db.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
    Ok(())
}

// ---------------------------------------------------------------- faults

fn clean_detail(s: &str) -> String {
    let one: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    trunc_chars(&one, 200)
}

/// Append one line to faults.log (rotated at 1 MiB). Never fails loudly.
pub fn log_fault_line(home: &Path, ws_id: &str, event: &str, kind: &str, detail: &str) {
    let _ = fs::create_dir_all(home);
    let p = home.join("faults.log");
    if let Ok(md) = fs::metadata(&p) {
        if md.len() > 1024 * 1024 {
            let _ = fs::rename(&p, home.join("faults.log.1"));
        }
    }
    let ws = if ws_id.is_empty() { "-" } else { ws_id };
    let ev = if event.is_empty() { "-" } else { event };
    let line = format!("{} {} {} {} {}\n", now_ns() / 1_000_000, ws, ev, kind, clean_detail(detail));
    if let Ok(mut f) = OpenOptions::new().append(true).create(true).mode(0o600).open(&p) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Record a fault inside the caller's transaction (row) and in faults.log.
pub fn fault_tx(db: &Connection, home: &Path, ws_id: &str, event: &str, kind: &str, detail: &str) {
    log_fault_line(home, ws_id, event, kind, detail);
    let _ = db.execute("INSERT INTO faults(at,event,kind,detail,seen) VALUES(?1,?2,?3,?4,0)", params![now_ns(), event, kind, clean_detail(detail)]);
}

/// Record a fault from outside any transaction: log line plus a best-effort row through a short connection.
pub fn record_fault(home: &Path, ws_dir: Option<&Path>, ws_id: &str, event: &str, kind: &str, detail: &str) {
    log_fault_line(home, ws_id, event, kind, detail);
    if let Some(d) = ws_dir {
        let p = d.join("torpor.db");
        if p.exists() {
            if let Ok(c) = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX) {
                let _ = c.busy_timeout(Duration::from_millis(50));
                let _ = c.execute("INSERT INTO faults(at,event,kind,detail,seen) VALUES(?1,?2,?3,?4,0)", params![now_ns(), event, kind, clean_detail(detail)]);
            }
        }
    }
}

// ---------------------------------------------------------------- disk

/// (free bytes for non-root users, total bytes) of the file system holding `path`.
pub fn disk_space(path: &Path) -> (u64, u64) {
    let c = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return (u64::MAX, u64::MAX),
    };
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return (u64::MAX, u64::MAX);
        }
        let fr = s.f_frsize as u64;
        (s.f_bavail as u64 * fr, s.f_blocks as u64 * fr)
    }
}

/// Free-space floor: max(1 GiB, min(5% of the disk, 10 GiB)).
pub fn disk_floor(total: u64) -> u64 {
    const GIB: u64 = 1 << 30;
    GIB.max((total / 20).min(10 * GIB))
}

/// Err(Disk) when the file system holding `path` is below the floor (spec 2.4).
pub fn check_disk(path: &Path) -> Result<()> {
    let (mut free, total) = disk_space(path);
    #[cfg(feature = "fault-inject")]
    if let Some(v) = std::env::var("TORPOR_TEST_FREE_BYTES").ok().and_then(|s| s.parse::<u64>().ok()) {
        free = v;
    }
    #[cfg(not(feature = "fault-inject"))]
    let _ = &mut free;
    if free < disk_floor(total) {
        return Err(Error::Disk(format!("{:.1} GB free", free as f64 / 1e9)));
    }
    Ok(())
}

// ---------------------------------------------------------------- key (spec 2.5)

pub struct Key {
    pub bytes: [u8; 32],
    pub id: String,
}

fn key_id_of(bytes: &[u8; 32]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"torpor key id v1\0");
    h.update(bytes);
    hex(&h.finalize().as_bytes()[..4])
}

impl Key {
    /// Read the key file. Ok(None) when there is none (or it is unreadable text).
    pub fn load() -> Result<Option<Key>> {
        let s = match fs::read_to_string(key_path()) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut it = s.split_whitespace();
        if it.next() != Some("torpor-key-v1") {
            return Ok(None);
        }
        let (id, k) = match (it.next(), it.next()) {
            (Some(i), Some(k)) => (i, k),
            _ => return Ok(None),
        };
        match unhex32(k) {
            Some(bytes) => Ok(Some(Key { bytes, id: id.to_string() })),
            None => Ok(None),
        }
    }

    /// Make the key with create_new (mode 0600, folder 0700). If another process won the race, use its key.
    pub fn create() -> Result<Key> {
        let p = key_path();
        if let Some(d) = p.parent() {
            mkdir_private(d)?;
        }
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| Error::msg(format!("random: {e}")))?;
        let id = key_id_of(&bytes);
        let line = format!("torpor-key-v1 {} {}\n", id, hex(&bytes));
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p) {
            Ok(mut f) => {
                f.write_all(line.as_bytes())?;
                f.sync_all()?;
                Ok(Key { bytes, id })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Key::load()?.ok_or_else(|| Error::msg("key file unreadable")),
            Err(e) => Err(e.into()),
        }
    }

    pub fn load_or_create() -> Result<Key> {
        match Key::load()? {
            Some(k) => Ok(k),
            None => Key::create(),
        }
    }

    fn id_key(&self) -> [u8; 32] {
        blake3::derive_key("torpor secret id v1", &self.bytes)
    }
    fn blob_key(&self) -> [u8; 32] {
        blake3::derive_key("torpor secret blob v1", &self.bytes)
    }
}

// ---------------------------------------------------------------- blobs

/// A blob ready to store: hashed, compressed or encrypted, no database access needed.
pub struct Prepared {
    pub id: [u8; 32],
    pub kind: &'static str,
    pub size: u64,
    pub stored: Vec<u8>,
    pub key_id: Option<String>,
    pub loose: bool,
}

pub fn prepare_plain(kind: &'static str, bytes: &[u8]) -> Result<Prepared> {
    let id = *blake3::hash(bytes).as_bytes();
    prepare_plain_with_id(kind, bytes, id)
}

fn prepare_plain_with_id(kind: &'static str, bytes: &[u8], id: [u8; 32]) -> Result<Prepared> {
    let stored = zstd::bulk::compress(bytes, 3)?;
    let loose = stored.len() > INLINE_MAX;
    Ok(Prepared { id, kind, size: bytes.len() as u64, stored, key_id: None, loose })
}

pub fn prepare_secret(bytes: &[u8], key: &Key) -> Result<Prepared> {
    let id = *blake3::keyed_hash(&key.id_key(), bytes).as_bytes();
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut nonce).map_err(|e| Error::msg(format!("random: {e}")))?;
    let cipher = XChaCha20Poly1305::new(&key.blob_key().into());
    let mut aad = b"torpor-secret-v1".to_vec();
    aad.extend_from_slice(&id);
    let ct = cipher.encrypt(&XNonce::from(nonce), Payload { msg: bytes, aad: &aad }).map_err(|_| Error::msg("encrypt failed"))?;
    let mut stored = nonce.to_vec();
    stored.extend_from_slice(&ct);
    Ok(Prepared { id, kind: "secret", size: bytes.len() as u64, stored, key_id: Some(key.id.clone()), loose: false })
}

fn fsync_dir(d: &Path) -> std::io::Result<()> {
    File::open(d)?.sync_all()
}

/// Protocol A (spec 6.2) for one file: temp file in the same folder, write, sync, rename, fsync the folder.
pub fn write_atomic(target: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let dir = target.parent().ok_or_else(|| std::io::Error::other("no parent"))?;
    let mut last = None;
    for _ in 0..2 {
        let mut r = [0u8; 8];
        let _ = getrandom::fill(&mut r);
        let tmp = dir.join(format!(".torpor-{}.tmp", hex(&r)));
        let res = (|| {
            let mut f = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, target)?;
            fsync_dir(dir)
        })();
        match res {
            Ok(()) => return Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                last = Some(e);
            }
        }
    }
    Err(last.unwrap())
}

impl Ws {
    /// Insert a prepared blob (and write its loose file first). Returns true when it was new.
    pub fn insert_blob(&self, db: &Connection, p: &Prepared) -> Result<bool> {
        if p.loose {
            let path = self.loose_path(&p.id);
            if !path.exists() {
                if let Some(d) = path.parent() {
                    mkdir_private(d)?;
                }
                write_atomic(&path, &p.stored, 0o600)?;
            }
        }
        let n = db
            .prepare_cached("INSERT OR IGNORE INTO blobs(id,kind,size,stored_size,key_id,data,loose,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)")?
            .execute(params![
                &p.id[..],
                p.kind,
                p.size as i64,
                p.stored.len() as i64,
                p.key_id,
                if p.loose { None } else { Some(&p.stored[..]) },
                p.loose as i64,
                now_ns()
            ])?;
        Ok(n > 0)
    }

    pub fn blob_exists(&self, db: &Connection, id: &[u8; 32]) -> bool {
        db.prepare_cached("SELECT 1 FROM blobs WHERE id=?1").and_then(|mut s| s.exists([&id[..]])).unwrap_or(false)
    }

    fn stored_bytes(&self, db: &Connection, id: &[u8; 32]) -> Result<(String, Option<String>, Vec<u8>)> {
        let row: Option<(String, Option<String>, Option<Vec<u8>>, i64)> = db
            .prepare_cached("SELECT kind,key_id,data,loose FROM blobs WHERE id=?1")?
            .query_row([&id[..]], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?;
        let (kind, key_id, data, loose) = row.ok_or_else(|| Error::NotFound(format!("blob {}", &hex(id)[..12])))?;
        let stored = if loose != 0 { fs::read(self.loose_path(id))? } else { data.unwrap_or_default() };
        Ok((kind, key_id, stored))
    }

    /// Read a non-secret blob and re-hash it. A mismatch is "damaged save point file".
    pub fn read_blob(&self, db: &Connection, id: &[u8; 32]) -> Result<Vec<u8>> {
        let (kind, _k, stored) = self.stored_bytes(db, id)?;
        if kind == "secret" {
            return Err(Error::msg("secret blob needs the key"));
        }
        let raw = zstd::stream::decode_all(&stored[..]).map_err(|_| Error::msg("damaged save point file"))?;
        if blake3::hash(&raw).as_bytes() != id {
            return Err(Error::msg("damaged save point file"));
        }
        Ok(raw)
    }

    /// Decrypt a secret blob. Err(NotFound("key")) when the key id differs or the key file is gone.
    pub fn read_secret(&self, db: &Connection, id: &[u8; 32], key: Option<&Key>) -> Result<Vec<u8>> {
        let (_kind, key_id, stored) = self.stored_bytes(db, id)?;
        let key = key.ok_or_else(|| Error::NotFound("key".into()))?;
        if key_id.as_deref() != Some(key.id.as_str()) || stored.len() < 24 + 16 {
            return Err(Error::NotFound("key".into()));
        }
        let (nonce, ct) = stored.split_at(24);
        let mut n = [0u8; 24];
        n.copy_from_slice(nonce);
        let cipher = XChaCha20Poly1305::new(&key.blob_key().into());
        let mut aad = b"torpor-secret-v1".to_vec();
        aad.extend_from_slice(id);
        let raw = cipher.decrypt(&XNonce::from(n), Payload { msg: ct, aad: &aad }).map_err(|_| Error::msg("damaged save point file"))?;
        if blake3::keyed_hash(&key.id_key(), &raw).as_bytes() != id {
            return Err(Error::msg("damaged save point file"));
        }
        Ok(raw)
    }
}

// ---------------------------------------------------------------- trees

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    /// f file, l symlink, s secret, d directory, x not captured
    pub kind: char,
    pub mode: u32,
    /// Hex id; for `x` the reason code.
    pub id: String,
    pub size: u64,
}

#[derive(Serialize)]
struct TreeDocOut<'a> {
    v: u32,
    e: Vec<(&'a str, String, u32, &'a str, u64)>,
}
#[derive(Deserialize)]
struct TreeDocIn {
    v: u32,
    e: Vec<(String, String, u32, String, u64)>,
}

/// Canonical tree bytes. `entries` must already be sorted by name bytes.
pub fn encode_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let doc = TreeDocOut { v: 1, e: entries.iter().map(|e| (e.name.as_str(), e.kind.to_string(), e.mode, e.id.as_str(), e.size)).collect() };
    serde_json::to_vec(&doc).expect("tree json")
}

pub fn decode_tree(bytes: &[u8]) -> Result<Vec<TreeEntry>> {
    let d: TreeDocIn = serde_json::from_slice(bytes)?;
    if d.v != 1 {
        return Err(Error::Newer);
    }
    Ok(d.e.into_iter().map(|(name, k, mode, id, size)| TreeEntry { name, kind: k.chars().next().unwrap_or('x'), mode, id, size }).collect())
}

impl Ws {
    pub fn load_tree(&self, db: &Connection, id: &[u8; 32]) -> Result<Vec<TreeEntry>> {
        decode_tree(&self.read_blob(db, id)?)
    }
}

// ---------------------------------------------------------------- points

#[derive(Clone, Debug)]
pub struct PointRow {
    pub id: i64,
    pub kind: String,
    pub tree_id: [u8; 32],
    pub created_at: i64,
}

pub fn last_point(db: &Connection) -> Result<Option<PointRow>> {
    let r = db
        .prepare_cached("SELECT id,kind,tree_id,created_at FROM points ORDER BY id DESC LIMIT 1")?
        .query_row([], |r| {
            let t: Vec<u8> = r.get(2)?;
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, t, r.get::<_, i64>(3)?))
        })
        .optional()?;
    Ok(r.and_then(|(id, kind, t, created_at)| {
        let mut tree_id = [0u8; 32];
        if t.len() == 32 {
            tree_id.copy_from_slice(&t);
            Some(PointRow { id, kind, tree_id, created_at })
        } else {
            None
        }
    }))
}

pub fn blob_id_from_vec(v: &[u8]) -> Option<[u8; 32]> {
    if v.len() == 32 {
        let mut a = [0u8; 32];
        a.copy_from_slice(v);
        Some(a)
    } else {
        None
    }
}
