//! Promotion (spec 17): when a proven promise enters shadow, and when shadow becomes blocking. What the gate does with a blocking promise is in
//! tests/gate.rs; here a shadow promise is sometimes made by hand so that one thing is tested at a time.
mod common;
use common::app::*;
use common::*;
use serde_json::{Value, json};

fn set(s: &Sim, sql: &str) {
    rw(s).execute_batch(sql).unwrap();
}

/// The fixture app with a ping route: one added hunk, so undoing it line by line is what the mutants do.
fn with_ping() -> String {
    SERVER.replace("  if (url.pathname === '/api/me') {", "  if (url.pathname === '/api/ping') {\n    return send(res, 200, { pong: true });\n  }\n  if (url.pathname === '/api/me') {")
}

fn ping(why: &str) -> String {
    spec("ping works", START, "{}", &format!(r#"[{{"http":{{"path":"/api/ping"}},"expect":{{"status":200,"json_has":["pong"]}},"why":"{why}"}}]"#))
}

/// Two finished turns (the app; then the route) and a third, open, that begins with the user's `third` prompt.
fn ping_story(third: &str) -> Sim {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    s.stop();
    s.prompt("add a ping route");
    s.edit("server.js", &with_ping());
    s.stop();
    s.prompt(third);
    s
}

const PRAISE: &str = "great, the ping route works now";

fn blocked(o: &Out) -> bool {
    o.json().is_some_and(|v: Value| v.get("decision") == Some(&json!("block")))
}

fn today() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() / 86_400
}

/// A login promise saved and put in shadow by hand; its turn ended.
fn shadow_login() -> Sim {
    let mut s = story();
    { let o = add(&mut s, &login()); assert_eq!(o.code, 0, "{}", why(&o)); }
    set(&s, "UPDATE promises SET tier='shadow' WHERE id=1");
    s.stop();
    s
}

// ---------------------------------------------------------------- into shadow

#[test]
fn a_proven_check_in_the_users_words_that_notices_an_undone_fix_starts_in_shadow() {
    let mut s = ping_story(PRAISE);
    let o = add(&mut s, &ping("the ping route answers"));
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("saved. Proof: passed 5 of 5") && o.stdout.contains("In shadow for now") && o.stdout.contains("torpor promise block 1"), "{}", why(&o));
    assert_eq!(s.q_str("SELECT source||' '||tier||' '||state FROM promises").as_deref(), Some("praise shadow live"));
    let (killed, tried) = (s.q_i64("SELECT mutants_killed FROM promises"), s.q_i64("SELECT mutants_tried FROM promises"));
    assert!(killed >= 1 && tried >= killed, "killed {killed} of {tried}");
    assert!(s.q_str("SELECT watched_json FROM promises").unwrap().contains("server.js"));
    // The mutant runs are not runs of the promise: only the 5 + 2 runs of the proof are stored.
    assert_eq!(s.q_i64("SELECT count(*) FROM promise_runs"), 7);
    let show = s.cli(&["promise", "show", "1"]);
    assert!(show.stdout.contains("shadow") && show.stdout.contains("Shadow: 0 of 3 clean days") && show.stdout.contains("Broken copies: the check failed on"), "{}", why(&show));
    eprintln!("B-shadow: mutants killed {killed} of {tried}");
}

#[test]
fn a_check_in_other_words_stays_advisory_and_says_why() {
    let mut s = ping_story(PRAISE);
    let o = add(&mut s, &ping("health endpoint responds"));
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("Advisory for now") && o.stdout.contains("does not use a word of what you said"), "{}", why(&o));
    assert_eq!(s.q_str("SELECT tier FROM promises").as_deref(), Some("advisory"));
    assert_eq!(s.q_i64("SELECT mutants_tried IS NULL FROM promises"), 1, "the cheap rule fails first, so no broken copy was run");
}

#[test]
fn what_the_ai_proposes_and_praise_in_hebrew_only_stay_advisory() {
    // A turn that a task notification started: the promise is the AI's own.
    let mut s = ping_story(PRAISE);
    s.stop();
    s.prompt_with("<task-notification>the ping route works</task-notification>", "task_notification", "typed");
    let o = add(&mut s, &ping("the ping route answers"));
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("because it came from the AI"), "{}", why(&o));
    assert_eq!(s.q_str("SELECT source||' '||tier FROM promises").as_deref(), Some("agent advisory"));

    let mut s = ping_story("\u{05DE}\u{05E2}\u{05D5}\u{05DC}\u{05D4}, \u{05D4}\u{05E0}\u{05EA}\u{05D9}\u{05D1} \u{05E2}\u{05D5}\u{05D1}\u{05D3}");
    let o = add(&mut s, &ping("\u{05D4}\u{05E0}\u{05EA}\u{05D9}\u{05D1} \u{05E2}\u{05D5}\u{05D1}\u{05D3}"));
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("only in Hebrew"), "{}", why(&o));
    assert_eq!(s.q_str("SELECT source||' '||tier FROM promises").as_deref(), Some("praise advisory"));
}

#[test]
fn shadow_given_on_provisional_words_is_taken_back_when_stop_finds_the_turn_was_not_human() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", SERVER);
    s.stop();
    s.prompt("add a ping route");
    s.edit("server.js", &with_ping());
    s.stop();
    // The user's line is not in the transcript yet, so the source is provisional.
    let pid = s.next_pid();
    s.hook("UserPromptSubmit", json!({"prompt": PRAISE}));
    let o = add(&mut s, &ping("the ping route answers"));
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(s.q_str("SELECT source||' '||want_source||' '||tier FROM promises").as_deref(), Some("agent praise shadow"));
    // Then the transcript says it was a task notification.
    s.transcript_line(&json!({"type": "user", "uuid": "u-late", "promptId": pid, "turnOrigin": "task_notification", "promptSource": "typed", "isSidechain": false}));
    s.stop();
    assert_eq!(s.q_str("SELECT source||' '||tier FROM promises").as_deref(), Some("agent advisory"));
    assert_eq!(s.q_i64("SELECT want_source IS NULL FROM promises"), 1);
}

// ---------------------------------------------------------------- out of shadow

#[test]
fn a_would_block_that_is_fixed_is_a_true_catch_and_the_promise_starts_to_block() {
    let mut s = ping_story(PRAISE);
    { let o = add(&mut s, &ping("the ping route answers")); assert_eq!(o.code, 0, "{}", why(&o)); }
    assert_eq!(s.q_str("SELECT tier FROM promises").as_deref(), Some("shadow"));
    s.stop();
    // The AI takes the route away: it would have been stopped, and was not.
    s.prompt("remove the ping route for now");
    s.edit("server.js", SERVER);
    let o = s.stop();
    assert!(!blocked(&o), "{}", why(&o));
    assert!(o.text().contains("It would have stopped the AI, but it is in shadow mode."), "{}", why(&o));
    assert_eq!(s.q_i64("SELECT count(*) FROM meta WHERE key='would:1'"), 1);
    assert_eq!(s.q_str("SELECT tier FROM promises").as_deref(), Some("shadow"));
    // The AI puts it back: the failure turned into a PASS on another version.
    s.prompt("put the ping route back");
    s.edit("server.js", &(with_ping() + "// back\n"));
    let o = s.stop();
    assert!(o.text().contains("stops the AI from now on"), "{}", why(&o));
    assert_eq!(s.q_str("SELECT tier||' '||catches FROM promises").as_deref(), Some("blocking 1"));
    assert_eq!(s.q_i64("SELECT count(*) FROM meta WHERE key LIKE 'would:%' OR key LIKE 'shadow_day:%'"), 0);
    // And the next regression is stopped for real.
    s.prompt("tidy up");
    s.edit("server.js", SERVER);
    assert!(blocked(&s.stop()), "a promoted promise blocks");
}

#[test]
fn a_failure_that_is_never_fixed_is_not_a_catch_and_does_not_count_a_day() {
    let mut s = shadow_login();
    s.prompt("tidy up");
    s.edit("server.js", &broken_server());
    let o = s.stop();
    assert!(o.text().contains("It would have stopped the AI, but it is in shadow mode."), "{}", why(&o));
    // Still failing on the next version: still shadow, no clean day.
    s.prompt("more tidying");
    s.edit("server.js", &(broken_server() + "// more\n"));
    s.stop();
    assert_eq!(s.q_str("SELECT tier||' '||shadow_days||' '||catches FROM promises").as_deref(), Some("shadow 0 0"));
    assert_eq!(s.q_i64("SELECT count(*) FROM meta WHERE key='would:1'"), 1);
}

#[test]
fn three_clean_days_start_the_blocking_and_one_day_counts_once() {
    let mut s = shadow_login();
    for (i, round) in ["a", "b"].iter().enumerate() {
        s.prompt(&format!("tidy {round}"));
        s.edit("server.js", &(SERVER.to_string() + &format!("// {round}\n")));
        s.stop();
        assert_eq!(s.q_i64("SELECT shadow_days FROM promises"), 1, "round {i}: two passes on one day are one day");
    }
    // Two clean days gone by.
    set(&s, &format!("UPDATE promises SET shadow_days=2 WHERE id=1; UPDATE meta SET value='{}' WHERE key='shadow_day:1'", today() - 1));
    s.prompt("tidy c");
    s.edit("server.js", &(SERVER.to_string() + "// c\n"));
    let o = s.stop();
    assert!(o.text().contains("passed on 3 different days"), "{}", why(&o));
    assert_eq!(s.q_str("SELECT tier||' '||catches FROM promises").as_deref(), Some("blocking 0"));
}

#[test]
fn a_dispute_or_a_drop_clears_the_wait() {
    for drop in [false, true] {
        let mut s = shadow_login();
        set(&s, &format!("UPDATE promises SET shadow_days=2 WHERE id=1; INSERT OR REPLACE INTO meta(key,value) VALUES('would:1','00'),('shadow_day:1','{}')", today() - 1));
        let o = if drop { s.cli(&["promise", "drop", "1"]) } else { agent(&mut s, &["promise", "dispute", "1", "outdated"], "") };
        assert_eq!(o.code, 0, "{}", why(&o));
        assert_eq!(s.q_i64("SELECT shadow_days FROM promises"), 0, "drop {drop}");
        assert_eq!(s.q_i64("SELECT count(*) FROM meta WHERE key IN ('would:1','shadow_day:1')"), 0, "drop {drop}");
    }
}

#[test]
fn promise_block_by_the_user_skips_only_the_wait() {
    let mut s = shadow_login();
    // The AI cannot do it.
    let o = s.cli_env(&["promise", "block", "1"], &[("CLAUDECODE", "1")]);
    assert_eq!(o.code, 3, "{}", why(&o));
    assert_eq!(s.q_str("SELECT tier FROM promises").as_deref(), Some("shadow"));
    // A promise that is not in shadow stays as it is.
    set(&s, "UPDATE promises SET tier='advisory' WHERE id=1");
    let o = s.cli(&["promise", "block", "1"]);
    assert_eq!(o.code, 1, "{}", why(&o));
    assert!(o.stderr.contains("only a promise in shadow can be made blocking"), "{}", why(&o));
    // Not while it is flaky, or disputed.
    set(&s, "UPDATE promises SET tier='shadow', state='flaky' WHERE id=1");
    let o = s.cli(&["promise", "block", "1"]);
    assert_eq!(o.code, 1, "{}", why(&o));
    assert_eq!(s.q_str("SELECT tier FROM promises").as_deref(), Some("shadow"));
    set(&s, "UPDATE promises SET state='live', flake_hat=0 WHERE id=1");
    let o = s.cli(&["promise", "block", "1"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("now stops the AI"), "{}", why(&o));
    assert_eq!(s.q_str("SELECT tier||' '||catches FROM promises").as_deref(), Some("blocking 0"));
    // It stops a real regression from the next turn on.
    s.prompt("tidy up");
    s.edit("server.js", &broken_server());
    assert!(blocked(&s.stop()));
}
