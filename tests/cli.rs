//! The rest of the command line (spec section 8): log, show, init, statusline, off/on, retain, forget and the user-only guard.
mod common;
use common::*;
use serde_json::Value;
use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::Stdio;
use std::time::Duration;

/// Two finished turns with an outside change between them, then a third turn that changes a secret file
/// and has an outside change during it.
fn busy() -> Sim {
    let mut s = Sim::new();
    write_files(s.proj(), &[("a.txt", "A1\nB\n"), ("b.txt", "B1\n"), ("old.txt", "O\n"), (".env", "SECRET=1\n")]);
    s.session_start();
    s.prompt("make the login button blue and then some more words to pass sixty characters in this prompt");
    s.edit("a.txt", "A2\nB\nC\n");
    s.bash("sed -i s/B1/B2/ b.txt");
    s.bash("rm -f old.txt");
    s.stop();
    s.outside_edit("notes.md", "hello\nworld\n");
    s.outside_edit(".env", "SECRET=2\n");
    s.prompt("second");
    s.edit("a.txt", "A3\nB\nC\n");
    s.stop();
    s
}

fn lines(o: &Out) -> Vec<String> {
    o.stdout.lines().map(|l| l.to_string()).collect()
}

// ---------------------------------------------------------------- log

#[test]
fn log_lists_the_turns_newest_first_and_the_changes_between_them() {
    let s = busy();
    let o = s.cli(&["log"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let l = lines(&o);
    assert_eq!(l.len(), 4, "{l:?}");
    assert!(l[0].starts_with("  ·  today "), "{}", l[0]);
    assert!(l[0].contains("-     2 files") && l[0].ends_with("changed outside the chat: .env, notes.md"), "{}", l[0]);
    assert!(l[1].starts_with("  2  today ") && l[1].contains(" you ") && l[1].contains("1 file ") && l[1].ends_with("edits: a.txt"), "{}", l[1]);
    assert!(l[2].starts_with("  1  today ") && l[2].contains("3 files") && l[2].ends_with("edits: a.txt · shell: b.txt, old.txt"), "{}", l[2]);
    assert_eq!(l[3], "Undo only turn 2: torpor undo 2 · Back to the end of turn 1: torpor back --turn 1");
    // -n cuts the list; the hint names only what is shown.
    let l = lines(&s.cli(&["log", "-n", "2"]));
    assert_eq!(l.len(), 3, "{l:?}");
    assert_eq!(l[2], "Undo only turn 2: torpor undo 2");
    for bad in [&["log", "-n"][..], &["log", "-n", "0"], &["log", "-n", "x"], &["log", "extra"]] {
        assert_eq!(s.cli(bad).code, 2, "{bad:?}");
    }
}

#[test]
fn log_json_has_one_object_per_row() {
    let s = busy();
    let o = s.cli(&["log", "--json"]);
    assert_eq!(o.code, 0);
    let v: Vec<Value> = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(v.len(), 3);
    assert!(v[0]["turn"].is_null() && v[0]["point"].as_i64().unwrap() > 0 && v[0]["who"] == "-");
    assert_eq!(v[0]["files"][0], serde_json::json!({"class": "outside", "path": ".env"}));
    assert_eq!((v[1]["turn"].as_i64(), v[1]["preview"].as_str(), v[1]["who"].as_str()), (Some(2), Some("second"), Some("you")));
    assert!(v[1]["at_ms"].as_i64().unwrap() > 1_000_000_000_000);
    let classes: Vec<(&str, &str)> = v[2]["files"].as_array().unwrap().iter().map(|f| (f["class"].as_str().unwrap(), f["path"].as_str().unwrap())).collect();
    assert_eq!(classes, [("edit", "a.txt"), ("shell", "b.txt"), ("shell", "old.txt")]);
}

#[test]
fn log_with_nothing_recorded_says_so() {
    let s = Sim::new();
    let o = s.cli(&["log"]);
    assert_eq!((o.code, o.stdout.as_str()), (5, ""));
    assert_eq!(o.stderr, "Torpor has no save points for this folder yet.\n");
    let mut s = Sim::new();
    s.session_start();
    let o = s.cli(&["log"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "No turns yet. Torpor saves a point at both ends of every turn.\n"));
}

// ---------------------------------------------------------------- show

#[test]
fn show_names_every_file_of_a_turn() {
    let mut s = busy();
    s.prompt("third");
    s.edit(".env", "SECRET=3\n");
    s.outside_edit("notes.md", "hello\nthere\nworld\n");
    s.stop();
    let o = s.cli(&["show", "1"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let l = lines(&o);
    assert!(l[0].starts_with("Turn 1 · today ") && l[0].ends_with(" · you: \"make the login button blue and then some more words to pass…\""), "{}", l[0]);
    assert_eq!(l[1].split_whitespace().collect::<Vec<_>>(), ["edit", "a.txt", "+2", "-1"]);
    assert_eq!(l[2].split_whitespace().collect::<Vec<_>>(), ["shell", "b.txt", "+1", "-1", "(sed", "-i", "s/B1/B2/", "b.txt)"]);
    assert_eq!(l[3].split_whitespace().collect::<Vec<_>>(), ["shell", "old.txt", "deleted", "(rm", "-f", "old.txt)"]);
    let re = regex::Regex::new(r"^Save points: \d+ start · \d+ before rm -f old\.txt · \d+ end$").unwrap();
    assert!(re.is_match(&l[4]), "{}", l[4]);
    assert_eq!(l[5], "Undo only this turn: torpor undo 1");
    // A secret file is named, never shown; a change that was not the AI's says undo leaves it.
    let l = lines(&s.cli(&["show", "3"]));
    assert!(l[1].starts_with("  edit    .env") && l[1].ends_with("changed (secret file, content not shown)"), "{}", l[1]);
    assert!(l[2].starts_with("  outside notes.md") && l[2].ends_with("+1 -0 (changed during the turn, not by the AI; undo leaves it)"), "{}", l[2]);
    assert!(!lines(&s.cli(&["show", "3"])).iter().any(|x| x.contains("SECRET")));
}

#[test]
fn show_a_save_point_and_the_json_shapes() {
    let s = busy();
    let end1 = s.q_i64("SELECT end_point FROM turns WHERE n=1");
    let o = s.cli(&["show", "--id", &end1.to_string()]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let l = lines(&o);
    assert!(l[0].starts_with(&format!("Save point {end1} · end of turn 1, today ")), "{}", l[0]);
    assert_eq!(l.last().unwrap(), &format!("Go back to it: torpor back --id {end1}"));
    let v: Value = serde_json::from_str(s.cli(&["show", "1", "--json"]).stdout.trim()).unwrap();
    assert_eq!(v["turn"]["n"], 1);
    assert_eq!(v["turn"]["end_point"].as_i64(), Some(end1));
    assert!(v["point"].is_null());
    assert_eq!(v["files"][0]["path"], "a.txt");
    assert_eq!((v["files"][0]["change"].as_str(), v["files"][0]["added"].as_i64(), v["files"][0]["removed"].as_i64()), (Some("changed"), Some(2), Some(1)));
    assert_eq!(v["files"][2]["command"], "rm -f old.txt");
    assert_eq!(v["files"][2]["change"], "deleted");
    assert_eq!(v["points"][0]["kind"], "start");
    let v: Value = serde_json::from_str(s.cli(&["show", "--id", &end1.to_string(), "--json"]).stdout.trim()).unwrap();
    assert!(v["turn"].is_null());
    assert_eq!(v["point"]["id"].as_i64(), Some(end1));
}

#[test]
fn show_says_when_there_is_no_such_turn_or_point() {
    let s = busy();
    let o = s.cli(&["show", "9"]);
    assert_eq!((o.code, o.stdout.as_str(), o.stderr.as_str()), (5, "", "Torpor has no turn 9. Try: torpor log\n"));
    let o = s.cli(&["show", "--id", "999"]);
    assert_eq!((o.code, o.stderr.as_str()), (5, "Torpor has no save point 999. Try: torpor log\n"));
    for bad in [&["show"][..], &["show", "x"], &["show", "0"], &["show", "--id"], &["show", "1", "2"]] {
        assert_eq!(s.cli(bad).code, 2, "{bad:?}");
    }
}

// ---------------------------------------------------------------- init

#[test]
fn init_reports_the_first_scan_and_the_hooks() {
    let mut s = Sim::new();
    let many: Vec<(String, &str)> = (0..1200).map(|i| (format!("src/f{i}.txt"), "x\n")).collect();
    let refs: Vec<(&str, &str)> = many.iter().map(|(p, c)| (p.as_str(), *c)).collect();
    write_files(s.proj(), &refs);
    write_files(s.proj(), &[(".env", "K=1\n"), ("app.db", "db\n")]);
    std::fs::write(s.path("big.bin"), vec![1u8; 3 << 20]).unwrap();
    let o = s.cli(&["init"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let l = lines(&o);
    assert!(l[0].starts_with("Torpor is set up for this folder. First scan: 1,203 files in "), "{}", l[0]);
    assert!(l[0].ends_with(" ms (checks after every shell command)."), "{}", l[0]);
    assert_eq!(l[1], "Secret files are saved encrypted and never shown: .env.");
    assert_eq!(l[2], "Saved even if git ignores them: app.db.");
    assert_eq!(l[3], "Not saved: big.bin (bigger than 2 MB).");
    assert_eq!(l[4], "Hooks: none seen yet. If Claude Code asked whether you trust this folder, answer yes; hooks wait for that.");
    // After a hook has run, the line says so; running init again changes nothing else.
    s.session_start();
    let o = s.cli(&["init"]);
    assert!(regex::Regex::new(r"(?m)^Hooks: running \(last event \d\d:\d\d\)\.$").unwrap().is_match(&o.stdout), "{}", o.stdout);
    assert_eq!(s.cli(&["init", "extra"]).code, 2);
}

// ---------------------------------------------------------------- statusline

fn line(o: &Out) -> &str {
    assert_eq!(o.code, 0, "the status line always exits 0: {}", o.stderr);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    assert_eq!(o.stdout.matches('\n').count(), 1, "one line: {:?}", o.stdout);
    o.stdout.trim_end()
}

#[test]
fn statusline_is_one_line_in_every_state() {
    let mut s = Sim::new();
    assert_eq!(line(&s.cli(&["statusline"])), "torpor · hooks not running", "no hook has ever run here");
    s.session_start();
    s.prompt("one");
    s.edit("a.txt", "x\n");
    s.stop();
    let n = s.q_i64("SELECT COUNT(*) FROM points WHERE visible=1 AND dropped=0");
    assert_eq!(line(&s.cli(&["statusline"])), format!("torpor · {n} save points · saved just now"));
    // Claude Code's JSON names the folder; garbage on stdin changes nothing.
    assert_eq!(line(&s.cli_stdin(&["statusline"], "not json")), format!("torpor · {n} save points · saved just now"));
    let other = tempfile::tempdir().unwrap();
    let json = format!("{{\"workspace\":{{\"project_dir\":{:?}}}}}", other.path().canonicalize().unwrap());
    let o = s.cli_env_stdin(&["statusline"], &[("CLAUDE_PROJECT_DIR", "")], &json);
    assert_eq!(line(&o), "torpor · hooks not running", "the folder in the JSON has no records");
    // Problems.
    let o = s.cli_env(&["statusline"], &[("TORPOR_TEST_FREE_BYTES", "0")]);
    assert_eq!(line(&o), "torpor · paused: disk full");
    let o = s.cli_env(&["statusline"], &[("TORPOR_OFF", "1")]);
    assert_eq!(line(&o), "torpor · off");
    std::fs::create_dir_all(s.env.home.join(".claude")).unwrap();
    std::fs::write(s.env.home.join(".claude").join("settings.json"), r#"{"disableAllHooks": true}"#).unwrap();
    assert_eq!(line(&s.cli(&["statusline"])), "torpor · hooks not running");
}

#[test]
fn statusline_wraps_the_users_own_command() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("one");
    s.stop();
    let ours = s.cli(&["statusline"]).stdout.trim_end().to_string();
    // Several arguments: run directly. Its first line comes first.
    let o = s.cli_stdin(&["statusline", "--", "echo", "hi there"], "{}");
    assert_eq!(line(&o), format!("hi there · {ours}"));
    // One argument: a shell reads it, so pipes work. Only the first line is used. The command gets the same stdin.
    let o = s.cli_stdin(&["statusline", "--", "head -c 5; echo; echo second line"], "abcdefgh");
    assert_eq!(line(&o), format!("abcde · {ours}"));
    // A command that fails, prints nothing or does not exist leaves just ours.
    for cmd in [&["false"][..], &["true"], &["/nonexistent/torpor-test"]] {
        let mut args = vec!["statusline", "--"];
        args.extend(cmd);
        assert_eq!(line(&s.cli(&args)), ours, "{cmd:?}");
    }
    // One that hangs is cut off after two seconds.
    let o = s.cli(&["statusline", "--", "sleep 5"]);
    assert_eq!(line(&o), ours);
    assert!(o.elapsed > Duration::from_millis(1500) && o.elapsed < Duration::from_secs(4), "{:?}", o.elapsed);
}

// ---------------------------------------------------------------- off, on, retain

#[test]
fn off_pauses_saving_and_on_resumes_it() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("one");
    s.edit("a.txt", "1\n");
    s.stop();
    let o = s.cli(&["off"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "Torpor stopped saving this folder. Turn it back on with: torpor on\n"));
    let before = s.max_point();
    s.prompt("two");
    s.edit("a.txt", "2\n");
    s.stop();
    assert_eq!(s.max_point(), before, "nothing is saved while it is off");
    assert!(s.status().stdout.starts_with("Torpor is turned off for this folder.\n"));
    assert_eq!(line(&s.cli(&["statusline"])), "torpor · off");
    let o = s.cli(&["on"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "Torpor is saving this folder again.\n"));
    s.prompt("three");
    s.edit("a.txt", "3\n");
    s.stop();
    assert!(s.max_point() > before, "saving is back");
    assert_eq!(s.cli(&["off", "now"]).code, 2);
    // `on` where nothing was ever recorded changes nothing and makes no store.
    let fresh = Sim::new();
    let o = fresh.cli(&["on"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "Torpor was not turned off here.\n"));
    assert!(!fresh.env.thome.exists());
}

#[test]
fn retain_lengthens_the_window_and_says_when_it_does_not() {
    let mut s = Sim::new();
    s.session_start();
    let o = s.cli(&["retain", "60"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout, "Save points are now kept for 60 days (longer than Claude Code's 30).\nThe 2 GB limit per project still applies.\n");
    assert!(s.status().stdout.contains("Undo and back reach back 60 days (set with torpor retain).\n"));
    // Claude Code's own setting is longer: it wins, and the answer says so.
    std::fs::create_dir_all(s.env.home.join(".claude")).unwrap();
    std::fs::write(s.env.home.join(".claude").join("settings.json"), r#"{"cleanupPeriodDays": 90}"#).unwrap();
    let o = s.cli(&["retain", "45"]);
    assert!(o.stdout.starts_with("Claude Code already keeps chats for 90 days, which is not less than 45, so save points are kept for 90 days.\n"), "{}", o.stdout);
    assert!(s.status().stdout.contains("Undo and back reach back 90 days (your Claude Code cleanup setting).\n"));
    for bad in [&["retain"][..], &["retain", "0"], &["retain", "x"], &["retain", "5000"], &["retain", "1", "2"]] {
        assert_eq!(s.cli(bad).code, 2, "{bad:?}");
    }
}

// ---------------------------------------------------------------- the user-only guard

#[test]
fn user_only_commands_refuse_inside_claude_code() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("one");
    s.stop();
    for args in [&["off"][..], &["on"], &["retain", "60"], &["forget"], &["forget", "--all"], &["forget", "--before", "2020-01-01"]] {
        let o = s.cli_env(args, &[("CLAUDECODE", "1")]);
        assert_eq!((o.code, o.stdout.as_str()), (3, ""), "{args:?}");
        assert_eq!(o.stderr, format!("Run this in your own terminal: ~/.torpor/bin/torpor {}\n", args.join(" ")), "{args:?}");
    }
    // The text carries no control characters from the arguments.
    let o = s.cli_env(&["forget", "--before", "a\nb\u{7}"], &[("CLAUDECODE", "1")]);
    assert_eq!(o.stderr, "Run this in your own terminal: ~/.torpor/bin/torpor forget --before ab\n");
    // Nothing happened, and the commands that only read still work.
    assert!(s.status().stdout.starts_with("Torpor is saving this project.\n"));
    for args in [&["status"][..], &["log"], &["statusline"]] {
        let o = s.cli_env(args, &[("CLAUDECODE", "1")]);
        assert_eq!(o.code, 0, "{args:?}: {}", o.stderr);
    }
    // The agent may go back and undo; those are not user-only.
    for args in [&["back", "1"][..], &["undo", "1"], &["recover"]] {
        let o = s.cli_env(args, &[("CLAUDECODE", "1")]);
        assert!(!o.stderr.contains("own terminal"), "{args:?}: {}", o.stderr);
    }
}

#[test]
fn help_lists_every_command() {
    let o = Sim::new().cli(&["help"]);
    assert_eq!(o.code, 0);
    for w in ["status", "log", "show", "back", "undo", "recover", "init", "statusline", "gc", "quiet", "off | on", "retain", "forget", "version", "help"] {
        assert!(o.stdout.contains(&format!("torpor {w}")), "{w}");
    }
    assert!(o.stdout.lines().count() <= 24, "one screen");
}

// ---------------------------------------------------------------- forget

/// Run the command with its standard input on a pseudo terminal that has `typed` already typed.
fn on_terminal(mut c: std::process::Command, typed: &str) -> Out {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty fills in two new descriptors, each wrapped in an OwnedFd right away.
    let (master, slave) = unsafe {
        // macOS declares the last two as *mut, Linux as *const: null_mut fits both.
        assert_eq!(libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()), 0);
        (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
    };
    c.stdin(Stdio::from(slave)).stdout(Stdio::piped()).stderr(Stdio::piped());
    let t0 = std::time::Instant::now();
    let child = c.spawn().unwrap();
    drop(c);
    let mut keys = std::fs::File::from(master);
    keys.write_all(format!("{typed}\n").as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    Out { code: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned(), elapsed: t0.elapsed() }
}

fn forget_cmd(s: &Sim, args: &[&str]) -> std::process::Command {
    let mut c = base_cmd(&s.env);
    c.arg("forget").args(args);
    c
}

fn recorded(s: &Sim) -> bool {
    std::fs::read_dir(&s.env.thome).map(|d| d.flatten().any(|e| e.path().join("torpor.db").exists())).unwrap_or(false)
}

#[test]
fn forget_needs_a_terminal_and_the_folder_name() {
    let mut s = busy();
    let files = oracle(s.proj());
    // No terminal: refused before anything is asked.
    let o = s.cli(&["forget"]);
    assert_eq!((o.code, o.stdout.as_str()), (3, ""));
    assert_eq!(o.stderr, "torpor forget asks you to type a confirmation, so it only works in a terminal.\n");
    assert!(recorded(&s));
    // Wrong words: nothing is deleted.
    let o = on_terminal(forget_cmd(&s, &[]), "nope");
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.ends_with("Type the folder name to confirm: "), "{:?}", o.stdout);
    assert_eq!(o.stderr, "Nothing was deleted.\n");
    assert!(recorded(&s));
    // The folder name: the records go, the project does not change, and recording starts again by itself.
    let o = on_terminal(forget_cmd(&s, &[]), "proj");
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("Deleted Torpor's records of this folder. Your files were not touched.\n"), "{}", o.stdout);
    assert!(!recorded(&s));
    assert_eq!(oracle(s.proj()), files, "the project is untouched");
    assert_eq!(s.cli(&["status"]).code, 5);
    s.session_start();
    s.prompt("again");
    s.stop();
    assert!(recorded(&s));
    // Bad arguments are caught before the terminal check.
    for bad in [&["--all", "--before", "2020-01-01"][..], &["--before"], &["--before", "2026-02-31"], &["--before", "yesterday"], &["now"]] {
        assert_eq!(s.cli_args_forget(bad), 2, "{bad:?}");
    }
}

trait ForgetArgs {
    fn cli_args_forget(&self, args: &[&str]) -> i32;
}
impl ForgetArgs for Sim {
    fn cli_args_forget(&self, args: &[&str]) -> i32 {
        let mut a = vec!["forget"];
        a.extend(args);
        self.cli(&a).code
    }
}

#[test]
fn forget_all_deletes_every_folder_but_not_the_launcher_or_the_key() {
    let mut s = busy();
    // A second folder in the same store, and the files that must survive.
    let other = tempfile::tempdir().unwrap();
    let other_root = other.path().canonicalize().unwrap();
    torpor::store::Ws::open_in(&s.env.thome, &other_root, true).unwrap();
    std::fs::create_dir_all(s.env.thome.join("bin")).unwrap();
    std::fs::write(s.env.thome.join("bin").join("torpor"), "launcher").unwrap();
    std::fs::write(s.env.thome.join("faults.log"), "1 - X io x\n").unwrap();
    assert!(s.env.key.exists(), "the .env change made a key");
    let n = std::fs::read_dir(&s.env.thome).unwrap().flatten().filter(|e| e.path().join("root").is_file()).count();
    assert_eq!(n, 2);
    let o = on_terminal(forget_cmd(&s, &["--all"]), "proj");
    assert_eq!(o.code, 3, "the folder name is not the word for --all: {}", o.stdout);
    assert_eq!(std::fs::read_dir(&s.env.thome).unwrap().flatten().filter(|e| e.path().join("root").is_file()).count(), 2);
    let o = on_terminal(forget_cmd(&s, &["--all"]), "all");
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("This deletes all of Torpor's records of 2 folders."), "{}", o.stdout);
    assert!(o.stdout.contains("Deleted Torpor's records of 2 folders.\n"), "{}", o.stdout);
    assert_eq!(std::fs::read_dir(&s.env.thome).unwrap().flatten().filter(|e| e.path().join("root").is_file()).count(), 0);
    assert_eq!(std::fs::read_to_string(s.env.thome.join("bin").join("torpor")).unwrap(), "launcher");
    assert!(s.env.thome.join("FORMAT").exists());
    assert!(!s.env.thome.join("faults.log").exists());
    assert!(s.env.key.exists(), "the key file is the user's to delete");
    assert!(o.stdout.contains("Torpor's key file is still at "), "{}", o.stdout);
    // Still usable.
    s.prompt("after");
    s.stop();
    assert!(recorded(&s));
}

fn date_days_ago(days: i64) -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as libc::time_t - days as libc::time_t * 86_400;
    // SAFETY: localtime_r only writes into the zeroed tm.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

#[test]
fn forget_before_drops_old_points_and_the_text_that_names_files() {
    let s = busy();
    // Make turn 1 and everything before it 40 days old.
    let end1 = s.q_i64("SELECT end_point FROM turns WHERE n=1");
    let db = rusqlite::Connection::open(s.ws_dir().join("torpor.db")).unwrap();
    let age = 40i64 * 86_400 * 1_000_000_000;
    db.execute("UPDATE points SET created_at=created_at-?1 WHERE id<=?2", rusqlite::params![age, end1]).unwrap();
    db.execute("UPDATE turns SET started_at=started_at-?1 WHERE n=1", [age]).unwrap();
    db.execute("UPDATE tool_events SET at=at-?1 WHERE turn_id=(SELECT id FROM turns WHERE n=1)", [age]).unwrap();
    drop(db);
    let newest = s.max_point();
    let blobs = s.q_i64("SELECT COUNT(*) FROM blobs");
    let cutoff = date_days_ago(10);
    let o = on_terminal(forget_cmd(&s, &["--before", &cutoff]), "proj");
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains(&format!("This deletes the save points of ")) && o.stdout.contains(&format!("made before {cutoff}")), "{}", o.stdout);
    let last = o.stdout.lines().last().unwrap();
    assert!(last.contains(&format!("Forgot {end1} save points made before {cutoff} and freed ")) && last.ends_with("The newest save point is always kept."), "{last}");
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM points WHERE dropped=1"), end1);
    assert_eq!(s.q_i64(&format!("SELECT dropped FROM points WHERE id={newest}")), 0);
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM turns WHERE n=1 AND preview IS NULL"), 1);
    assert_eq!(s.q_str("SELECT preview FROM turns WHERE n=2").as_deref(), Some("second"));
    assert_eq!(s.q_i64(&format!("SELECT COUNT(*) FROM changes WHERE point_id<={end1}")), 0);
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM tool_events WHERE cmd_preview IS NOT NULL AND turn_id=(SELECT id FROM turns WHERE n=1)"), 0);
    assert!(s.q_i64("SELECT COUNT(*) FROM blobs") < blobs, "the old file contents are freed now");
    // What is left still works.
    let o = s.cli(&["log"]);
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("\n  2  today "), "{}", o.stdout);
}
