//! Retention (spec 2.4): thinning by age, the 2 GB cap, garbage collection with its grace period.
//! `thin` and `gc` are called in-process with an injected `now` (they read nothing from the environment on these
//! paths); the once-a-day thinning and the cap are driven through the real binary with a temp HOME.
mod common;
use common::*;
use rusqlite::{params, Connection};
use std::collections::{BTreeSet, HashMap};
use torpor::retention::{self, GcOutcome};
use torpor::store::Ws;
use torpor::util::now_ns;

const H: i64 = 3_600_000_000_000;
const D: i64 = 24 * H;

struct P {
    name: &'static str,
    kind: &'static str,
    visible: bool,
    pinned: bool,
    /// Age in hours at `now`.
    age_h: i64,
}

fn p(name: &'static str, kind: &'static str, visible: bool, pinned: bool, age_h: i64) -> P {
    P { name, kind, visible, pinned, age_h }
}

fn rw(sim: &Sim) -> Connection {
    let c = Connection::open(sim.ws_dir().join("torpor.db")).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c
}

/// A real store with no history, so the schema is the product's own.
fn empty_store() -> (Sim, Connection) {
    let mut sim = Sim::new();
    sim.session_start();
    let c = rw(&sim);
    c.execute_batch("DELETE FROM points; DELETE FROM turns; DELETE FROM tool_events; DELETE FROM changes; DELETE FROM meta WHERE key IN ('last_thin','thinned_harder_at');").unwrap();
    (sim, c)
}

/// Insert points oldest first so that ids follow time. Returns name -> id.
fn load(c: &Connection, now: i64, mut list: Vec<P>) -> HashMap<&'static str, i64> {
    list.sort_by_key(|p| std::cmp::Reverse(p.age_h));
    let mut ids = HashMap::new();
    for (i, p) in list.iter().enumerate() {
        let id = i as i64 + 1;
        c.execute(
            "INSERT INTO points(id,kind,visible,tree_id,created_at,pinned,note) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![id, p.kind, p.visible as i64, vec![0u8; 32], now - p.age_h * H, p.pinned as i64, if p.kind == "pre_bash" { Some("rm -rf build") } else { None }],
        )
        .unwrap();
        ids.insert(p.name, id);
    }
    ids
}

fn dropped(c: &Connection, ids: &HashMap<&'static str, i64>) -> BTreeSet<&'static str> {
    ids.iter().filter(|(_, id)| c.query_row("SELECT dropped FROM points WHERE id=?1", [**id], |r| r.get::<_, i64>(0)).unwrap() == 1).map(|(n, _)| *n).collect()
}

/// A moment at 12:00 UTC, so that "N hours ago" lands on known calendar days.
fn noon() -> i64 {
    ((1_760_000_000i64 / 86_400) * 86_400 + 12 * 3600) * 1_000_000_000
}

#[test]
fn thinning_keeps_two_days_then_one_point_per_day() {
    let (_sim, c) = empty_store();
    let now = noon();
    let ids = load(
        &c,
        now,
        vec![
            p("new_start", "start", true, false, 1),
            p("end_30h", "end", true, false, 30),
            p("tool_40h", "tool", false, false, 40),
            p("tool_49h", "tool", false, false, 49),
            // three on the same calendar day, 3 days ago: only the last one stays
            p("d3_a", "end", true, false, 72),
            p("d3_b", "start", true, false, 73),
            p("d3_c", "end", true, false, 74),
            p("d4", "end", true, false, 100),
            // a pinned point is never thinned inside W; the unpinned two of its day keep only the later one
            p("d5_pin", "pre_restore", true, true, 120),
            p("d5_a", "end", true, false, 121),
            p("d5_b", "start", true, false, 122),
            p("in_w", "end", true, false, 239),
            p("out_w", "end", true, false, 241),
            p("old_vis", "end", true, false, 264),
            p("old_pin", "pre_restore", true, true, 264),
            p("old_prebash", "pre_bash", true, false, 288),
        ],
    );
    // Text: a prompt and a command from 11 days ago and from 3 days ago.
    for (n, started, preview) in [(1, now - 11 * D, "old prompt"), (2, now - 3 * D, "recent prompt")] {
        c.execute("INSERT INTO turns(id,n,session_id,started_at,preview) VALUES(?1,?1,'s',?2,?3)", params![n, started, preview]).unwrap();
        c.execute("INSERT INTO tool_events(session_id,turn_id,tool_name,cmd_preview,at) VALUES('s',?1,'Bash',?2,?3)", params![n, format!("cmd {preview}"), started]).unwrap();
    }

    let n = retention::thin(&c, now, 10, false).unwrap();
    let got = dropped(&c, &ids);
    let want: BTreeSet<&str> = ["tool_49h", "d3_b", "d3_c", "d5_b", "out_w", "old_vis", "old_pin", "old_prebash"].into_iter().collect();
    assert_eq!(got, want, "dropped points");
    assert_eq!(n, want.len(), "the count thin() reports");

    let preview = |id: i64| c.query_row("SELECT preview FROM turns WHERE id=?1", [id], |r| r.get::<_, Option<String>>(0)).unwrap();
    assert_eq!(preview(1), None, "text older than W goes");
    assert_eq!(preview(2).as_deref(), Some("recent prompt"));
    let cmd = |id: i64| c.query_row("SELECT cmd_preview FROM tool_events WHERE turn_id=?1", [id], |r| r.get::<_, Option<String>>(0)).unwrap();
    assert_eq!(cmd(1), None);
    assert_eq!(cmd(2).as_deref(), Some("cmd recent prompt"));
    let note = |id: i64| c.query_row("SELECT note FROM points WHERE id=?1", [id], |r| r.get::<_, Option<String>>(0)).unwrap();
    assert_eq!(note(ids["old_prebash"]), None, "a command note older than W is cleared");
    assert_eq!(c.query_row("SELECT COUNT(*) FROM turns", [], |r| r.get::<_, i64>(0)).unwrap(), 2, "turn rows are metadata and stay");
    assert_eq!(c.query_row("SELECT COUNT(*) FROM tool_events", [], |r| r.get::<_, i64>(0)).unwrap(), 2);

    // Doing it again changes nothing.
    assert_eq!(retention::thin(&c, now, 10, false).unwrap(), 0);
}

#[test]
fn the_newest_point_is_never_thinned() {
    let (_sim, c) = empty_store();
    let now = noon();
    let ids = load(&c, now, vec![p("a", "end", true, false, 900), p("b", "end", true, false, 800), p("last", "end", true, false, 700)]);
    retention::thin(&c, now, 30, false).unwrap();
    assert_eq!(dropped(&c, &ids), ["a", "b"].into_iter().collect());
    retention::thin(&c, now, 30, true).unwrap();
    assert_eq!(dropped(&c, &ids), ["a", "b"].into_iter().collect());
}

#[test]
fn thinning_harder_keeps_a_day_and_a_week() {
    let (_sim, c) = empty_store();
    let now = noon();
    let ids = load(
        &c,
        now,
        vec![
            p("new", "start", true, false, 1),
            p("tool_20h", "tool", false, false, 20),
            p("tool_26h", "tool", false, false, 26),
            p("v30h", "end", true, false, 30),
            p("v31h", "start", true, false, 31),
            p("v6d", "end", true, false, 144),
            p("v6d_b", "start", true, false, 145),
            p("v8d", "end", true, false, 192),
            p("pin10d", "pre_restore", true, true, 240),
            p("v40d", "end", true, false, 960),
            p("pin40d", "pre_restore", true, true, 960),
        ],
    );
    retention::thin(&c, now, 30, true).unwrap();
    let want: BTreeSet<&str> = ["tool_26h", "v31h", "v6d_b", "v8d", "v40d", "pin40d"].into_iter().collect();
    assert_eq!(dropped(&c, &ids), want);
}

/// Spread every point except the newest over two old calendar days (D-5 and D-3, real UTC days).
fn age_history(c: &Connection, now: i64) -> (Vec<i64>, Vec<i64>, i64) {
    let ids: Vec<i64> = c.prepare("SELECT id FROM points ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    let newest = *ids.last().unwrap();
    let today0 = (now / D) * D;
    let (mut a, mut b) = (vec![], vec![]);
    for (i, id) in ids[..ids.len() - 1].iter().enumerate() {
        let (group, day_back) = if i < (ids.len() - 1) / 2 { (&mut a, 5) } else { (&mut b, 3) };
        group.push(*id);
        c.execute("UPDATE points SET created_at=?2 WHERE id=?1", params![id, today0 - day_back * D + 10 * H + i as i64 * 60_000_000_000]).unwrap();
    }
    (a, b, newest)
}

#[test]
fn thinning_runs_at_session_start_at_most_once_a_day() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "0\n")]);
    sim.session_start();
    for i in 1..=4 {
        sim.prompt(&format!("turn {i}"));
        sim.write_tool("a.txt", &format!("{i}\n"));
        sim.bash("true");
        sim.stop();
    }
    sim.session_end("other");
    let c = rw(&sim);
    let now = now_ns();
    let (day5, day3, newest) = age_history(&c, now);
    assert!(day5.len() >= 3 && day3.len() >= 3);
    c.execute("DELETE FROM meta WHERE key='last_thin'", []).unwrap();
    let alive = |ids: &[i64]| -> Vec<i64> { ids.iter().copied().filter(|id| c.query_row("SELECT dropped FROM points WHERE id=?1", [id], |r| r.get::<_, i64>(0)).unwrap() == 0).collect() };
    let last_visible = |ids: &[i64]| -> i64 { ids.iter().copied().filter(|id| c.query_row("SELECT visible FROM points WHERE id=?1", [id], |r| r.get::<_, i64>(0)).unwrap() == 1).max().unwrap() };

    let mut t = sim.second_session();
    t.session_start();
    assert_eq!(alive(&day5), vec![last_visible(&day5)], "day 5: only the last visible point stays");
    assert_eq!(alive(&day3), vec![last_visible(&day3)], "day 3: only the last visible point stays");
    assert!(alive(&[newest]) == vec![newest], "the newest stays");
    let stamp: i64 = c.query_row("SELECT value FROM meta WHERE key='last_thin'", [], |r| r.get::<_, String>(0)).unwrap().parse().unwrap();
    assert!((now_ns() - stamp).abs() < 30 * 1_000_000_000, "last_thin is stamped");

    // Within a day: not again. A point that would be thinned stays.
    c.execute("INSERT INTO points(id,kind,visible,tree_id,created_at) SELECT 9001,'end',1,tree_id,?1 FROM points WHERE id=?2", params![(now / D) * D - 4 * D, newest]).unwrap();
    c.execute("INSERT INTO points(id,kind,visible,tree_id,created_at) SELECT 9002,'end',1,tree_id,?1 FROM points WHERE id=?2", params![(now / D) * D - 4 * D + H, newest]).unwrap();
    let mut u = t.second_session();
    u.session_start();
    assert_eq!(alive(&[9001, 9002]), vec![9001, 9002], "thinning ran twice within a day");
    // A day later: it does.
    c.execute("UPDATE meta SET value=?1 WHERE key='last_thin'", [(now_ns() - 25 * H).to_string()]).unwrap();
    let mut v = u.second_session();
    v.session_start();
    assert_eq!(alive(&[9001, 9002]), vec![9002]);
}

#[test]
fn a_store_over_two_gigabytes_thins_harder_and_says_so() {
    for over in [false, true] {
        let mut sim = Sim::new();
        write_files(sim.proj(), &[("a.txt", "0\n")]);
        sim.session_start();
        for i in 1..=3 {
            sim.prompt(&format!("turn {i}"));
            sim.write_tool("a.txt", &format!("{i}\n"));
            sim.stop();
        }
        sim.session_end("other");
        let c = rw(&sim);
        let today0 = (now_ns() / D) * D;
        let ids: Vec<i64> = c.prepare("SELECT id FROM points ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        // One visible point ten days old (kept by normal thinning, dropped by the harder kind), then the rest.
        let old = ids[0];
        c.execute("UPDATE points SET created_at=?2 WHERE id=?1", params![old, today0 - 10 * D + 10 * H]).unwrap();
        c.execute("UPDATE points SET visible=1 WHERE id=?1", [old]).unwrap();
        c.execute("DELETE FROM meta WHERE key='last_thin'", []).unwrap();
        if over {
            c.execute("INSERT INTO blobs(id,kind,size,stored_size,created_at) VALUES(randomblob(32),'file',?1,?1,?2)", params![3i64 << 30, now_ns()]).unwrap();
        }
        let mut t = sim.second_session();
        t.session_start();
        let gone: i64 = c.query_row("SELECT dropped FROM points WHERE id=?1", [old], |r| r.get(0)).unwrap();
        let harder_at: Option<String> = c.query_row("SELECT value FROM meta WHERE key='thinned_harder_at'", [], |r| r.get(0)).ok();
        let v: serde_json::Value = serde_json::from_str(t.cli(&["status", "--json"]).stdout.trim()).unwrap();
        let notes = v["notes"].to_string();
        let sentence = "Torpor reached its 2 GB limit for this project and kept fewer old save points.";
        if over {
            assert_eq!(gone, 1, "over the cap the ten-day-old point goes");
            assert!(harder_at.is_some());
            assert!(notes.contains(sentence), "status must say so: {notes}");
        } else {
            assert_eq!(gone, 0, "under the cap a ten-day-old point stays");
            assert!(harder_at.is_none());
            assert!(!notes.contains(sentence), "{notes}");
        }
    }
}

fn noise(seed: u64) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ seed.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    (0..200 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn loose_files(sim: &Sim) -> usize {
    let mut n = 0;
    for d in std::fs::read_dir(sim.ws_dir().join("objects")).unwrap().flatten() {
        n += std::fs::read_dir(d.path()).unwrap().flatten().count();
    }
    n
}

fn count(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn gc_deletes_only_what_nothing_reaches_and_only_after_the_grace() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "version 0\n")]);
    std::fs::write(sim.path("big.bin"), noise(1)).unwrap();
    sim.session_start();
    for i in 1..=4u64 {
        sim.prompt(&format!("turn {i}"));
        sim.write_tool("a.txt", &format!("version {i}\n"));
        std::fs::write(sim.path("big.bin"), noise(i + 1)).unwrap();
        sim.stop();
    }
    let loose0 = loose_files(&sim);
    assert_eq!(loose0, 5, "five versions of the big file are loose objects");

    // Stray files in the object folders: one that no blob row names, one leftover temp file.
    let stray_dir = sim.ws_dir().join("objects").join("cd");
    std::fs::create_dir_all(&stray_dir).unwrap();
    std::fs::write(stray_dir.join("0".repeat(62)), b"orphan").unwrap();
    std::fs::write(stray_dir.join(".torpor-99.tmp"), b"half written").unwrap();

    let (thome, root) = (sim.env.thome.clone(), sim.env.proj.clone());
    let real = now_ns();
    let mut ws = Ws::open_in(&thome, &root, false).unwrap();
    ws.conn.execute("UPDATE heartbeat SET last_at = last_at - ?1", [H]).unwrap(); // the session went quiet an hour ago
    let blobs0 = count(&ws.conn, "SELECT COUNT(*) FROM blobs");

    // Nothing is unreachable yet, and everything is younger than the grace anyway.
    let GcOutcome::Done(s) = retention::gc(&mut ws, false, real + 20 * 60_000_000_000).unwrap() else { panic!("not quiet") };
    assert_eq!((s.blobs, s.orphan_files), (0, 0));
    assert_eq!(count(&ws.conn, "SELECT COUNT(*) FROM blobs"), blobs0);

    // Everything but the newest save point ages out. The old blobs are unreachable now, but still inside the grace.
    let dropped = retention::thin(&ws.conn, real + 60 * D, 30, false).unwrap();
    assert!(dropped >= 4, "dropped {dropped}");
    let GcOutcome::Done(s) = retention::gc(&mut ws, false, real + 20 * 60_000_000_000).unwrap() else { panic!("not quiet") };
    assert_eq!((s.blobs, s.orphan_files), (0, 0), "the grace period protects young blobs and young stray files");
    assert_eq!(loose_files(&sim), loose0 + 2, "no file went: the five versions and the two stray files are all still there");

    // Two days later: the dead go, the live stay.
    let GcOutcome::Done(s) = retention::gc(&mut ws, false, real + 2 * D).unwrap() else { panic!("not quiet") };
    assert!(s.blobs >= 4, "{s:?}");
    assert_eq!(s.orphan_files, 2, "the stray file and the temp file");
    assert_eq!(count(&ws.conn, "SELECT COUNT(*) FROM blobs"), blobs0 - s.blobs as i64);
    assert_eq!(loose_files(&sim), 1, "only the newest big file is left on disk");
    assert_eq!(count(&ws.conn, "SELECT COUNT(*) FROM stat_cache WHERE blob_id NOT IN (SELECT id FROM blobs)"), 0, "a cached hash never names a blob that is gone");
    drop(ws);
    sim.verify_point(sim.max_point());

    // Content whose blob was deleted comes back: the next save point stores it again.
    sim.prompt("turn 5");
    sim.write_tool("a.txt", "version 1\n");
    std::fs::write(sim.path("big.bin"), noise(2)).unwrap();
    sim.stop();
    sim.verify_last_point();
    assert_eq!(loose_files(&sim), 2);
    assert!(sim.faults().is_empty(), "faults: {:?}", sim.faults());
}

#[test]
fn gc_waits_for_a_quiet_moment() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "x\n")]);
    sim.session_start();
    sim.prompt("one");
    sim.stop();
    let (thome, root) = (sim.env.thome.clone(), sim.env.proj.clone());
    let now = now_ns();
    {
        let mut ws = Ws::open_in(&thome, &root, false).unwrap();
        assert!(matches!(retention::gc(&mut ws, false, now), Ok(GcOutcome::NotQuiet)), "a hook ran a moment ago");
        assert!(matches!(retention::gc(&mut ws, true, now), Ok(GcOutcome::NotQuiet)), "and its session is still open");
    }
    sim.session_end("other");
    let mut ws = Ws::open_in(&thome, &root, false).unwrap();
    assert!(matches!(retention::gc(&mut ws, true, now), Ok(GcOutcome::Done(_))), "--if-quiet ignores sessions that ended");
    assert!(matches!(retention::gc(&mut ws, false, now), Ok(GcOutcome::NotQuiet)), "a plain gc still waits ten minutes");
}
