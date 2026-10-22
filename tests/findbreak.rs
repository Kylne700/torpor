//! Find-break end to end (spec 19, B3 on real versions): the real binary, a simulated Claude Code session and the fixture app.
//! Needs `node`. The search maths (accuracy, runs per search, the refusal at 35%) is unit-tested in src/findbreak.rs.
mod common;
use common::*;
use serde_json::json;
use std::time::{Duration, Instant};

const SERVER: &str = include_str!("fixtures/app/server.js");

const START: &str = r#"{"cmd":"node server.js --port {port}","ready":"/login","timeout_s":30}"#;
const LOGIN_STEPS: &str = r#"[
  {"http":{"method":"POST","path":"/api/login","json":{"email":"kim@example.com"}},"expect":{"status":200},"why":"sign-in is accepted"},
  {"http":{"path":"/api/me"},"expect":{"status":200,"json_has":["email"]},"why":"after sign-in the account data shows"}]"#;

fn spec(name: &str, start: &str, env: &str, steps: &str) -> String {
    format!(r#"{{"v":1,"name":"{name}","why":"{name} for the user","start":{start},"env":{env},"steps":{steps}}}"#)
}

fn login() -> String {
    spec("login works", START, "{}", LOGIN_STEPS)
}

fn broken_server() -> String {
    SERVER.replace("const BROKEN = false;", "const BROKEN = true;")
}

fn rw(s: &Sim) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(s.ws_dir().join("torpor.db")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c
}

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What the AI does in a Bash call: Torpor sees the command before it runs (PreToolUse), then the command runs with CLAUDECODE set.
fn agent(s: &mut Sim, args: &[&str], stdin: &str) -> Out {
    s.pre_bash(&format!("torpor {}", args.join(" ")));
    s.cli_env_stdin(args, &[("CLAUDECODE", "1")], stdin)
}

fn why(o: &Out) -> String {
    format!("exit {}\nstdout: {}\nstderr: {}", o.code, o.stdout, o.stderr)
}

/// Six finished turns. Turn 4 seeds the bug; turn 7 is open and the user has just said login broke.
fn six_turns() -> Sim {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    s.write_tool("notes.md", "# notes\n");
    s.stop();
    s.prompt("add some style");
    s.write_tool("style.css", "body { margin: 0 }\n");
    s.stop();
    s.prompt("write a readme");
    s.write_tool("README.md", "# app\n");
    s.stop();
    s.prompt("tidy up the server");
    s.edit("server.js", &broken_server());
    s.stop();
    s.prompt("add a todo list");
    s.write_tool("todo.txt", "- ship it\n");
    s.stop();
    s.prompt("update the notes");
    s.edit("notes.md", "# notes\nmore\n");
    s.stop();
    s.prompt("login is broken, what changed?");
    s
}

/// The kind and turn number of the save point a promise column names, e.g. "start 4".
fn at(s: &Sim, col: &str) -> Option<String> {
    s.q_str(&format!("SELECT p.kind||' '||t.n FROM promises m JOIN points p ON p.id=m.{col} JOIN turns t ON t.id=p.turn_id"))
}

// ---------------------------------------------------------------- the search (B3 on real versions)

#[test]
fn it_finds_the_turn_that_broke_login_saves_the_check_and_can_search_again() {
    let mut s = six_turns();
    s.pre_bash("torpor find-break --check -");
    let points = s.q_i64("SELECT count(*) FROM points");
    let o = s.cli_env_stdin(&["find-break", "--check", "-"], &[("CLAUDECODE", "1")], &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("Turn 4 broke it ("), "{}", why(&o));
    assert!(o.stdout.contains("Undo only turn 4: torpor undo 4"), "{}", why(&o));
    assert!(o.stdout.contains("Promise #1 \"login works\" saved. Proof: passed ") && o.stdout.contains("failed 2 of 2"), "{}", why(&o));
    // The search looks at the folder as it is now without adding a save point.
    assert_eq!(s.q_i64("SELECT count(*) FROM points"), points);
    // The promise is the repro: good = the working version before turn 4, bad = the version turn 4 made.
    assert_eq!(s.q_str("SELECT source||' '||tier||' '||state FROM promises").as_deref(), Some("repro advisory live"));
    assert_eq!(at(&s, "good_point").as_deref(), Some("start 4"));
    assert_eq!(at(&s, "bad_point").as_deref(), Some("end 4"));
    assert_eq!(s.q_str("SELECT result||' '||culprit FROM bisects").as_deref(), Some("culprit 4"));
    assert_eq!(s.q_i64("SELECT prob > 0.9 FROM bisects"), 1, "the stored probability");
    // 6 versions: far fewer runs than a run on each one of the ~12 states of a naive scan with proof.
    let runs = s.q_i64("SELECT count(*) FROM promise_runs");
    println!("find-break over 6 turns: {runs} runs");
    assert!((10..=24).contains(&runs), "a search of 6 turns used {runs} runs");
    assert!(s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='create' AND verdict='PASS'") >= 5, "5 runs of proof on the working version");
    assert!(s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='create' AND verdict='FAIL'") >= 2, "2 runs of proof on the broken one");
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='bisect'") + s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='create'"), runs);
    // The user hears about the new promise once, at the next Stop (below).
    assert_eq!(s.q_i64("SELECT announced FROM promises"), 0);

    // The same check again finds the same turn through the stored promise, and adds no promise.
    let o = agent(&mut s, &["find-break", "--promise", "1"], "");
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("Looking for the turn that broke promise #1 \"login works\"") && o.stdout.contains("Turn 4 broke it ("), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promises"), 1);
    assert_eq!(s.q_i64("SELECT count(*) FROM bisects"), 2);
    assert_eq!(s.q_i64("SELECT count(*) FROM bisects WHERE promise_id=1"), 2);
    // The same check handed in again is that promise, not a second one.
    let o = agent(&mut s, &["find-break", "--check", "-"], &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.starts_with("This check is promise #1 already.") && o.stdout.contains("Turn 4 broke it ("), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promises"), 1);

    // A promise that does not exist.
    let o = agent(&mut s, &["find-break", "--promise", "99"], "");
    assert_eq!(o.code, 5, "{}", why(&o));

    // The lite mode names the files changed since the promise last passed, newest turn last.
    let o = agent(&mut s, &["find-break"], "");
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(
        o.stdout.starts_with("Torpor: since promise #1 last passed (turn 3) these files changed: server.js (4), todo.txt (5), notes.md (6). `torpor find-break --check -` can test which turn broke it"),
        "{}",
        why(&o)
    );

    // The undo it points at is the real one (S1): the login works again and the other turns stay.
    let stop = s.stop();
    assert!(stop.text().starts_with("Torpor: new promise #1 \"login works\" (advisory), from your words at turn 7."), "{}", stop.stdout);
    let o = s.cli(&["undo", "4"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(std::fs::read_to_string(s.path("server.js")).unwrap(), SERVER);
    assert!(s.path("todo.txt").exists() && std::fs::read_to_string(s.path("notes.md")).unwrap().contains("more"));
    assert!(s.faults().is_empty(), "{:?}", s.faults());
}

#[test]
fn a_check_that_fails_one_time_in_three_is_refused_before_any_run() {
    let mut s = six_turns();
    let o = agent(&mut s, &["find-break", "--check", "-"], &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    let (runs, bisects) = (s.q_i64("SELECT count(*) FROM promise_runs"), s.q_i64("SELECT count(*) FROM bisects"));
    rw(&s).execute("UPDATE promises SET flake_hat=0.35 WHERE id=1", []).unwrap();
    let o = agent(&mut s, &["find-break", "--promise", "1"], "");
    assert_eq!(o.code, 3, "{}", why(&o));
    assert!(o.stderr.contains("This check fails at random too often (35%) to search with. Fix the check first."), "{}", why(&o));
    assert_eq!(o.stdout, "");
    assert_eq!((s.q_i64("SELECT count(*) FROM promise_runs"), s.q_i64("SELECT count(*) FROM bisects")), (runs, bisects), "no run was made");
    // A steady check (0%) still searches.
    rw(&s).execute("UPDATE promises SET flake_hat=0.0 WHERE id=1", []).unwrap();
    let o = agent(&mut s, &["find-break", "--promise", "1"], "");
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("Turn 4 broke it ("), "{}", why(&o));
}

#[test]
fn a_change_outside_the_chat_is_named_with_the_way_back() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    s.stop();
    // Something else (an editor, a git pull) breaks the app between the turns.
    s.outside_edit("server.js", &broken_server());
    s.prompt("add some notes");
    s.write_tool("notes.md", "# notes\n");
    s.stop();
    s.prompt("login is broken, what changed?");
    let o = agent(&mut s, &["find-break", "--check", "-"], &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    let end1 = s.q_i64("SELECT p.id FROM points p JOIN turns t ON t.id=p.turn_id WHERE p.kind='end' AND t.n=1");
    assert!(o.stdout.contains("It broke between turns 1 and 2, outside the chat (server.js)."), "{}", why(&o));
    assert!(o.stdout.contains(&format!("To go back before it: torpor back --id {end1}")), "{}", why(&o));
    assert!(o.stdout.contains("Promise #1 \"login works\" saved."), "{}", why(&o));
    assert_eq!(at(&s, "good_point").as_deref(), Some("end 1"));
    assert_eq!(at(&s, "bad_point").as_deref(), Some("start 2"));
    // A gap has no turn to undo.
    assert_eq!(s.q_str("SELECT result||' '||COALESCE(culprit,'none') FROM bisects").as_deref(), Some("culprit none"));
}

#[test]
fn a_version_that_will_not_start_leaves_a_range_of_turns_and_saves_nothing() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    let k = agent(&mut s, &["keep", "login works"], "");
    assert_eq!(k.code, 0, "{}", why(&k));
    s.stop();
    s.prompt("refactor the server");
    s.edit("server.js", &format!("{SERVER}\nfunction (\n"));
    s.stop();
    s.prompt("fix that");
    s.edit("server.js", &broken_server());
    s.stop();
    s.prompt("add some notes");
    s.write_tool("notes.md", "# notes\n");
    s.stop();
    s.prompt("login is broken, what changed?");
    let o = agent(&mut s, &["find-break", "--check", "-"], &login());
    assert_eq!(o.code, 4, "{}", why(&o));
    assert!(o.stdout.contains("One of turns 2-3 broke it (") && o.stdout.contains("would not start"), "{}", why(&o));
    assert!(o.stdout.contains("Not saved as a promise: Torpor could not tell which of turns 2-3 broke it."), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promises WHERE state<>'dropped'"), 0);
    assert_eq!(s.q_str("SELECT result FROM bisects").as_deref(), Some("interval"));
    // The answer is a range, so no turn is named and no undo is offered.
    assert!(!o.stdout.contains("torpor undo"), "{}", why(&o));
}

#[test]
fn a_check_that_passes_now_has_nothing_to_find() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    s.stop();
    s.prompt("check it again");
    let o = agent(&mut s, &["find-break", "--check", "-"], &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("It works now."), "{}", why(&o));
    assert!(o.stdout.contains("Not saved as a promise: the check passes on the folder as it is now, so there is nothing to reproduce."), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promises WHERE state<>'dropped'"), 0);
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs WHERE verdict='PASS'"), 3, "three runs on the folder as it is now, no more");
    assert_eq!(s.stop().stdout, "", "the AI was told in the command; Stop does not repeat it");
}

#[test]
fn a_check_that_fails_at_random_cannot_be_searched_with() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    s.stop();
    s.prompt("login is flaky");
    // Every 3rd request in the file counter fails: of 3 runs on the folder as it is now, one fails.
    let counter = s.env.home.join("flaky-count");
    let env = json!({"FLAKY_FILE": counter}).to_string();
    let steps = r#"[{"http":{"path":"/api/flaky"},"expect":{"status":200},"why":"the endpoint answers"}]"#;
    let o = agent(&mut s, &["find-break", "--check", "-"], &spec("flaky page", START, &env, steps));
    assert_eq!(o.code, 4, "{}", why(&o));
    assert!(o.stdout.contains("It fails sometimes (1 of 3 runs failed), so Torpor cannot search with it. Make the check steadier first."), "{}", why(&o));
    assert!(o.stdout.contains("Not saved as a promise: it fails only 1 of 3 runs on the folder as it is now."), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promises WHERE state<>'dropped'"), 0);
}

// ---------------------------------------------------------------- lite mode: no check

#[test]
fn without_a_check_it_names_the_files_changed_since_the_last_works() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    s.stop();
    // No working point yet.
    s.prompt("what is going on?");
    let o = agent(&mut s, &["find-break"], "");
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(o.stdout, "Torpor has no confirmed working point in the last 50 turns. `torpor log` lists them.\n");
    s.stop();
    // The user says it works: the AI keeps that version.
    s.prompt("great, login works");
    let k = agent(&mut s, &["keep", "login works"], "");
    assert_eq!(k.code, 0, "{}", why(&k));
    // Nothing changed since: the cause is not in the folder.
    let o = agent(&mut s, &["find-break"], "");
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(o.stdout, "Torpor: the files are the same as at your last 'works' (turn 3), so the cause is outside the project folder.\n");
    s.stop();
    s.prompt("add some notes");
    s.write_tool("notes.md", "# notes\n");
    s.edit("server.js", &broken_server());
    s.stop();
    s.prompt("login is broken");
    let o = agent(&mut s, &["find-break"], "");
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(
        o.stdout,
        "Torpor: since your last 'works' (turn 3) these files changed: notes.md (4), server.js (4). `torpor find-break --check -` can test which turn broke it\n"
    );
    assert!(s.faults().is_empty(), "{:?}", s.faults());
}

#[test]
fn the_lite_mode_lists_five_names_and_counts_the_rest() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    let k = agent(&mut s, &["keep", "login works"], "");
    assert_eq!(k.code, 0, "{}", why(&k));
    s.stop();
    s.prompt("add many files");
    for n in 1..=8 {
        s.write_tool(&format!("part{n}.txt"), "x\n");
    }
    s.stop();
    s.prompt("login is broken");
    let o = agent(&mut s, &["find-break"], "");
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("part1.txt (2), part2.txt (2), part3.txt (2), part4.txt (2), part5.txt (2) and 3 more."), "{}", why(&o));
}

// ---------------------------------------------------------------- promise add --repro

#[test]
fn promise_add_repro_proves_the_check_against_the_turn_that_broke_it() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    s.stop();
    s.prompt("tidy up");
    s.edit("server.js", &broken_server());
    s.stop();
    s.prompt("login is broken");
    let o = agent(&mut s, &["promise", "add", "--repro", "-"], &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("Turn 2 broke it ("), "{}", why(&o));
    assert!(o.stdout.contains("Promise #1 \"login works\" saved. Proof: passed "), "{}", why(&o));
    assert_eq!(s.q_str("SELECT source||' '||tier||' '||state FROM promises").as_deref(), Some("repro advisory live"));
    assert_eq!(at(&s, "good_point").as_deref(), Some("start 2"));
    assert_eq!(at(&s, "bad_point").as_deref(), Some("end 2"));
    // A repro names its own versions, so the options that name one are refused.
    let o = agent(&mut s, &["promise", "add", "--repro", "--good", "1", "-"], &login());
    assert_eq!(o.code, 2, "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promises"), 1);
}

#[test]
fn promise_add_repro_in_the_background_ends_with_one_stop_note() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    s.stop();
    s.prompt("tidy up");
    s.edit("server.js", &broken_server());
    s.stop();
    s.prompt("login is broken");
    let o = agent(&mut s, &["promise", "add", "--repro", "--background", "-"], &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.starts_with("Promise #1 \"login works\" is being checked in the background."), "{}", why(&o));
    wait_for("the proof", 90, || s.q_i64("SELECT count(*) FROM promises WHERE tier<>'proposed' OR state='dropped'") == 1);
    assert_eq!(s.q_str("SELECT source||' '||tier||' '||state FROM promises").as_deref(), Some("repro advisory live"));
    assert_eq!(s.q_str("SELECT result||' '||culprit FROM bisects").as_deref(), Some("culprit 2"));
    let stop = s.stop();
    assert!(stop.text().starts_with("Torpor: new promise #1 \"login works\""), "{}", stop.stdout);
    s.prompt("thanks");
    assert_eq!(s.stop().stdout, "", "said once");
}

// ---------------------------------------------------------------- usage and refusals

#[test]
fn bad_arguments_and_bad_checks_are_refused_without_a_search() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    for args in [&["find-break", "--check"][..], &["find-break", "--promise"], &["find-break", "--promise", "abc"], &["find-break", "stuff"]] {
        let o = agent(&mut s, args, "");
        assert_eq!(o.code, 2, "{args:?}: {}", why(&o));
        assert!(o.stderr.contains("find-break"), "{}", why(&o));
    }
    // A check the linter rejects (no assertions) is refused before anything runs.
    let bad = spec("login works", START, "{}", "[]");
    let o = agent(&mut s, &["find-break", "--check", "-"], &bad);
    assert_eq!(o.code, 3, "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promises"), 0);
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs"), 0);
}
