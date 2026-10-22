//! Promises end to end (spec 14, 17, 24, B2, B7): the real binary, a simulated Claude Code session and the fixture app.
//! Needs `node`. Every app runs in its own process group inside Torpor's scratch folder and is stopped when its run ends.
mod common;
use common::app::*;
use common::*;
use rusqlite::params;
use serde_json::json;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

const SENTINEL: &str = "SENTINEL-4f9a1c7e2b60";

// ---------------------------------------------------------------- qualification (B2)

#[test]
fn the_seeded_regression_qualifies_a_promise_5_pass_then_2_fail() {
    let mut s = story();
    let o = add(&mut s, &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("saved. Proof: passed 5 of 5") && o.stdout.contains("failed 2 of 2"), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promises WHERE tier='advisory' AND state='live'"), 1);
    // Good = the start of the turn the user spoke in (the working version); bad = the start of the turn before (the seeded bug).
    let at = |col: &str| s.q_str(&format!("SELECT p.kind||' '||t.n FROM promises m JOIN points p ON p.id=m.{col} JOIN turns t ON t.id=p.turn_id"));
    assert_eq!(at("good_point").as_deref(), Some("start 3"));
    assert_eq!(at("bad_point").as_deref(), Some("start 2"));
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='create' AND verdict='PASS'"), 5);
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='create' AND verdict='FAIL'"), 2);
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='create' AND verdict='FAIL' AND failed_step=2"), 2, "the bug shows at the /api/me step");
    // The AI ran it in a turn that is the user's own words (the transcript line was there): praise.
    assert_eq!(s.q_str("SELECT source FROM promises").as_deref(), Some("praise"));
    assert_eq!(s.q_i64("SELECT want_source IS NULL FROM promises"), 1);
    let stop = s.stop();
    assert!(stop.text().starts_with("Torpor: new promise #1 \"login works\"") && stop.text().contains("from your words at turn 3"), "{}", stop.stdout);
    assert!(s.faults().is_empty(), "{:?}", s.faults());
    // It is said once.
    s.prompt("thanks");
    assert_eq!(s.stop().stdout, "");
}

#[test]
fn a_check_that_fails_one_run_in_three_at_random_never_qualifies() {
    let mut s = story();
    let counter = s.env.home.join("flaky-count");
    let env = json!({"FLAKY_FILE": counter}).to_string();
    let steps = r#"[{"http":{"path":"/api/flaky"},"expect":{"status":200},"why":"the endpoint answers"}]"#;
    let o = add(&mut s, &spec("endpoint answers", START, &env, steps));
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("saved but flaky: it failed 1 of 5 runs"), "{}", why(&o));
    assert_eq!(s.q_str("SELECT tier||' '||state FROM promises").as_deref(), Some("advisory flaky"));
    assert_eq!(s.q_i64("SELECT bad_point IS NULL FROM promises"), 1, "a flaky check proves nothing, so it looks for no bad version");
    assert_eq!(s.q_i64("SELECT CAST(round(flake_hat*100) AS INTEGER) FROM promises"), 20);
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='create'"), 5);
    // The counter outlives a run, so the next check lands on a failing request (the 6th) and the re-run on a good one (the 7th).
    let c = s.cli(&["check", "1"]);
    assert_eq!(c.code, 1, "{}", why(&c));
    assert!(c.stdout.contains("FLAKY: it failed once and passed on a re-run"), "{}", why(&c));
    // It can never be made to stop the AI: not by promotion, not by hand.
    let b = s.cli(&["promise", "block", "1"]);
    assert_ne!(b.code, 0, "{}", why(&b));
    assert_eq!(s.q_str("SELECT tier FROM promises").as_deref(), Some("advisory"));
}

#[test]
fn a_setup_failure_is_unrunnable_and_not_saved() {
    let mut s = story();
    let steps = r#"[{"http":{"path":"/login"},"expect":{"status":200},"why":"the page loads"}]"#;
    let o = add(&mut s, &spec("page loads", r#"{"cmd":"echo boom here; exit 3","ready":"/login","timeout_s":10}"#, "{}", steps));
    assert_eq!(o.code, 1, "{}", why(&o));
    assert!(o.stderr.contains("Not saved:") && o.stderr.contains("could not run") && o.stderr.contains("exited 3"), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promises WHERE state<>'dropped'"), 0);
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='create' AND tree_id IS NOT NULL"), 1, "one failed setup is enough to know");
    assert!(s.cli(&["promise", "list"]).stdout.contains("No promises yet."));
    // The AI was told directly, so the user is not bothered at Stop.
    assert_eq!(s.stop().stdout, "");
}

// ---------------------------------------------------------------- who said it

#[test]
fn who_said_it_decides_where_a_promise_comes_from() {
    // The user, in their own terminal.
    let s = story();
    let o = s.cli_stdin(&["promise", "add", "-"], &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(s.q_str("SELECT source FROM promises").as_deref(), Some("praise"));

    // The AI, in a turn that a task notification started: not the user's praise.
    let mut s = story();
    s.stop();
    s.prompt_with("<task-notification>login works</task-notification>", "task_notification", "typed");
    let o = add(&mut s, &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(s.q_str("SELECT source FROM promises").as_deref(), Some("agent"));
    assert_eq!(s.q_i64("SELECT want_source IS NULL FROM promises"), 1);
    let stop = s.stop();
    assert!(stop.text().contains("(advisory), proposed by the AI at turn 4."), "{}", stop.stdout);

    // The AI, before Claude Code has written the user's line to the transcript: advisory now, settled at Stop.
    let mut s = two_turns(&[]);
    let pid = s.next_pid();
    s.hook("UserPromptSubmit", json!({"prompt": "great, login works now"}));
    let o = add(&mut s, &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(s.q_str("SELECT source||' '||want_source FROM promises").as_deref(), Some("agent praise"));
    let line = json!({"type": "user", "uuid": "u-late", "promptId": pid, "turnOrigin": "human", "promptSource": "typed", "isSidechain": false});
    writeln!(std::fs::OpenOptions::new().create(true).append(true).open(&s.transcript).unwrap(), "{line}").unwrap();
    let stop = s.stop();
    assert_eq!(s.q_str("SELECT source FROM promises").as_deref(), Some("praise"));
    assert_eq!(s.q_i64("SELECT want_source IS NULL FROM promises"), 1);
    assert!(stop.text().contains("from your words at turn 3"), "{}", stop.stdout);
}

// ---------------------------------------------------------------- the check is linted before anything is stored

#[test]
fn a_bad_check_is_refused_with_the_lint_lines_and_exit_3() {
    let mut s = story();
    let no_why = login().replace("\"why\":\"sign-in is accepted\"", "\"why\":\"\"");
    let bad = [r#"{"v":1,"name":"x"}"#.to_string(), "not json at all".to_string(), no_why, "{\"v\":1}\n\u{0}".to_string(), String::new()];
    for b in &bad {
        let o = add(&mut s, b);
        assert_eq!((o.code, o.stdout.as_str()), (3, ""), "{b:?}: {}", why(&o));
        assert!(o.stderr.lines().count() >= 1 && o.stderr.lines().all(|l| l.starts_with("Rejected:")), "{b:?}: {}", why(&o));
    }
    assert_eq!(s.q_i64("SELECT count(*) FROM promises"), 0);
    assert!(!s.ws_dir().join("vault").exists() || std::fs::read_dir(s.ws_dir().join("vault")).unwrap().count() == 0, "a refused check is not stored");
    // Wrong usage is exit 2.
    assert_eq!(s.cli(&["promise", "add"]).code, 2);
    assert_eq!(s.cli_stdin(&["promise", "add", "--good", "x", "-"], &login()).code, 2);
    assert_eq!(s.cli(&["promise", "frobnicate"]).code, 2);
}

#[test]
fn a_repeated_check_and_a_fifty_first_promise_are_refused() {
    let mut s = story();
    { let o = add(&mut s, &login()); assert_eq!(o.code, 0, "{}", why(&o)); }
    // The same check again (here with the keys in another order and with other spacing) is the same promise.
    let again = format!(r#"{{ "steps": {LOGIN_STEPS}, "env": {{}}, "start": {START}, "why": "login works for the user", "name": "login works", "v": 1 }}"#);
    let o = add(&mut s, &again);
    assert_eq!((o.code, o.stderr.as_str()), (3, "Promise #1 already has this check.\n"), "{}", why(&o));
    // Fifty live promises is the limit; dropped ones do not count.
    let c = rw(&s);
    for i in 2..=50 {
        c.execute("INSERT INTO promises(id,name,source,tier,state,spec_id,created_at) VALUES(?1,?2,'agent','advisory','live',?3,1)", params![i, format!("dummy {i}"), format!("{i:064}")]).unwrap();
    }
    let other = spec("home page loads", START, "{}", r#"[{"http":{"path":"/login"},"expect":{"status":200},"why":"the page loads"}]"#);
    let o = add(&mut s, &other);
    assert_eq!((o.code, o.stderr.as_str()), (3, "Torpor keeps at most 50 promises. Drop one first: torpor promise list\n"), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promises"), 50);
    c.execute("UPDATE promises SET state='dropped' WHERE id=50", []).unwrap();
    let o = add(&mut s, &other);
    assert_eq!(o.code, 0, "{}", why(&o));
    // A rewrite replaces its old promise even at the limit, and the old one goes once the new one is saved.
    c.execute("UPDATE promises SET state='dropped' WHERE id>1 AND id<51", []).unwrap();
    for i in 2..=50 {
        c.execute("UPDATE promises SET state='live' WHERE id=?1", [i]).unwrap();
    }
    let o = agent(&mut s, &["promise", "add", "--replace", "1", "-"], &spec("login works well", START, "{}", LOGIN_STEPS));
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(s.q_str("SELECT state||' '||announced FROM promises WHERE id=1").as_deref(), Some("dropped 1"));
    assert_eq!(s.q_i64("SELECT replaces FROM promises WHERE id=52"), 1);
    // The new one keeps the words of the old one.
    assert_eq!(s.q_str("SELECT source FROM promises WHERE id=52").as_deref(), Some("praise"));
}

// ---------------------------------------------------------------- keep

#[test]
fn keep_saves_the_version_with_the_words_and_can_make_a_promise() {
    let mut s = two_turns(&[]);
    s.prompt("nice, login works");
    // The user keeps it in their own terminal: after the newest turn, nobody's changes.
    let o = s.cli(&["keep", "login works, do not touch it"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.starts_with("Kept: \"login works, do not touch it\" (save point "), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM points WHERE kind='keep' AND visible=1 AND note='login works, do not touch it'"), 1);
    assert_eq!(s.q_str("SELECT source FROM constraints WHERE text='login works, do not touch it'").as_deref(), Some("keep_user"));
    let point = s.q_i64("SELECT id FROM points WHERE kind='keep' ORDER BY id LIMIT 1");
    let shown = s.cli(&["show", "--id", &point.to_string()]).stdout;
    assert!(shown.starts_with(&format!("Save point {point} \u{b7} kept \"login works, do not touch it\", ")), "{shown}");
    // The AI keeps one too, inside its turn; it is written as the AI's.
    let o = agent(&mut s, &["keep", "signup works too"], "");
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(s.q_str("SELECT source FROM constraints WHERE text='signup works too'").as_deref(), Some("keep_agent"));
    assert!(s.cli(&["show", "3"]).stdout.contains("kept \"signup works too\""), "{}", s.cli(&["show", "3"]).stdout);
    assert_eq!(s.cli(&["keep"]).code, 2);
    // keep with a check: the kept version is the good one; the proof is found by going back through the turns.
    let o = agent(&mut s, &["keep", "login still works", "--check", "-"], &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("saved. Proof: passed 5 of 5") && o.stdout.contains("failed 2 of 2"), "{}", why(&o));
    assert_eq!(s.q_str("SELECT p.kind FROM promises m JOIN points p ON p.id=m.good_point").as_deref(), Some("keep"));
    assert_eq!(s.q_str("SELECT p.kind||' '||t.n FROM promises m JOIN points p ON p.id=m.bad_point JOIN turns t ON t.id=p.turn_id").as_deref(), Some("start 2"));
    assert!(s.faults().is_empty(), "{:?}", s.faults());
}

// ---------------------------------------------------------------- the user decides

#[test]
fn promise_decisions_are_the_users_not_the_ais() {
    let mut s = story();
    { let o = add(&mut s, &login()); assert_eq!(o.code, 0, "{}", why(&o)); }
    for args in [&["promise", "ok", "1"][..], &["promise", "drop", "1"], &["promise", "block", "1"], &["gate", "on"], &["gate", "off"], &["promise", "show", "1", "--spec"], &["promise", "show", "--specs", "1"]] {
        let o = s.cli_env(args, &[("CLAUDECODE", "1")]);
        assert_eq!((o.code, o.stdout.as_str()), (3, ""), "{args:?}");
        assert_eq!(o.stderr, format!("Run this in your own terminal: ~/.torpor/bin/torpor {}\n", args.join(" ")), "{args:?}");
        let name = if args[0] == "gate" { format!("torpor gate {}", args[1]) } else if args[2..].iter().any(|a| a.starts_with("--spec")) { "torpor promise show --spec".to_string() } else { format!("torpor promise {}", args[1]) };
        let want = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": format!("Torpor: only the user can do this, in their own terminal ({name}).")}});
        assert_eq!(s.pre_bash(&format!("torpor {}", args.join(" "))).json().unwrap(), want, "{args:?}");
        // Also behind a shell, a pipe and the installed path.
        assert_eq!(s.pre_bash(&format!("cd . && ~/.torpor/bin/torpor {} | cat", args.join(" "))).json().unwrap(), want, "{args:?}");
    }
    assert_eq!(s.q_i64("SELECT count(*) FROM promises WHERE state='live'"), 1, "nothing changed");
    assert!(s.cli_env(&["gate"], &[("CLAUDECODE", "1")]).stdout.contains("The gate is on."));
    // What only reads or only disputes is the AI's.
    for args in [&["promise", "list"][..], &["promise", "show", "1"], &["check", "1"], &["promise", "dispute", "1", "outdated"]] {
        let o = agent(&mut s, args, "");
        assert_eq!(o.code, 0, "{args:?}: {}", why(&o));
        assert!(s.pre_bash(&format!("torpor {}", args.join(" "))).json().is_none(), "{args:?}");
    }
    // The user can do all of it. The check source is theirs to read.
    let o = s.cli(&["promise", "show", "1", "--spec"]);
    assert!(o.code == 0 && o.stdout.contains("\"name\":\"login works\"") && o.stdout.contains("/api/login"), "{}", why(&o));
    assert!(s.cli(&["gate", "off"]).stdout.starts_with("The gate is off."));
    assert!(s.cli(&["gate"]).stdout.starts_with("The gate is off."));
    assert!(s.cli(&["gate", "on"]).stdout.starts_with("The gate is on."));
    assert_eq!(s.cli(&["promise", "ok", "1"]).stdout, "Promise #1 stays. The dispute was rejected.\n");
    assert_eq!(s.cli(&["promise", "ok", "1"]).stdout, "Promise #1 has no open dispute.\n");
    // The AI's own listing never shows the check source.
    let shown = agent(&mut s, &["promise", "show", "1"], "").stdout;
    assert!(!shown.contains("/api/login") && shown.contains("step 1 http: sign-in is accepted"), "{shown}");
}

#[test]
fn torpor_calls_are_noted_before_they_run_and_old_notes_go() {
    let mut s = two_turns(&[]);
    s.prompt("go");
    let pid = s.pid.clone();
    s.pre_bash("cd . && torpor keep \"works\" && torpor check --all | tail -3 ; echo torpor off");
    assert_eq!(s.q_str("SELECT group_concat(subcommand,'|') FROM (SELECT subcommand FROM cli_calls ORDER BY rowid)").as_deref(), Some("keep|check"));
    assert_eq!(s.q_i64(&format!("SELECT count(*) FROM cli_calls WHERE prompt_id='{pid}' AND session_id='{}'", s.sid)), 2);
    // A note older than an hour is cleared on the next call.
    rw(&s).execute("INSERT INTO cli_calls(session_id,prompt_id,subcommand,at) VALUES('old','old','old',1)", []).unwrap();
    assert_eq!(s.q_i64("SELECT count(*) FROM cli_calls WHERE subcommand='old'"), 1);
    s.pre_bash("torpor promise list");
    assert_eq!(s.q_i64("SELECT count(*) FROM cli_calls WHERE subcommand='old'"), 0);
    assert_eq!(s.q_i64("SELECT count(*) FROM cli_calls WHERE subcommand='promise list'"), 1);
    // Commands that merely mention torpor are not calls.
    let n = s.q_i64("SELECT count(*) FROM cli_calls");
    s.pre_bash("echo torpor keep; cat torpor.txt; git log --grep torpor");
    assert_eq!(s.q_i64("SELECT count(*) FROM cli_calls"), n);
}

// ---------------------------------------------------------------- Stop says it

#[test]
fn a_dispute_makes_the_user_decide_and_is_said_once() {
    let mut s = story();
    { let o = add(&mut s, &login()); assert_eq!(o.code, 0, "{}", why(&o)); }
    assert!(s.stop().text().starts_with("Torpor: new promise #1"));
    s.prompt("now add a signup page");
    let o = agent(&mut s, &["promise", "dispute", "1", "the login page was renamed, so this check is outdated"], "");
    assert_eq!((o.code, o.stdout.as_str()), (0, "Promise #1 is advisory until the user decides.\n"), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT json_array_length(disputes_json) FROM promises"), 1);
    // The turn changed no files, yet the user is told, with the two ways to decide.
    let stop = s.stop();
    assert_eq!(
        stop.text(),
        "Torpor: the AI disputes promise #1 (login works): the login page was renamed, so this check is outdated. Keep it: torpor promise ok 1 · Drop it: torpor promise drop 1"
    );
    s.prompt("more");
    assert_eq!(s.stop().stdout, "", "said once");
    assert!(s.cli(&["promise", "show", "1"]).stdout.contains("Open dispute: the login page was renamed"));
    // The user rejects it: the promise stays, the dispute is closed, and a second dispute is a fresh question.
    assert_eq!(s.cli(&["promise", "ok", "1"]).stdout, "Promise #1 stays. The dispute was rejected.\n");
    assert!(!s.cli(&["promise", "show", "1"]).stdout.contains("Open dispute"));
    s.prompt("again");
    assert_eq!(agent(&mut s, &["promise", "dispute", "1", "still outdated"], "").code, 0);
    assert!(s.stop().text().contains("the AI disputes promise #1 (login works): still outdated"));
    // The user drops it.
    assert_eq!(s.cli(&["promise", "drop", "1"]).stdout, "Promise #1 dropped. Its save points are no longer pinned.\n");
    assert_eq!(s.q_str("SELECT state FROM promises").as_deref(), Some("dropped"));
    assert_eq!(s.cli(&["promise", "list"]).stdout, "No promises yet.\n");
    assert!(s.cli(&["promise", "list", "--all"]).stdout.starts_with("#1 login works"), "{}", s.cli(&["promise", "list", "--all"]).stdout);
    assert_eq!(s.cli(&["promise", "show", "1"]).code, 5, "a dropped promise is gone for show");
    assert_eq!(s.cli(&["promise", "dispute", "7", "x"]).code, 5);
}

#[test]
fn a_background_check_is_said_at_the_next_stop_whether_it_was_saved_or_not() {
    let mut s = story();
    // One that works is announced like a foreground one, once it is saved.
    let o = agent(&mut s, &["promise", "add", "--background", "-"], &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.starts_with("Promise #1 \"login works\" is being checked in the background."), "{}", why(&o));
    wait_for("the background check to finish", 90, || s.q_str("SELECT tier FROM promises WHERE id=1").is_some_and(|t| t != "proposed"));
    assert_eq!(s.q_str("SELECT tier FROM promises WHERE id=1").as_deref(), Some("advisory"));
    assert!(s.stop().text().starts_with("Torpor: new promise #1 \"login works\" (advisory)"));
    // One that cannot run is dropped, and the user hears why at the next Stop (the AI was not there to hear it).
    s.prompt("and this");
    let steps = r#"[{"http":{"path":"/login"},"expect":{"status":200},"why":"the page loads"}]"#;
    let o = agent(&mut s, &["promise", "add", "--background", "-"], &spec("page loads", r#"{"cmd":"echo boom here; exit 3","ready":"/login","timeout_s":10}"#, "{}", steps));
    assert_eq!(o.code, 0, "{}", why(&o));
    wait_for("the background check to give up", 90, || s.q_str("SELECT state FROM promises WHERE id=2").as_deref() == Some("dropped"));
    let stop = s.stop();
    assert!(stop.text().starts_with("Torpor: promise #2 \"page loads\" was not saved: the check could not run"), "{}", stop.stdout);
    s.prompt("next");
    assert_eq!(s.stop().stdout, "", "said once");
}

#[test]
fn a_changed_check_file_is_told_at_session_start_and_once_at_stop() {
    let mut s = story();
    { let o = add(&mut s, &login()); assert_eq!(o.code, 0, "{}", why(&o)); }
    s.stop();
    assert_eq!(s.second_session().session_start().stdout, "", "a clean vault says nothing");
    let id = s.q_str("SELECT spec_id FROM promises").unwrap();
    let f = s.ws_dir().join("vault").join(format!("{id}.json"));
    let orig = std::fs::read(&f).unwrap();
    std::fs::remove_file(&f).unwrap();
    std::fs::write(&f, [orig.as_slice(), b" "].concat()).unwrap();
    let notice = "Torpor's promise records were changed outside Torpor (#1). Check torpor promise list.";
    let mut s2 = s.second_session();
    assert_eq!(s2.session_start().text(), notice);
    assert_eq!(s.second_session().session_start().text(), notice, "every session start tells it");
    s2.prompt("go");
    assert_eq!(s2.stop().text(), notice);
    s2.prompt("go on");
    assert_eq!(s2.stop().stdout, "", "Stop tells each mismatch once");
    // A check whose file changed does not run.
    let c = s.cli(&["check", "1"]);
    assert_eq!(c.code, 1, "{}", why(&c));
    assert!(c.stdout.contains("UNRUNNABLE"), "{}", why(&c));
    // Put it back: quiet again.
    std::fs::remove_file(&f).unwrap();
    std::fs::write(&f, &orig).unwrap();
    assert_eq!(s.second_session().session_start().stdout, "");
    assert!(s.faults().is_empty(), "{:?}", s.faults());
}

// ---------------------------------------------------------------- torpor check

#[test]
fn check_finds_the_regression_names_the_step_and_passes_again_after_the_fix() {
    let mut s = story();
    { let o = add(&mut s, &login()); assert_eq!(o.code, 0, "{}", why(&o)); }
    s.stop();
    s.prompt("add dark mode");
    s.edit("server.js", &broken_server());
    let c = s.cli(&["check", "1"]);
    assert_eq!(c.code, 1, "{}", why(&c));
    assert!(c.stdout.starts_with("Promise #1 login works: FAIL at step 2: after sign-in the account data shows ("), "{}\n{}", why(&c), runs(&s));
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='check' AND verdict='FAIL' AND failed_step=2"), 3, "3 of 3 fresh runs");
    assert!(s.cli(&["promise", "list"]).stdout.contains("failed "), "{}", s.cli(&["promise", "list"]).stdout);
    s.edit("server.js", SERVER);
    let c = s.cli(&["check", "--all"]);
    assert_eq!(c.code, 0, "{}", why(&c));
    assert!(c.stdout.starts_with("Promise #1 login works: PASS (2 checks, "), "{}", why(&c));
    assert_eq!(s.q_i64("SELECT last_pass_point IS NOT NULL FROM promises"), 1);
    assert!(s.cli(&["promise", "list"]).stdout.contains("passed "), "{}", s.cli(&["promise", "list"]).stdout);
    // Usage.
    for args in [&["check"][..], &["check", "--all", "1"], &["check", "x"]] {
        assert_eq!(s.cli(args).code, 2, "{args:?}");
    }
    assert_eq!(s.cli(&["check", "9"]).code, 5);
    assert!(s.faults().is_empty(), "{:?}", s.faults());
}

#[test]
fn status_and_the_status_line_count_the_promises() {
    let mut s = story();
    assert!(!s.status().stdout.contains("Promises:"));
    { let o = add(&mut s, &login()); assert_eq!(o.code, 0, "{}", why(&o)); }
    s.stop();
    let st = s.status().stdout;
    assert!(st.contains("Promises: 1 (1 advisory) · caught 0\n"), "{st}");
    let line = s.cli(&["statusline"]).stdout;
    assert!(line.starts_with("torpor · ") && line.contains(" · 1 promise ok · caught 0"), "{line}");
    let j = s.cli(&["status", "--json"]).json().expect("status --json");
    assert_eq!(j["promises"]["total"], json!(1), "{j}");
    // A failing promise is not "ok" any more.
    s.prompt("break it");
    s.edit("server.js", &broken_server());
    assert_eq!(s.cli(&["check", "1"]).code, 1);
    assert!(s.cli(&["statusline"]).stdout.contains(" · 0 promises ok · caught 0"), "{}", s.cli(&["statusline"]).stdout);
}

// ---------------------------------------------------------------- isolation, proxies, locks

#[test]
fn b7_the_real_dotenv_and_a_sample_never_reach_the_app() {
    // A real .env (a secret: kept encrypted under the key) and a sample file, in every saved version.
    let secret = format!("API_TOKEN={SENTINEL}\n");
    let mut s = two_turns(&[(".env", secret.as_str()), (".env.example", secret.as_str())]);
    s.prompt("login works now");
    let isolated = r#"[{"http":{"path":"/api/isolated"},"expect":{"status":200,"json_has":["isolated"]},"why":"the app sees none of the user's secrets"}]"#;
    let o = add(&mut s, &spec("app is isolated", START, "{}", isolated));
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(s.q_i64("SELECT count(*) FROM blobs WHERE kind='secret'") >= 1, "the .env was saved as a secret");
    let c = s.cli(&["check", "1"]);
    assert_eq!(c.code, 0, "{}", why(&c));
    // The control: the same app does see the sentinel when the check itself hands it over, so the check can tell.
    let leaky = spec("app is isolated, leaky", START, &json!({"MODE": SENTINEL}).to_string(), isolated);
    let o = add(&mut s, &leaky);
    assert_eq!(o.code, 1, "{}", why(&o));
    assert!(o.stderr.contains("Not saved: it fails on") && o.stderr.contains("5 of 5 runs"), "{}", why(&o));
    // Nothing Torpor printed or wrote for people holds the secret.
    let rows = s.q_str("SELECT group_concat(COALESCE(reason,''),'|') FROM promise_runs").unwrap_or_default();
    for t in [&c.stdout, &c.stderr, &o.stdout, &o.stderr, &rows, &s.cli(&["promise", "show", "1"]).stdout] {
        assert!(!t.contains(SENTINEL), "the secret leaked into: {t}");
    }
}

#[test]
fn proxy_variables_in_the_environment_do_not_reach_the_checks() {
    let mut s = story();
    s.pre_bash("torpor promise add -");
    let dead = "http://10.255.255.1:9";
    let env = [("CLAUDECODE", "1"), ("HTTP_PROXY", dead), ("http_proxy", dead), ("HTTPS_PROXY", dead), ("https_proxy", dead), ("ALL_PROXY", dead), ("all_proxy", dead), ("NO_PROXY", "")];
    let t = Instant::now();
    let o = s.cli_env_stdin(&["promise", "add", "-"], &env, &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("saved. Proof: passed 5 of 5"), "{}", why(&o));
    let c = s.cli_env(&["check", "1"], &env);
    assert_eq!(c.code, 0, "{}", why(&c));
    assert!(t.elapsed() < Duration::from_secs(60), "a proxy that swallows packets would have stalled every request");
}

#[test]
fn a_hook_is_not_held_up_while_a_check_runs() {
    let mut s = story();
    // The app only starts slowly when slow.flag exists, so saving the promise stays fast.
    let slow = r#"{"cmd":"if [ -f slow.flag ]; then sleep 4; fi; node server.js --port {port}","ready":"/login","timeout_s":30}"#;
    { let o = add(&mut s, &spec("login works", slow, "{}", LOGIN_STEPS)); assert_eq!(o.code, 0, "{}", why(&o)); }
    s.outside_edit("slow.flag", "x");
    let mut c = cmd_with(std::path::Path::new(BIN), &s.env);
    c.args(["check", "1"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let hook = s.edit("notes.txt", "an edit made while the check runs\n");
    assert!(child.try_wait().unwrap().is_none(), "the check should still be waiting for the slow app");
    assert!(hook.elapsed < Duration::from_secs(2), "the hook took {:?}", hook.elapsed);
    assert_eq!(s.q_i64("SELECT count(*) FROM tool_events WHERE tool_name='Edit' AND paths_json LIKE '%notes.txt%'"), 1, "the edit was recorded while the check ran");
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("PASS"));
    assert!(s.faults().is_empty(), "{:?}", s.faults());
}
