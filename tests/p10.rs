//! P10 (spec section 10, N5-25): does opening a SQLite file read-only with rusqlite, and running the online
//! backup API from that connection, write anything next to the database?
//! Watches the database directory with inotify and compares content, size and mtime of every entry before and
//! after (mmap stores to -shm raise no inotify event, so the content comparison is what catches those).
//! The result decides spec 5.8: backup API (no writes) or raw-copy fallback (any write).
use inotify::{Inotify, WatchMask};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

type Snap = BTreeMap<String, (u64, i64, String)>; // name -> (len, mtime_ns, blake3)

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
fn p10_readonly_open_and_backup_write_nothing() {
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
}
