//! Undo with checks (spec 20): the result is built in a scratch copy first and the promises that watch the undone files run on it.
//! A promise that passes now and fails there stops the undo (exit 3; only the user can go on, with `--force`); anything else is only reported.
mod common;
use common::app::*;
use common::*;
use std::fs;

/// The login promise saved in turn 3, after the AI wrote the app with the seeded bug (turn 1) and fixed it (turn 2).
fn promised() -> Sim {
    let mut s = story();
    let o = add(&mut s, &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    s.stop();
    s
}

/// One finished turn that rewrites `server.js`.
fn rewrite(s: &mut Sim, content: &str) {
    s.prompt("go");
    s.edit("server.js", content);
    s.stop();
}

fn undo_runs(s: &Sim, verdict: &str) -> i64 {
    s.q_i64(&format!("SELECT COUNT(*) FROM promise_runs WHERE kind='undo' AND verdict='{verdict}'"))
}

fn server(s: &Sim) -> String {
    fs::read_to_string(s.path("server.js")).unwrap()
}

#[test]
fn an_undo_that_breaks_a_passing_promise_changes_nothing_and_force_goes_on() {
    let s = promised();
    let before = oracle(s.proj());
    let (points, restores) = (s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    // Turn 2 fixed the login; taking it out brings the seeded bug back.
    let o = s.cli(&["undo", "2"]);
    assert_eq!(o.code, 3, "{}", why(&o));
    assert_eq!(
        o.stdout,
        "Undoing turn 2 breaks promise #1 (login works), so nothing was changed. To do it anyway, run this in your own terminal: torpor undo 2 --force\nFailing step: after sign-in the account data shows.\n"
    );
    assert_eq!(oracle(s.proj()), before, "a refused undo must not write");
    assert_eq!(s.max_point(), points, "a refused undo must not leave a save point");
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM restores"), restores, "a refused undo must not leave a restore row");
    // The runs happened all the same: 3 fresh failures in the copy and one pass on the project as it is. None belongs to a saved version.
    assert_eq!((undo_runs(&s, "FAIL"), undo_runs(&s, "PASS")), (3, 1), "{}", runs(&s));
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM promise_runs WHERE kind='undo' AND tree_id IS NOT NULL"), 0);
    // --force goes on without running anything.
    let o = s.cli(&["undo", "2", "--force"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(!o.stdout.contains("Checked") && !o.stdout.contains("No checks"), "{}", o.stdout);
    assert_eq!(server(&s), broken_server());
    assert_eq!(undo_runs(&s, "FAIL"), 3, "--force runs no check");
    s.verify_last_point();
    assert_store_sound(&s.env);
}

#[test]
fn an_undo_that_leaves_the_promise_passing_says_it_was_checked() {
    let mut s = promised();
    rewrite(&mut s, &format!("// tidy\n{SERVER}"));
    let o = s.cli(&["undo", "4"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("\nChecked first in a scratch copy, still passing: #1 (login works).\nDone. To undo this: torpor back --id "), "{}", o.stdout);
    assert_eq!(server(&s), SERVER);
    assert_eq!((undo_runs(&s, "PASS"), undo_runs(&s, "FAIL")), (1, 0), "{}", runs(&s));
    s.verify_last_point();
}

#[test]
fn an_undo_no_promise_watches_is_applied_and_says_it_is_untested() {
    let mut s = promised();
    assert_eq!(s.q_i64("SELECT deps_json IS NOT NULL FROM promises"), 1, "the dependency record is known, so an unrelated file picks nothing");
    s.prompt("go");
    s.write_tool("extra.js", "module.exports = 1;\n");
    s.write_tool("NOTES.md", "notes\n");
    s.stop();
    let o = s.cli(&["undo", "4"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("\nNo checks cover these files, so the result is untested.\nDone. To undo this: torpor back --id "), "{}", o.stdout);
    assert!(!s.path("extra.js").exists() && !s.path("NOTES.md").exists());
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM promise_runs WHERE kind='undo'"), 0);
}

#[test]
fn a_promise_that_already_fails_does_not_stop_an_undo() {
    let mut s = promised();
    rewrite(&mut s, &broken_server());
    rewrite(&mut s, &(broken_server() + "// note\n"));
    let o = s.cli(&["undo", "5"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("\nPromise #1 (login works) did not count: it does not pass now either.\n"), "{}", o.stdout);
    assert_eq!(server(&s), broken_server());
}

#[test]
fn a_dry_run_names_the_checks_and_runs_none() {
    let mut s = promised();
    rewrite(&mut s, &format!("// tidy\n{SERVER}"));
    let before = oracle(s.proj());
    let o = s.cli(&["undo", "4", "--dry-run"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("\nWould check the result first: #1 (login works) (a dry run does not run the checks).\n"), "{}", o.stdout);
    assert_eq!(oracle(s.proj()), before);
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM promise_runs WHERE kind='undo'"), 0);
}

#[test]
fn an_undo_after_which_the_app_does_not_start_is_reported_and_goes_ahead() {
    // Spec 18: a check that cannot run never blocks, whatever the reason.
    let mut s = promised();
    rewrite(&mut s, "this is not javascript (\n");
    rewrite(&mut s, SERVER);
    let o = s.cli(&["undo", "5"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("\nPromise #1 (login works) did not count: it could not run (the app did not start"), "{}", o.stdout);
    assert!(server(&s).starts_with("this is not javascript"));
}
