//! Undo with checks (spec 20): the result is built in a scratch copy first and the promises that watch the undone files run on it.
//! A promise that passes now and fails there stops the undo (exit 3; only the user can go on, with `--force`), and so does an app that starts now and not there; anything else is only reported.
mod common;
use common::app::*;
use common::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

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
    s.q_i64(&format!(
        "SELECT COUNT(*) FROM promise_runs WHERE kind='undo' AND verdict='{verdict}'"
    ))
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
    assert_eq!(
        s.max_point(),
        points,
        "a refused undo must not leave a save point"
    );
    assert_eq!(
        s.q_i64("SELECT COUNT(*) FROM restores"),
        restores,
        "a refused undo must not leave a restore row"
    );
    // The runs happened all the same: 3 fresh failures in the copy and one pass on the project as it is. None belongs to a saved version.
    assert_eq!(
        (undo_runs(&s, "FAIL"), undo_runs(&s, "PASS")),
        (3, 1),
        "{}",
        runs(&s)
    );
    assert_eq!(
        s.q_i64("SELECT COUNT(*) FROM promise_runs WHERE kind='undo' AND tree_id IS NOT NULL"),
        0
    );
    // --force goes on without running anything.
    let o = s.cli(&["undo", "2", "--force"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(
        !o.stdout.contains("Checked") && !o.stdout.contains("No checks"),
        "{}",
        o.stdout
    );
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
    assert_eq!(
        (undo_runs(&s, "PASS"), undo_runs(&s, "FAIL")),
        (1, 0),
        "{}",
        runs(&s)
    );
    s.verify_last_point();
}

#[test]
fn an_undo_no_promise_watches_is_applied_and_says_it_is_untested() {
    let mut s = promised();
    assert_eq!(
        s.q_i64("SELECT deps_json IS NOT NULL FROM promises"),
        1,
        "the dependency record is known, so an unrelated file picks nothing"
    );
    s.prompt("go");
    s.write_tool("extra.js", "module.exports = 1;\n");
    s.write_tool("NOTES.md", "notes\n");
    s.stop();
    let o = s.cli(&["undo", "4"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("\nNo checks cover these files, so the result is untested.\nDone. To undo this: torpor back --id "), "{}", o.stdout);
    assert!(!s.path("extra.js").exists() && !s.path("NOTES.md").exists());
    assert_eq!(
        s.q_i64("SELECT COUNT(*) FROM promise_runs WHERE kind='undo'"),
        0
    );
}

#[test]
fn a_promise_that_already_fails_does_not_stop_an_undo() {
    let mut s = promised();
    rewrite(&mut s, &broken_server());
    rewrite(&mut s, &(broken_server() + "// note\n"));
    let o = s.cli(&["undo", "5"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(
        o.stdout
            .contains("\nPromise #1 (login works) did not count: it does not pass now either.\n"),
        "{}",
        o.stdout
    );
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
    assert_eq!(
        s.q_i64("SELECT COUNT(*) FROM promise_runs WHERE kind='undo'"),
        0
    );
}

#[test]
fn an_undo_after_which_the_app_does_not_start_is_refused_and_force_goes_ahead() {
    // Spec 20: the same check starts the app on the project as it is now and not on the result, so the undo is the cause.
    let mut s = promised();
    rewrite(&mut s, "this is not javascript (\n");
    rewrite(&mut s, SERVER);
    let before = oracle(s.proj());
    let (points, restores) = (s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "5"]);
    assert_eq!(o.code, 3, "{}", why(&o));
    let head = "Undoing turn 5 stops the app from starting (promise #1 (login works)), so nothing was changed. To do it anyway, run this in your own terminal: torpor undo 5 --force\nStart: the app did not start (";
    assert!(o.stdout.starts_with(head), "{}", o.stdout);
    assert_eq!(oracle(s.proj()), before, "a refused undo must not write");
    assert_eq!(
        (s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores")),
        (points, restores),
        "a refused undo leaves no save point and no restore row"
    );
    // The runs happened all the same: none could run in the copy and the check passes on the project as it is.
    assert_eq!(
        (undo_runs(&s, "UNRUNNABLE") > 0, undo_runs(&s, "PASS")),
        (true, 1),
        "{}",
        runs(&s)
    );
    let o = s.cli(&["undo", "5", "--force"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(server(&s).starts_with("this is not javascript"));
}

#[test]
fn an_undo_after_which_the_app_does_not_start_either_way_is_reported_and_goes_ahead() {
    // Spec 18: a check that cannot run never blocks. The app does not start on the project as it is now either, so the undo is not the cause.
    let mut s = promised();
    rewrite(&mut s, "this is not javascript (\n");
    rewrite(&mut s, "this is not javascript (\n// note\n");
    let o = s.cli(&["undo", "5"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(
        o.stdout.contains(
            "\nPromise #1 (login works) did not count: it could not run (the app did not start"
        ),
        "{}",
        o.stdout
    );
    assert_eq!(server(&s), "this is not javascript (\n");
}

// ---------------------------------------------------------------- the checks run without the store lock (S2c-fix 7)

/// The login promise is saved. Its app waits at its start while `slow.flag` exists: it touches `$D/up`, then waits (at most 20 s) for `$D/go`.
/// `$D` is a folder outside the project where the test and the app meet.
fn slow_promised() -> (Sim, tempfile::TempDir) {
    let mut s = story();
    let d = tempfile::Builder::new()
        .prefix("torpor-meet")
        .tempdir()
        .unwrap();
    let start = r#"{"cmd":"if [ -f slow.flag ]; then touch $D/up; i=0; while [ ! -f $D/go ] && [ $i -lt 80 ]; do i=$((i+1)); sleep 0.25; done; fi; node server.js --port {port}","ready":"/login","timeout_s":40}"#;
    let env = format!(r#"{{"D":"{}"}}"#, d.path().display());
    let o = add(&mut s, &spec("login works", start, &env, LOGIN_STEPS));
    assert_eq!(o.code, 0, "{}", why(&o));
    s.stop();
    (s, d)
}

/// `torpor undo ...` running in the background. The drop guard lets a held check go on and waits for it: killing Torpor would leave its app behind.
struct Bg {
    child: Option<Child>,
    dir: PathBuf,
}

impl Bg {
    fn start(s: &Sim, args: &[&str], dir: &Path) -> Bg {
        let mut c = base_cmd(&s.env);
        c.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Bg {
            child: Some(c.spawn().unwrap()),
            dir: dir.to_path_buf(),
        }
    }

    /// Wait until the app of the check is held at its start. Panics when the undo ended first or nothing happened for 60 s.
    fn held(&mut self) {
        let t = Instant::now();
        while !self.dir.join("up").exists() {
            assert!(
                self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "the undo ended before its check started the app"
            );
            assert!(
                t.elapsed() < Duration::from_secs(60),
                "the check never started the app"
            );
            sleep(Duration::from_millis(50));
        }
    }

    /// Let the app go on and wait for the undo to end.
    fn finish(mut self) -> Out {
        fs::write(self.dir.join("go"), "x").unwrap();
        let t = Instant::now();
        let o = self.child.take().unwrap().wait_with_output().unwrap();
        Out {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            elapsed: t.elapsed(),
        }
    }
}

impl Drop for Bg {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = fs::write(self.dir.join("go"), "x");
            let _ = c.wait();
        }
    }
}

#[test]
fn hooks_go_on_while_an_undo_checks_its_result() {
    let (mut s, d) = slow_promised();
    rewrite(&mut s, &format!("// tidy\n{SERVER}"));
    s.outside_edit("slow.flag", "x"); // from now on the app waits at its start
    let mut undo = Bg::start(&s, &["undo", "4"], d.path());
    undo.held();
    // The undo is checking its result right now. Hooks must still get their lock and save what happens (this is where they used to give up).
    let mark = s.max_point();
    s.prompt("meanwhile");
    s.write_tool("NOTES.md", "notes\n");
    assert!(
        s.faults().is_empty(),
        "a hook gave up while the undo checked: {:?}",
        s.faults()
    );
    assert!(
        s.max_point() > mark,
        "the new turn got no save point while the undo checked"
    );
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM tool_events WHERE tool_name='Write' AND paths_json LIKE '%NOTES.md%'"), 1, "the edit made while the undo checked was not recorded");
    let o = undo.finish();
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(o.stdout.contains("\nChecked first in a scratch copy, still passing: #1 (login works).\nDone. To undo this: torpor back --id "), "{}", o.stdout);
    assert_eq!(server(&s), SERVER);
    assert!(s.path("NOTES.md").exists(), "the work done meanwhile stays");
    assert_eq!(undo_runs(&s, "PASS"), 1, "{}", runs(&s));
    s.verify_last_point();
    assert_store_sound(&s.env);
}

#[test]
fn an_undo_whose_plan_changed_during_its_checks_writes_nothing_and_a_second_try_goes_through() {
    let (mut s, d) = slow_promised();
    rewrite(&mut s, &format!("// tidy\n{SERVER}"));
    s.outside_edit("slow.flag", "x");
    let mut undo = Bg::start(&s, &["undo", "4"], d.path());
    undo.held();
    // Another turn adds a line at the end of the file the undo would rewrite. The merge would still be clean, but it is no longer the one that was checked.
    s.prompt("meanwhile");
    s.edit("server.js", &format!("// tidy\n{SERVER}// later\n"));
    let (now, restores) = (oracle(s.proj()), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = undo.finish();
    assert_eq!(o.code, 3, "{}", why(&o));
    assert_eq!(
        o.stdout,
        "The project changed while the checks ran, so nothing was changed. Run `torpor undo 4` again.\n"
    );
    assert_eq!(oracle(s.proj()), now, "a refused undo must not write");
    assert_eq!(
        s.q_i64("SELECT COUNT(*) FROM restores"),
        restores,
        "a refused undo must not leave a restore row"
    );
    assert_eq!(
        undo_runs(&s, "PASS"),
        1,
        "the check ran all the same: {}",
        runs(&s)
    );
    // Nothing changes during the second try.
    let o = s.cli(&["undo", "4"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert_eq!(server(&s), format!("{SERVER}// later\n"));
    assert_store_sound(&s.env);
}

#[test]
fn an_undo_checks_the_merged_file_and_not_the_one_on_disk() {
    // Turn 4 adds a flag at the top, turn 5 makes the app depend on it. Taking turn 4 out merges cleanly and leaves an app without its flag. The
    // merged bytes exist only inside the undo's transaction, which ends before the checks run.
    let mut s = promised();
    let with_flag = |body: &str| format!("const FLAG = false;\n{body}");
    rewrite(&mut s, &with_flag(SERVER));
    rewrite(
        &mut s,
        &with_flag(&SERVER.replace(
            "const BROKEN = false;",
            "const BROKEN = typeof FLAG === 'undefined' ? true : FLAG;",
        )),
    );
    let before = oracle(s.proj());
    let o = s.cli(&["undo", "4"]);
    assert_eq!(o.code, 3, "{}", why(&o));
    assert!(
        o.stdout
            .starts_with("Undoing turn 4 breaks promise #1 (login works), so nothing was changed."),
        "{}",
        o.stdout
    );
    assert_eq!(oracle(s.proj()), before);
}

#[test]
fn an_app_that_fails_only_its_first_start_does_not_stop_an_undo() {
    // S2d 1: one failed start is a hiccup; the second start on the result passes, so the undo goes ahead.
    let mut s = story();
    let d = tempfile::Builder::new()
        .prefix("torpor-meet")
        .tempdir()
        .unwrap();
    let start = r#"{"cmd":"if [ -f hiccup.flag ] && [ ! -f $D/seen ]; then touch $D/seen; exit 1; fi; node server.js --port {port}","ready":"/login","timeout_s":40}"#;
    let env = format!(r#"{{"D":"{}"}}"#, d.path().display());
    let o = add(&mut s, &spec("login works", start, &env, LOGIN_STEPS));
    assert_eq!(o.code, 0, "{}", why(&o));
    s.stop();
    rewrite(&mut s, &format!("{SERVER}\n// note\n"));
    s.outside_edit("hiccup.flag", "x");
    let o = s.cli(&["undo", "4"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(
        d.path().join("seen").exists(),
        "the first start never failed, the test checks nothing"
    );
    assert!(!o.stdout.contains("did not start"), "{}", o.stdout);
}

// ---------------------------------------------------------------- the build step (S2d 7)

/// Turn 1 writes an `app.py` that does not compile, turn 2 fixes it and the AI builds (`python3 -m compileall`, exit 0), turn 3 only adds a note.
/// No promise exists.
fn built() -> Sim {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("write the app");
    s.write_tool("app.py", "x = (\n");
    s.stop();
    s.prompt("fix the app");
    s.edit("app.py", "x = 1\n");
    assert_eq!(s.bash("python3 -m compileall -q ."), 0);
    s.stop();
    s.prompt("add a note");
    s.write_tool("notes.txt", "n\n");
    s.stop();
    s
}

#[test]
fn an_undo_after_which_the_last_build_fails_is_refused_and_force_goes_ahead() {
    let s = built();
    let o = s.cli(&["undo", "2"]);
    assert_eq!(o.code, 3, "{}", why(&o));
    assert!(o.stdout.starts_with("Undoing turn 2 stops the build `python3 -m compileall -q .` from passing, so nothing was changed."), "{}", o.stdout);
    assert_eq!(
        fs::read_to_string(s.path("app.py")).unwrap(),
        "x = 1\n",
        "a refused undo writes nothing"
    );
    assert_eq!(s.cli(&["undo", "2", "--force"]).code, 0);
    assert_eq!(fs::read_to_string(s.path("app.py")).unwrap(), "x = (\n");
}

#[test]
fn an_undo_that_keeps_the_build_passing_says_so() {
    let s = built();
    let o = s.cli(&["undo", "3"]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(
        o.stdout
            .contains("The build `python3 -m compileall -q .` still passes on the result."),
        "{}",
        o.stdout
    );
}

// ---------------------------------------------------------------- the limits (S2d 8)

#[test]
fn an_undo_whose_checks_run_past_their_seconds_goes_ahead_and_says_so() {
    let (mut s, d) = slow_promised();
    rewrite(&mut s, &format!("// tidy\n{SERVER}"));
    s.outside_edit("slow.flag", "x"); // the app waits up to 20 s at its start
    let t = Instant::now();
    let o = s.cli_env(&["undo", "4"], &[("TORPOR_TEST_CHECK_SECS", "3")]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(
        o.stdout
            .contains("1 check did not run: the 3 seconds for checks were up."),
        "{}",
        o.stdout
    );
    assert!(
        t.elapsed() < Duration::from_secs(15),
        "the undo ran {:?}",
        t.elapsed()
    );
    assert_eq!(server(&s), SERVER);
    drop(d);
}

#[test]
fn an_undo_that_finds_the_run_lock_taken_waits_its_time_and_goes_ahead() {
    let (mut s, _d) = slow_promised();
    rewrite(&mut s, &format!("// tidy\n{SERVER}"));
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(s.ws_dir().join("run.lock"))
        .unwrap();
    lock.lock().unwrap();
    let t = Instant::now();
    let o = s.cli_env(&["undo", "4"], &[("TORPOR_TEST_RUN_WAIT_MS", "400")]);
    assert_eq!(o.code, 0, "{}", why(&o));
    assert!(
        o.stdout
            .contains("Another check is running in this project, so the result is untested."),
        "{}",
        o.stdout
    );
    assert!(
        t.elapsed() < Duration::from_secs(10),
        "the undo waited {:?}",
        t.elapsed()
    );
    assert_eq!(server(&s), SERVER);
    drop(lock);
}
