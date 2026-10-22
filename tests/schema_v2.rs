//! Schema v2 (spec 13): the move from a store made by the shipped 2.0.0 binary, the fresh store, texts, and the promise points kept by thinning.
mod common;
use common::*;
use rusqlite::{params, Connection};
use serde_json::Value;

const NEW_TABLES: [&str; 9] = ["texts", "constraints", "promises", "promise_runs", "receipts", "gate_chain", "push_log", "bisects", "cli_calls"];

fn user_version(sim: &Sim) -> i64 {
    sim.q_i64("PRAGMA user_version")
}

fn rw(sim: &Sim) -> Connection {
    let c = Connection::open(sim.ws_dir().join("torpor.db")).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c
}

/// The v1 fixture (made by the shipped 2.0.0 binary: three turns writing a.txt, src/b.txt, a.txt) replaces this project's database.
fn install_v1_fixture(sim: &Sim) {
    let dir = sim.ws_dir();
    for f in ["torpor.db", "torpor.db-wal", "torpor.db-shm"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    std::fs::copy(manifest_dir().join("tests/fixtures/v1-store/torpor.db"), dir.join("torpor.db")).unwrap();
    assert_eq!(user_version(sim), 1, "the fixture is a v1 store");
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM sqlite_master WHERE name='texts'"), 0);
}

fn assert_v2(sim: &Sim) {
    assert_eq!(user_version(sim), 2);
    for t in NEW_TABLES {
        assert_eq!(sim.q_i64(&format!("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='{t}'")), 1, "table {t}");
    }
}

#[test]
fn a_v1_store_migrates_on_the_command_line_and_old_points_still_restore() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A3\n"), ("src/b.txt", "B1\n")]);
    sim.session_start();
    install_v1_fixture(&sim);

    // The command line opens without `create`: it must still migrate, not report a missing store.
    let o = sim.cli(&["log", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(o.stdout.trim()).unwrap();
    let turns: Vec<i64> = rows.iter().filter_map(|r| r["turn"].as_i64()).collect();
    assert_eq!(turns, [3, 2, 1]);
    assert_v2(&sim);
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM points"), 6, "v1 rows untouched");
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM blobs"), 10);

    let o = sim.cli(&["back", "--turn", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(std::fs::read_to_string(sim.path("a.txt")).unwrap(), "A2\n");
    assert!(!sim.path("src/b.txt").exists(), "turn 2 added it; turn 1 did not have it");
}

#[test]
fn a_hook_migrates_a_v1_store_and_the_numbering_goes_on() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A3\n"), ("src/b.txt", "B1\n")]);
    sim.session_start();
    install_v1_fixture(&sim);
    sim.prompt("a fourth turn with password=Xk9#mP2$vLq8Zw in it");
    sim.edit("a.txt", "A4\n");
    sim.stop();
    assert_v2(&sim);
    assert_eq!(sim.q_i64("SELECT MAX(n) FROM turns"), 4);
    assert_eq!(sim.q_str("SELECT prompt FROM texts").as_deref(), Some("a fourth turn with password=[secret] in it"));
    assert_eq!(sim.q_i64("SELECT redactions FROM texts"), 1);
    assert_eq!(sim.q_str("SELECT origin FROM texts").as_deref(), Some("human"), "Stop sets the origin from the transcript");
    // And the plain case again: a second open of a v2 store changes nothing.
    sim.prompt("fifth");
    sim.stop();
    assert_v2(&sim);
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM texts"), 2);
}

#[test]
fn a_fresh_store_is_v2_and_a_task_notification_has_no_text() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A1\n")]);
    sim.session_start();
    sim.prompt("hello there");
    sim.stop();
    assert_v2(&sim);
    assert_eq!(sim.q_str("SELECT prompt FROM texts").as_deref(), Some("hello there"));
    // A background task's output is tool output: it is never stored.
    sim.prompt_with("<task-notification>build log: token=Xk9mP2vLq8Zw3N</task-notification>", "task_notification", "typed");
    sim.stop();
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM texts"), 1);
}

#[test]
fn prompt_history_off_means_no_texts_and_status_says_so() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A1\n")]);
    sim.extra_env.push(("CLAUDE_CODE_SKIP_PROMPT_HISTORY".into(), "1".into()));
    sim.session_start();
    sim.prompt("no record of these words");
    sim.stop();
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM texts"), 0);
    let st = sim.status();
    assert!(st.stdout.contains("keeps no prompt text because CLAUDE_CODE_SKIP_PROMPT_HISTORY is set"), "{}", st.stdout);
}

/// Spec 13: thinning and GC keep every point a live promise names inside W; the prompt text goes at W.
#[test]
fn thinning_keeps_promise_points_inside_the_window_and_nulls_old_texts() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A3\n"), ("src/b.txt", "B1\n")]);
    sim.session_start();
    install_v1_fixture(&sim);
    sim.cli(&["log"]); // migrates
    sim.prompt("a new turn");
    sim.stop();
    let c = rw(&sim);
    let now = torpor::util::now_ns();
    let day = torpor::retention::DAY_NS;
    // Every old point sits on one UTC day 20 days back (inside W = 30), one hour apart.
    let base = (now / day - 20) * day;
    let old: Vec<i64> = c.prepare("SELECT id FROM points WHERE id<=6 ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
    for (i, id) in old.iter().enumerate() {
        c.execute("UPDATE points SET created_at=?2 WHERE id=?1", params![id, base + i as i64 * 3_600_000_000_000]).unwrap();
    }
    c.execute("UPDATE turns SET started_at=?1 WHERE n=1", [now - 40 * day]).unwrap();
    c.execute("INSERT INTO texts(turn_id, prompt) VALUES((SELECT id FROM turns WHERE n=1), 'old words')", []).unwrap();
    let keep = old[1];
    c.execute("INSERT INTO promises(name, source, spec_id, good_point) VALUES('login works', 'praise', 'abc', ?1)", [keep]).unwrap();

    torpor::retention::thin(&c, now, 30, true).unwrap();
    let alive: Vec<i64> = c.prepare("SELECT id FROM points WHERE dropped=0 AND id<=6 ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
    assert!(alive.contains(&keep), "the promise's good point stays: {alive:?}");
    assert!(alive.len() <= 2, "the others of that day are thinned: {alive:?}");
    let texts: Vec<Option<String>> = c.prepare("SELECT prompt FROM texts ORDER BY turn_id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
    assert_eq!(texts.iter().filter(|t| t.is_none()).count(), 1, "the 40-day-old text is gone, the new one stays: {texts:?}");

    // A dropped promise no longer holds its points.
    c.execute("UPDATE promises SET state='dropped'", []).unwrap();
    torpor::retention::thin(&c, now, 30, true).unwrap();
    assert_eq!(c.query_row("SELECT dropped FROM points WHERE id=?1", [keep], |r| r.get::<_, i64>(0)).unwrap(), 1);
}
