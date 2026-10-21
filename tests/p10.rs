//! P10 (spec section 10, N5-25): does opening a SQLite file read-only with rusqlite, and running the online
//! backup API from that connection, write anything next to the database?
//! Watches the database directory with inotify and compares content, size and mtime of every entry before and
//! after (mmap stores to -shm raise no inotify event, so the content comparison is what catches those).
//! The result decides spec 5.8: backup API (no writes) or raw-copy fallback (any write).
//! ANSWER (recorded by the asserts below): yes, a read-only open of a WAL database writes next to it, so Torpor never
//! opens a user's database: it reads the main file as plain bytes and skips it while a -wal or -journal is live.
mod common;
use common::*;
use inotify::{Inotify, WatchMask};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

type Snap = BTreeMap<String, (u64, i64, String)>; // name -> (len, mtime_ns, blake3)

/// One test at a time: when another thread forks (the Sim tests spawn processes) while a database file is open, the
/// child holds a copy of the descriptor and the kernel delays that file's CLOSE_WRITE event into the next watch.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn snap(dir: &Path) -> Snap {
    let mut m = Snap::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let md = e.metadata().unwrap();
        let bytes = std::fs::read(e.path()).unwrap();
        let mtime = md.mtime() * 1_000_000_000 + md.mtime_nsec();
        m.insert(e.file_name().to_string_lossy().into_owned(), (md.len(), mtime, blake3::hash(&bytes).to_hex().to_string()));
    }
    m
}

fn diff(before: &Snap, after: &Snap) -> Vec<String> {
    let mut out = vec![];
    for (k, v) in before {
        match after.get(k) {
            None => out.push(format!("deleted {k}")),
            Some(a) if a.2 != v.2 => out.push(format!("content-changed {k} ({} -> {} bytes)", v.0, a.0)),
            Some(a) if a.1 != v.1 => out.push(format!("mtime-only {k}")),
            _ => {}
        }
    }
    for k in after.keys() {
        if !before.contains_key(k) {
            out.push(format!("created {k} ({} bytes)", after[k].0));
        }
    }
    out
}

fn watch(dir: &Path) -> Inotify {
    let ino = Inotify::init().unwrap();
    unsafe {
        let fd = ino.as_raw_fd();
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    // Everything that is a write or a change of the directory: not open/access/close-nowrite.
    let mask = WatchMask::MODIFY
        | WatchMask::ATTRIB
        | WatchMask::CLOSE_WRITE
        | WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::DELETE_SELF;
    ino.watches().add(dir, mask).unwrap();
    ino
}

fn events(ino: &mut Inotify) -> Vec<String> {
    let mut buf = [0u8; 4096];
    let mut out = vec![];
    loop {
        match ino.read_events(&mut buf) {
            Ok(it) => {
                let mut any = false;
                for e in it {
                    any = true;
                    out.push(format!("{:?} {}", e.mask, e.name.map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
                }
                if !any {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    out
}

/// What Torpor would do: read-only connection, busy_timeout 200 ms, backup to another file in one step(-1).
fn ro_backup(db: &Path, dest: &Path) -> Result<i64, String> {
    let src = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| format!("open: {e}"))?;
    src.busy_timeout(std::time::Duration::from_millis(200)).map_err(|e| e.to_string())?;
    let mut dst = Connection::open(dest).map_err(|e| e.to_string())?;
    {
        let b = Backup::new(&src, &mut dst).map_err(|e| format!("backup-init: {e}"))?;
        b.step(-1).map_err(|e| format!("step: {e}"))?;
    }
    dst.query_row("select count(*) from t", [], |r| r.get::<_, i64>(0)).map_err(|e| format!("count: {e}"))
}

struct Out {
    name: &'static str,
    result: String,
    writes: Vec<String>,
    events: Vec<String>,
}

fn run(name: &'static str, dir: &Path, db: &Path, dest_dir: &Path) -> Out {
    let before = snap(dir);
    let mut ino = watch(dir);
    let res = ro_backup(db, &dest_dir.join(format!("{name}.copy")));
    let ev = events(&mut ino);
    let after = snap(dir);
    Out { name, result: format!("{res:?}"), writes: diff(&before, &after), events: ev }
}

fn make_wal(db: &Path, rows: i64) -> Connection {
    let c = Connection::open(db).unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    c.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    c.execute("create table if not exists t(a integer, b text)", []).unwrap();
    for i in 0..rows {
        c.execute("insert into t values (?1, ?2)", (i, format!("row-{i}"))).unwrap();
    }
    c
}

#[test]
fn p10_readonly_open_of_a_wal_database_writes_next_to_it() {
    let _one_at_a_time = serial();
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("dest");
    std::fs::create_dir(&dest).unwrap();
    let mut all: Vec<Out> = vec![];

    // 1. live WAL writer: -wal has uncheckpointed frames, -shm exists.
    {
        let d = root.path().join("s1");
        std::fs::create_dir(&d).unwrap();
        let db = d.join("a.db");
        let w = make_wal(&db, 200);
        all.push(run("s1_live_wal_writer", &d, &db, &dest));
        drop(w);
    }
    // 2. live WAL writer with an open (uncommitted) write transaction.
    {
        let d = root.path().join("s2");
        std::fs::create_dir(&d).unwrap();
        let db = d.join("a.db");
        let w = make_wal(&db, 200);
        w.execute_batch("begin immediate; insert into t values (999,'pending');").unwrap();
        all.push(run("s2_wal_open_write_txn", &d, &db, &dest));
        w.execute_batch("rollback").unwrap();
        drop(w);
    }
    // 3. cleanly closed WAL database: no -wal, no -shm on disk (last close checkpointed and removed them).
    {
        let d = root.path().join("s3");
        std::fs::create_dir(&d).unwrap();
        let db = d.join("a.db");
        drop(make_wal(&db, 200));
        let present: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        println!("s3 files before: {present:?}");
        all.push(run("s3_closed_wal_clean", &d, &db, &dest));
    }
    // 4. crash leftover: main + non-empty -wal copied without -shm (no process has it open).
    {
        let live = root.path().join("s4live");
        std::fs::create_dir(&live).unwrap();
        let w = make_wal(&live.join("a.db"), 200);
        let d = root.path().join("s4");
        std::fs::create_dir(&d).unwrap();
        std::fs::copy(live.join("a.db"), d.join("a.db")).unwrap();
        std::fs::copy(live.join("a.db-wal"), d.join("a.db-wal")).unwrap();
        drop(w);
        all.push(run("s4_leftover_wal_no_shm", &d, &d.join("a.db"), &dest));
    }
    // 5. rollback-journal database, idle.
    {
        let d = root.path().join("s5");
        std::fs::create_dir(&d).unwrap();
        let db = d.join("a.db");
        let c = Connection::open(&db).unwrap();
        c.execute("create table t(a integer, b text)", []).unwrap();
        for i in 0..200 {
            c.execute("insert into t values (?1, 'x')", [i]).unwrap();
        }
        drop(c);
        all.push(run("s5_rollback_journal_idle", &d, &db, &dest));
    }
    // 6. rollback-journal database with a writer holding an open write transaction (journal file present).
    {
        let d = root.path().join("s6");
        std::fs::create_dir(&d).unwrap();
        let db = d.join("a.db");
        let c = Connection::open(&db).unwrap();
        c.execute("create table t(a integer, b text)", []).unwrap();
        for i in 0..200 {
            c.execute("insert into t values (?1, 'x')", [i]).unwrap();
        }
        c.execute_batch("begin immediate; update t set b='y';").unwrap();
        all.push(run("s6_rollback_journal_open_writer", &d, &db, &dest));
        c.execute_batch("rollback").unwrap();
    }
    // 7. WAL database whose directory is read-only for us is not testable as root-less here; skipped on purpose.

    let mut report = String::new();
    for o in &all {
        report.push_str(&format!("{}: result={} | file changes: {:?} | inotify: {:?}\n", o.name, o.result, o.writes, o.events));
    }
    println!("{report}");
    let _ = std::fs::write(Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("p10-observations.txt"), &report);

    // The finding. Every scenario still read all 200 rows ...
    for o in &all {
        assert_eq!(o.result, "Ok(200)", "{}: the backup itself failed", o.name);
    }
    let by = |prefix: &str| all.iter().find(|o| o.name.starts_with(prefix)).unwrap();
    // ... but for WAL databases the read-only open touched the side files (opened the -wal for writing, created or
    // changed the -shm) and touched nothing else.
    // (s2 is allowed to show nothing: SQLite may keep the -wal file open until the writer in this process lets go,
    // so the close event can come late.)
    for prefix in ["s1_", "s2_", "s3_", "s4_"] {
        let o = by(prefix);
        let touched: Vec<&String> = o.writes.iter().chain(o.events.iter()).collect();
        assert!(prefix == "s2_" || !touched.is_empty(), "{}: a read-only open of a WAL database wrote nothing, so the backup API would be safe: P10 changed", o.name);
        assert!(touched.iter().all(|t| t.contains("a.db-wal") || t.contains("a.db-shm")), "{}: touched more than the side files: {touched:?}", o.name);
    }
    assert!(by("s3_").writes.iter().any(|w| w.starts_with("created a.db-shm")), "a cleanly closed WAL database got no -shm: {:?}", by("s3_").writes);
    assert!(by("s3_").writes.iter().any(|w| w.starts_with("created a.db-wal")), "a cleanly closed WAL database got no -wal: {:?}", by("s3_").writes);
    assert!(by("s4_").writes.iter().any(|w| w.starts_with("created a.db-shm")), "a leftover -wal got no -shm: {:?}", by("s4_").writes);
    // Rollback-journal databases are not touched.
    for prefix in ["s5_", "s6_"] {
        let o = by(prefix);
        assert!(o.writes.is_empty() && o.events.is_empty(), "{}: a read-only open wrote: {:?} {:?}", o.name, o.writes, o.events);
    }
}

/// The idea for S2 (not built): copying main + -wal + -shm byte for byte into a private folder gives a database that
/// opens with every committed row, and the copy itself writes nothing next to the original.
#[test]
fn p10_raw_copy_of_the_three_files_is_a_readable_database() {
    let _one_at_a_time = serial();
    let root = tempfile::tempdir().unwrap();
    for pending in [false, true] {
        let src = root.path().join(format!("src{pending}"));
        std::fs::create_dir(&src).unwrap();
        let w = make_wal(&src.join("a.db"), 200);
        if pending {
            w.execute_batch("begin immediate; insert into t values (999,'pending');").unwrap();
        }
        assert!(std::fs::metadata(src.join("a.db-wal")).unwrap().len() > 0, "the scenario needs live frames in the -wal");
        let before = snap(&src);
        let copy = root.path().join(format!("copy{pending}"));
        std::fs::create_dir(&copy).unwrap();
        for f in ["a.db", "a.db-wal", "a.db-shm"] {
            std::fs::copy(src.join(f), copy.join(f)).unwrap();
        }
        assert!(diff(&before, &snap(&src)).is_empty(), "copying wrote next to the original");
        let c = Connection::open(copy.join("a.db")).unwrap();
        assert_eq!(c.query_row("select count(*) from t", [], |r| r.get::<_, i64>(0)).unwrap(), 200, "pending={pending}: the copy lost committed rows or shows uncommitted ones");
        assert_eq!(c.query_row("pragma integrity_check", [], |r| r.get::<_, String>(0)).unwrap(), "ok");
        if pending {
            w.execute_batch("rollback").unwrap();
        }
    }
}

/// What Torpor does instead: plain bytes for a database nobody holds, `db_busy` while a -wal or -journal is live,
/// and nothing written next to any database.
#[test]
fn databases_are_saved_as_plain_bytes_and_never_opened() {
    let _one_at_a_time = serial();
    let mut sim = Sim::new();
    let proj = sim.env.proj.clone();
    write_files(&proj, &[("src/a.rs", "fn a() {}\n")]);
    std::fs::create_dir_all(proj.join("data")).unwrap();
    drop(make_wal(&proj.join("data/closed.db"), 50)); // last close checkpoints and removes the side files
    {
        let c = Connection::open(proj.join("data/old.sqlite3")).unwrap(); // rollback journal
        c.execute_batch("create table t(a); insert into t values (1);").unwrap();
    }
    assert!(!proj.join("data/closed.db-wal").exists());

    // Phase A: nobody holds a database. Saved byte for byte.
    sim.session_start();
    sim.prompt("one");
    sim.edit("src/a.rs", "fn a() { 1 }\n");
    sim.stop();
    sim.verify_last_point();
    assert!(!proj.join("data/closed.db-wal").exists() && !proj.join("data/closed.db-shm").exists(), "Torpor opened a database: side files appeared");

    // Phase B: a live WAL writer. The hooks must not wake it, wrap it or leave anything next to it.
    let live = make_wal(&proj.join("data/live.db"), 200);
    assert!(std::fs::metadata(proj.join("data/live.db-wal")).unwrap().len() > 0);
    let before = snap(&proj.join("data"));
    sim.prompt("two");
    sim.edit("src/a.rs", "fn a() { 2 }\n");
    sim.stop();
    let changed = diff(&before, &snap(&proj.join("data")));
    assert!(changed.is_empty(), "the hooks changed things next to the databases: {changed:?}");
    let last = sim.last_visible_point();
    let busy = |path: &str| sim.q_i64(&format!("SELECT COUNT(*) FROM not_captured WHERE point_id={last} AND path='{path}' AND reason='db_busy'"));
    assert_eq!(busy("data/live.db"), 1, "a database with a live -wal is reported as in use");
    assert_eq!(busy("data/closed.db") + busy("data/old.sqlite3"), 0, "idle databases are not reported");
    assert_eq!(sim.q_i64(&format!("SELECT COUNT(*) FROM not_captured WHERE point_id={last}")), 1);

    // Phase C: the writer lets go; the database is now an ordinary file and is saved byte for byte.
    drop(live);
    sim.prompt("three");
    sim.edit("src/a.rs", "fn a() { 3 }\n");
    sim.stop();
    sim.verify_last_point();
    assert!(sim.faults().is_empty(), "faults: {:?}", sim.faults());
}
