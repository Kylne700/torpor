//! Small shared helpers: error type, time, hex.
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Db(rusqlite::Error),
    Json(serde_json::Error),
    /// A lock or SQLite stayed busy past its wait limit.
    Busy,
    /// Free space is below the limit; carries a short text like "0.8 GB free".
    Disk(String),
    /// The store was written by a newer Torpor.
    Newer,
    /// The workspace root is refused (home folder, `/`, inside the store).
    Refused(String),
    NotFound(String),
    /// Input that is not usable (bad JSON, stdin over the limit).
    Parse(String),
    Msg(String),
}

impl Error {
    /// The `faults.kind` value for this error.
    pub fn fault_kind(&self) -> &'static str {
        match self {
            Error::Io(_) => "io",
            Error::Db(_) => "io",
            Error::Json(_) => "parse",
            Error::Busy => "busy",
            Error::Disk(_) => "disk",
            Error::Newer => "schema",
            Error::Refused(_) => "io",
            Error::NotFound(_) => "io",
            Error::Parse(_) => "parse",
            Error::Msg(_) => "io",
        }
    }
    pub fn msg(s: impl Into<String>) -> Error {
        Error::Msg(s.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Db(e) => write!(f, "db: {e}"),
            Error::Json(e) => write!(f, "json: {e}"),
            Error::Busy => write!(f, "busy"),
            Error::Disk(s) => write!(f, "disk: {s}"),
            Error::Newer => write!(f, "newer store format"),
            Error::Refused(s) => write!(f, "refused: {s}"),
            Error::NotFound(s) => write!(f, "not found: {s}"),
            Error::Parse(s) => write!(f, "parse: {s}"),
            Error::Msg(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(f, _) = &e {
            if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) {
                return Error::Busy;
            }
        }
        Error::Db(e)
    }
}

/// Unix time in nanoseconds. Every timestamp Torpor stores is this unit.
pub fn now_ns() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(_) => 0,
    }
}

pub const NS_PER_SEC: i64 = 1_000_000_000;

pub fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = (b[2 * i] as char).to_digit(16)?;
        let lo = (b[2 * i + 1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// First `n` characters (not bytes).
pub fn trunc_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Local-free day number (UTC) of a ns timestamp.
pub fn utc_day(ns: i64) -> i64 {
    ns.div_euclid(NS_PER_SEC * 86_400)
}

fn local_tm(ns: i64) -> libc::tm {
    let t = (ns / NS_PER_SEC) as libc::time_t;
    // SAFETY: localtime_r only writes into the zeroed tm we pass.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// Local wall-clock "HH:MM" of a ns timestamp (restore notice).
pub fn local_hhmm(ns: i64) -> String {
    let tm = local_tm(ns);
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// "Oct 20" in local time.
pub fn local_md(ns: i64) -> String {
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let a = local_tm(ns);
    format!("{} {}", M[(a.tm_mon as usize).min(11)], a.tm_mday)
}

/// "today 14:02" or "Oct 20 14:02" in local time.
pub fn local_stamp(ns: i64, now: i64) -> String {
    let (a, b) = (local_tm(ns), local_tm(now));
    if (a.tm_year, a.tm_yday) == (b.tm_year, b.tm_yday) {
        return format!("today {}", local_hhmm(ns));
    }
    format!("{} {}", local_md(ns), local_hhmm(ns))
}

/// "just now", "2 min ago", "3 h ago", "4 days ago".
pub fn ago(now: i64, then: i64) -> String {
    let s = ((now - then) / NS_PER_SEC).max(0);
    match s {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

/// "38 MB", "1.2 GB", "700 KB".
pub fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    let f = n as f64;
    if f >= KB * KB * KB {
        format!("{:.1} GB", f / (KB * KB * KB))
    } else if f >= KB * KB {
        format!("{:.0} MB", f / (KB * KB))
    } else if f >= KB {
        format!("{:.0} KB", f / KB)
    } else {
        format!("{n} B")
    }
}

/// Crash-test hook. `TORPOR_FAULT=<domain>:<k>` kills this process with SIGKILL at the k-th call for that domain
/// (domains: `restore`, `capture`); `<domain>:0` only prints `fault-step <domain> <n>` on stderr for every call, so a test can count the steps.
/// Release builds (no `fault-inject` feature) contain no code that reads the variable.
#[cfg(feature = "fault-inject")]
pub fn fault_point(domain: &str) {
    use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
    static RESTORE: AtomicU64 = AtomicU64::new(0);
    static CAPTURE: AtomicU64 = AtomicU64::new(0);
    let Ok(spec) = std::env::var("TORPOR_FAULT") else { return };
    let Some((d, k)) = spec.split_once(':') else { return };
    let Ok(k) = k.parse::<u64>() else { return };
    if d != domain {
        return;
    }
    let n = if domain == "restore" { &RESTORE } else { &CAPTURE }.fetch_add(1, SeqCst) + 1;
    if k == 0 {
        eprintln!("fault-step {domain} {n}");
    } else if n == k {
        // SAFETY: plain syscall on our own pid; there is nothing to clean up, which is the point.
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }
}
#[cfg(not(feature = "fault-inject"))]
#[inline(always)]
pub fn fault_point(_domain: &str) {}
