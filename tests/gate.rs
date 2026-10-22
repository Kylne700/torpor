//! The promise gate at Stop (spec 18, 25): what it runs, what it prints, and the one case that blocks.
//! Promotion to the blocking tier is tested in tests/promote.rs; here a promise is made blocking by hand.
mod common;
use common::app::*;
use common::*;
use serde_json::{Value, json};
use std::time::Duration;

fn set(s: &Sim, sql: &str) {
    rw(s).execute_batch(sql).unwrap();
}

/// A login promise, saved and made blocking by hand, and the turn that saved it ended.
fn blocking_story() -> Sim {
    let mut s = story();
    let o = add(&mut s, &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    set(&s, "UPDATE promises SET tier='blocking' WHERE id=1");
    s.stop();
    s
}

/// The reason of a block, None when the Stop was allowed.
fn blocked(o: &Out) -> Option<String> {
    let v: Value = o.json()?;
    v.get("decision")?;
    assert_eq!(v["decision"], "block");
    Some(v["reason"].as_str().unwrap_or("").to_string())
}

fn runs(s: &Sim, kind: &str, verdict: &str) -> i64 {
    s.q_i64(&format!("SELECT count(*) FROM promise_runs WHERE kind='{kind}' AND verdict='{verdict}'"))
}

fn last_receipt(s: &Sim) -> String {
    s.q_str("SELECT selected||'/'||ran||'/'||outcome FROM receipts ORDER BY id DESC LIMIT 1").unwrap_or_default()
}

#[test]
fn b1_a_blocking_promise_stops_a_real_regression_once_per_change() {
    let mut s = blocking_story();
    // The turn that saved the promise changed no file, so nothing was checked.
    assert_eq!(last_receipt(&s), "0/0/none");
    s.prompt("tidy up the server");
    s.edit("server.js", &broken_server());
    let o = s.stop();
    let reason = blocked(&o).unwrap_or_else(|| panic!("no block: {}", why(&o)));
    assert!(reason.starts_with("Torpor: promise #1 \"login works\" ("), "{reason}");
    assert!(reason.contains(") fails now. Failing step: after sign-in the account data shows."), "{reason}");
    assert!(reason.contains("It failed 3 of 3 fresh runs and still passes on the saved working version."), "{reason}");
    assert!(reason.contains("Changed since it last passed: server.js (turn 4)."), "{reason}");
    assert!(reason.contains("Fix the app, not the check") && reason.contains("torpor promise dispute 1 \"<why>\""), "{reason}");
    assert!(reason.split_whitespace().count() <= 100, "{} words: {reason}", reason.split_whitespace().count());
    assert_eq!((runs(&s, "gate", "FAIL"), runs(&s, "control", "PASS")), (3, 1), "3 fresh runs, then the control run on the last good version");
    assert_eq!(last_receipt(&s), "1/1/block");
    assert_eq!(s.q_i64("SELECT blocks FROM gate_chain"), 1);
    assert_eq!(s.q_str("SELECT tier||' '||state FROM promises").as_deref(), Some("blocking live"));
    // The AI stops again without changing anything: the turn is allowed to end, and the user is told.
    let o = s.stop();
    assert!(blocked(&o).is_none(), "{}", why(&o));
    assert!(o.text().contains("promise #1 (login works) still fails. The AI was stopped for it already and nothing changed since"), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT blocks FROM gate_chain"), 1);
    assert_eq!(last_receipt(&s), "0/0/unresolved");
    // It changes the file but the page is still broken: one more block is allowed, never a third.
    s.edit("server.js", &(broken_server() + "// second try\n"));
    assert!(blocked(&s.stop()).is_some(), "a changed version may be blocked once more");
    assert_eq!(s.q_i64("SELECT SUM(blocks) FROM gate_chain"), 2);
    s.edit("server.js", &(broken_server() + "// third try\n"));
    let o = s.stop();
    assert!(blocked(&o).is_none(), "{}", why(&o));
    assert!(o.text().contains("failed after this turn"), "{}", why(&o));
    assert_eq!(last_receipt(&s), "1/1/unresolved");
    assert_eq!(s.q_i64("SELECT SUM(blocks) FROM gate_chain"), 2);
    // The next prompt is a new chain: the same regression blocks again; fixing it passes quietly.
    s.prompt("try again");
    s.edit("server.js", &(broken_server() + "// next prompt\n"));
    assert!(blocked(&s.stop()).is_some());
    s.edit("server.js", SERVER);
    let o = s.stop();
    assert!(blocked(&o).is_none() && !o.text().contains("promise #1"), "{}", why(&o));
    assert_eq!(last_receipt(&s), "1/1/ok");
    assert_eq!(s.q_i64("SELECT last_pass_point=(SELECT MAX(id) FROM points WHERE visible=1) FROM promises"), 1, "the version that passes is the new good one");
    assert!(s.faults().is_empty(), "{:?}", s.faults());
}

#[test]
fn a_dispute_or_the_gate_switch_ends_the_blocking() {
    let mut s = blocking_story();
    s.prompt("tidy up the server");
    s.edit("server.js", &broken_server());
    assert!(blocked(&s.stop()).is_some());
    // The AI disputes: the promise is the user's to decide, so it is not checked any more.
    let d = agent(&mut s, &["promise", "dispute", "1", "the sign-in page was removed on purpose"], "");
    assert_eq!(d.code, 0, "{}", why(&d));
    s.edit("server.js", &(broken_server() + "// after the dispute\n"));
    let n = s.q_i64("SELECT count(*) FROM promise_runs");
    let o = s.stop();
    assert!(blocked(&o).is_none(), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs"), n, "a disputed promise is not run");
    // The user can turn the gate off: the failure is shown, nobody is stopped.
    set(&s, "UPDATE promises SET state='live' WHERE id=1");
    assert_eq!(s.cli(&["gate", "off"]).code, 0);
    s.prompt("again");
    s.edit("server.js", &(broken_server() + "// gate off\n"));
    let o = s.stop();
    assert!(blocked(&o).is_none(), "{}", why(&o));
    assert!(o.text().contains("promise #1 (login works) failed after this turn") && o.text().contains("The AI was not stopped."), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT SUM(blocks) FROM gate_chain"), 1, "only the first regression was blocked");
}

#[test]
fn an_advisory_failure_is_told_to_the_user_and_once_to_the_ai() {
    let mut s = story();
    assert_eq!(add(&mut s, &login()).code, 0);
    s.stop();
    s.prompt("add dark mode");
    s.edit("server.js", &broken_server());
    let o = s.stop();
    assert!(blocked(&o).is_none(), "{}", why(&o));
    let t = o.text();
    assert!(t.contains("Torpor: promise #1 (login works) failed after this turn (3 of 3 runs; it passed on the version from the start of turn 3). The AI was not stopped. torpor promise show 1"), "{t}");
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs WHERE kind='control'"), 0, "an advisory promise needs no control run");
    // The next prompt gives the model one short line; the one after does not repeat it.
    let p = s.prompt("what happened?");
    let ctx = p.json().and_then(|v| v.pointer("/hookSpecificOutput/additionalContext").and_then(|x| x.as_str().map(String::from))).unwrap_or_default();
    assert!(ctx.contains("Torpor: promise #1 failed after turn 4: after sign-in the account data shows. Run torpor check 1."), "{ctx}");
    let line = ctx.lines().find(|l| l.contains("promise #1 failed")).unwrap();
    assert!(line.chars().count() <= 120, "{line}");
    let p = s.prompt("and now?");
    assert!(!p.stdout.contains("promise #1 failed"), "{}", p.stdout);
}

#[test]
fn a_turn_that_changes_only_docs_and_tests_runs_nothing_20_of_20() {
    let mut s = blocking_story();
    let before = s.q_i64("SELECT count(*) FROM promise_runs");
    let receipts = s.q_i64("SELECT count(*) FROM receipts");
    for i in 0..20 {
        s.prompt(&format!("write it up {i}"));
        s.edit("README.md", &format!("# Notes {i}\n"));
        if i % 2 == 0 {
            s.edit("docs/guide.md", &format!("guide {i}\n"));
            s.edit("tests/login.test.js", &format!("// test {i}\n"));
        }
        let o = s.stop();
        assert!(blocked(&o).is_none() && !o.text().contains("promise #"), "trial {i}: {}", why(&o));
        assert_eq!(last_receipt(&s), "0/0/none", "trial {i}");
    }
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs"), before, "no check ran");
    assert_eq!(s.q_i64("SELECT count(*) FROM receipts"), receipts + 20, "one receipt per Stop");
}

#[test]
fn a_setup_failure_is_unrunnable_and_never_blocks_20_of_20() {
    let mut s = blocking_story();
    for i in 0..20 {
        s.prompt(&format!("change {i}"));
        // The app cannot start; the text differs every time so each Stop sees a new version.
        s.edit("server.js", &format!("this is not javascript {i}\n"));
        let o = s.stop();
        assert!(blocked(&o).is_none(), "trial {i}: {}", why(&o));
        assert!(last_receipt(&s).ends_with("/unrunnable"), "trial {i}: {}", last_receipt(&s));
    }
    assert_eq!((runs(&s, "gate", "FAIL"), runs(&s, "gate", "UNRUNNABLE")), (0, 20));
    assert_eq!(s.q_i64("SELECT count(*) FROM gate_chain"), 0);
    assert_eq!(s.q_str("SELECT tier||' '||state FROM promises").as_deref(), Some("blocking live"), "an unrunnable check is not a failure");
}

#[test]
fn b2_the_seeded_regression_blocks_20_of_20() {
    // 4 sessions in parallel, 5 trials each: break the page (blocked), then fix it (passes).
    let blocks: Vec<u32> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..4)
            .map(|_| {
                sc.spawn(|| {
                    let mut s = blocking_story();
                    let mut n = 0;
                    for i in 0..5 {
                        s.prompt(&format!("refactor {i}"));
                        s.edit("server.js", &(broken_server() + &format!("// {i}\n")));
                        let o = s.stop();
                        if blocked(&o).is_some() {
                            n += 1;
                        }
                        s.edit("server.js", &(SERVER.to_string() + &format!("// {i}\n")));
                        let o = s.stop();
                        assert!(blocked(&o).is_none(), "the fixed version was blocked: {}", why(&o));
                    }
                    n
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(blocks.iter().sum::<u32>(), 20, "blocks per session: {blocks:?}");
}

#[test]
fn b2_a_check_that_fails_one_run_in_three_at_random_never_blocks_20_of_20() {
    // /api/randflaky answers 500 to one request in three, with no seed: every run of this test sees different luck. 4 sessions in parallel, 5 trials
    // each: save the check (the 5-run proof sees the flakiness 87% of the time), then four turns of edits with the gate on.
    let steps = r#"[{"http":{"path":"/api/randflaky"},"expect":{"status":200},"why":"the login endpoint answers"}]"#;
    let trials: Vec<(u32, String, String)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..4)
            .map(|_| {
                sc.spawn(|| {
                    (0..5)
                        .map(|i| {
                            let mut s = story();
                            let o = add(&mut s, &spec("login endpoint answers", START, "{}", steps));
                            let saved = if o.code == 0 { s.q_str("SELECT tier||' '||state FROM promises").unwrap() } else { "not saved".to_string() };
                            s.stop();
                            let mut blocks = 0;
                            for r in 0..4 {
                                s.prompt(&format!("change {r}"));
                                s.edit("server.js", &(SERVER.to_string() + &format!("// {i} {r}\n")));
                                if blocked(&s.stop()).is_some() {
                                    blocks += 1;
                                }
                            }
                            (blocks, saved, s.q_str("SELECT tier||' '||state FROM promises").unwrap_or_default())
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let blocked_trials = trials.iter().filter(|t| t.0 > 0).count();
    let mut tally = std::collections::BTreeMap::<String, u32>::new();
    for t in &trials {
        *tally.entry(format!("{} -> {}", t.1, t.2)).or_default() += 1;
    }
    eprintln!("B2 random flaky: {blocked_trials} of {} trials blocked; saved as -> at the end: {tally:?}", trials.len());
    assert_eq!(trials.len(), 20);
    assert_eq!(blocked_trials, 0, "{tally:?}");
    // The proof sees it (P(no trial of 20 is flagged) is below 1e-15), so the claim is not empty.
    assert!(trials.iter().any(|t| t.1 == "advisory flaky"), "{tally:?}");
}

#[test]
fn an_unproven_promise_that_fails_and_then_passes_on_one_version_turns_flaky() {
    let mut s = story();
    let counter = s.env.home.join("flaky-count");
    let env = json!({"FLAKY_FILE": counter}).to_string();
    let steps = r#"[{"http":{"path":"/api/flaky"},"expect":{"status":200},"why":"the endpoint answers"}]"#;
    assert_eq!(add(&mut s, &spec("endpoint answers", START, &env, steps)).code, 0);
    // Put it where a guard promise would be: live, no earlier version fails it. The next request is the 6th, which fails.
    set(&s, "UPDATE promises SET state='live', tier='unproven', flake_hat=0 WHERE id=1");
    std::fs::write(&counter, "5").unwrap();
    s.stop();
    s.prompt("touch the server");
    s.edit("server.js", &(SERVER.to_string() + "// x\n"));
    let o = s.stop();
    assert!(blocked(&o).is_none(), "{}", why(&o));
    assert!(o.text().contains("Torpor: promise #1 (endpoint answers) passed and failed on the same version, so it is flaky and advisory only."), "{}", why(&o));
    assert_eq!(s.q_str("SELECT tier||' '||state FROM promises").as_deref(), Some("unproven flaky"));
    assert!(s.q_i64("SELECT flake_hat>0 FROM promises") == 1);
    assert_eq!(last_receipt(&s), "1/1/flaky");
    // A flaky promise is not run at the next Stop, and it never gets near blocking.
    let n = s.q_i64("SELECT count(*) FROM promise_runs");
    s.prompt("again");
    s.edit("server.js", &(SERVER.to_string() + "// y\n"));
    s.stop();
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs"), n);
    assert_ne!(s.cli(&["promise", "block", "1"]).code, 0);
}

#[test]
fn a_check_that_cannot_be_read_is_unverified_and_first_in_line_next_time() {
    let mut s = blocking_story();
    let spec_id = s.q_str("SELECT spec_id FROM promises WHERE id=1").unwrap();
    set(&s, "UPDATE promises SET spec_id='00ff00ff' WHERE id=1");
    for i in 0..3 {
        s.prompt(&format!("change {i}"));
        s.edit("server.js", &(SERVER.to_string() + &format!("// {i}\n")));
        let o = s.stop();
        assert!(blocked(&o).is_none(), "{}", why(&o));
        assert_eq!(last_receipt(&s), "1/0/unverified");
        assert_eq!(s.q_str("SELECT value FROM meta WHERE key='gate_unrun'").as_deref(), Some("[1]"));
        if i < 2 {
            assert!(!o.text().contains("cut short"), "{}", why(&o));
        } else {
            assert!(o.text().contains("Torpor: promise checks were cut short at the last 3 turn ends. torpor status"), "three unverified Stops in a row are said: {}", why(&o));
            let st = s.status().stdout;
            assert!(st.contains("The promise checks were cut short at the last 3 turn ends. Run them: torpor check --all"), "{st}");
            assert_eq!(s.cli(&["status", "--json"]).json().unwrap()["promises"]["checks_cut_short"], json!(true));
        }
    }
    // Readable again: the unverified promise runs first, even for a turn that changed only a doc.
    set(&s, &format!("UPDATE promises SET spec_id='{spec_id}' WHERE id=1"));
    s.prompt("notes");
    s.edit("README.md", "# notes\n");
    s.stop();
    assert_eq!(last_receipt(&s), "1/1/ok");
    assert_eq!(s.q_i64("SELECT count(*) FROM meta WHERE key='gate_unrun'"), 0);
}

#[test]
fn silent_gates_are_noticed_at_session_start_and_at_the_next_prompt() {
    let mut s = blocking_story();
    for i in 0..3 {
        s.prompt(&format!("change {i}"));
        s.edit("README.md", &format!("# {i}\n"));
        s.stop();
    }
    assert!(s.session_start().text().is_empty() || !s.session_start().text().contains("did not run"));
    // Something removed the receipts: the checks did not run at the last 3 turn ends.
    set(&s, "DELETE FROM receipts");
    let st = s.session_start();
    assert!(st.text().contains("Torpor's promise checks did not run at the last 3 turn ends."), "{}", why(&st));
    assert!(s.status().stdout.contains("The promise checks did not run at the last 3 turn ends. Run them: torpor check --all"));
    let p = s.prompt("hello");
    let ctx = p.json().and_then(|v| v.pointer("/hookSpecificOutput/additionalContext").and_then(|x| x.as_str().map(String::from))).unwrap_or_default();
    assert!(ctx.contains("Torpor: the promise checks did not run at the last 3 turn ends. Run torpor check --all."), "{ctx}");
}

#[test]
fn a_slow_check_is_cut_at_the_clock_and_the_stop_is_allowed() {
    let mut s = story();
    // The app only starts slowly when slow.flag exists, so saving the promise stays fast.
    let slow = r#"{"cmd":"if [ -f slow.flag ]; then sleep 40; fi; node server.js --port {port}","ready":"/login","timeout_s":60}"#;
    assert_eq!(add(&mut s, &spec("login works", slow, "{}", LOGIN_STEPS)).code, 0);
    set(&s, "UPDATE promises SET tier='blocking' WHERE id=1");
    s.stop();
    s.prompt("tidy up the server");
    s.edit("server.js", &broken_server());
    s.outside_edit("slow.flag", "x");
    let o = s.stop();
    assert!(o.elapsed < Duration::from_secs(30), "the hook took {:?}", o.elapsed);
    assert!(blocked(&o).is_none(), "{}", why(&o));
    assert_eq!(last_receipt(&s), "1/0/unverified");
    assert_eq!(s.q_str("SELECT value FROM meta WHERE key='gate_unrun'").as_deref(), Some("[1]"));
    assert_eq!(runs(&s, "gate", "FAIL"), 0, "a run cut by the clock says nothing about the check");
    assert!(s.faults().is_empty(), "{:?}", s.faults());
}

/// The PostToolUse line, None when the hook said nothing.
fn touch_line_of(o: &Out) -> Option<String> {
    let v = o.json()?;
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    Some(v["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_string())
}

#[test]
fn b8_an_edit_that_meets_a_promise_gets_one_short_line_per_promise_per_turn() {
    let mut s = story();
    for name in ["login works", "account shows"] {
        let o = add(&mut s, &spec(name, START, "{}", LOGIN_STEPS));
        assert_eq!(o.code, 0, "{}", why(&o));
    }
    s.stop();
    s.prompt("tidy up the server");
    let line = touch_line_of(&s.edit("server.js", &format!("{SERVER}\n// tidy\n"))).expect("a line for the edit");
    assert_eq!(line, "Torpor: edit touches promises #1, #2; `torpor check 1 2` before finishing");
    assert!(line.chars().count().div_ceil(4) <= 30, "at most 30 tokens: {line}");
    // Same turn, same files: nothing more. A file no promise watches: nothing.
    assert!(touch_line_of(&s.edit("server.js", &format!("{SERVER}\n// tidy again\n"))).is_none());
    assert!(touch_line_of(&s.edit("notes.txt", "x")).is_none());
    assert!(touch_line_of(&s.write_tool("README.md", "docs")).is_none());
    s.stop();
    // The next turn may say it again.
    s.prompt("one more tweak");
    assert_eq!(touch_line_of(&s.edit("server.js", &format!("{SERVER}\n// tweak\n"))).as_deref(), Some(line.as_str()));
    // A promise that is disputed or dropped is not mentioned.
    set(&s, "UPDATE promises SET state='dropped' WHERE id=1");
    s.stop();
    s.prompt("and a last one");
    assert_eq!(
        touch_line_of(&s.edit("server.js", &format!("{SERVER}\n// last\n"))).as_deref(),
        Some("Torpor: edit touches promise #2; `torpor check 2` before finishing")
    );
    assert_eq!(s.q_i64("SELECT count(*) FROM push_log WHERE kind LIKE 'touch:%' AND delivered=1 AND chars>0"), 3);
    assert!(s.faults().is_empty(), "{:?}", s.faults());
}
