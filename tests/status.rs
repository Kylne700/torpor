//! `torpor status` (spec section 8): the plain-words health report and the places problems come from.
//! Source 1 (faults) is also covered by A4; source 4 (settings files) is S1b's.
mod common;
use common::*;
use serde_json::Value;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, SystemTime};

fn json(sim: &Sim) -> Value {
    let o = sim.cli(&["status", "--json"]);
    assert_eq!(o.code, 0, "status --json: {}", o.stderr);
    serde_json::from_str(o.stdout.trim()).unwrap_or_else(|e| panic!("status --json is not JSON ({e}): {:?}", o.stdout))
}

fn problems(v: &Value) -> Vec<String> {
    v["problems"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect()
}

fn claude_dir(sim: &Sim) -> std::path::PathBuf {
    sim.env.home.join(".claude")
}

#[test]
fn a_healthy_folder_reads_well() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("src/a.rs", "fn a() {}\n"), ("notes.txt", "hi\n")]);
    std::fs::write(sim.path("video.mp4"), vec![7u8; 3 << 20]).unwrap();
    sim.session_start();
    sim.prompt("make the login button blue");
    sim.edit("src/a.rs", "fn a() { 1 }\n");
    sim.bash("echo more >> notes.txt");
    sim.stop();

    let s = sim.cli(&["status"]);
    assert_eq!(s.code, 0);
    let t = s.stdout;
    for want in [
        "Torpor is saving this project.\n",
        "Save points: ",
        ", last one ",
        "Last turn: 1 \"make the login button blue\" changed 2 files (1 edit, 1 shell command).\n",
        "Checks: ",
        "Not saved: video.mp4 (bigger than 2 MB).\n",
        "Problems: none.\n",
        "Save points are kept for 30 days (your Claude Code cleanup setting).\n",
    ] {
        assert!(t.contains(want), "status lacks {want:?}; it said:\n{t}");
    }
    assert!(t.lines().all(|l| l.len() < 200), "lines are short enough to read");

    let v = json(&sim);
    assert_eq!(v["saving"], true);
    assert_eq!(v["off"], false);
    assert!(v["points"].as_i64().unwrap() >= 2);
    assert_eq!(v["last_turn"]["n"], 1);
    assert_eq!(v["last_turn"]["edits"], 1);
    assert_eq!(v["last_turn"]["shell"], 1);
    assert_eq!(v["not_saved_total"], 1);
    assert_eq!(v["retain_days"], 30);
    assert!(problems(&v).is_empty());

    // Turned off: it says so and stops claiming to save.
    sim.extra_env.push(("TORPOR_OFF".into(), "1".into()));
    let off = sim.cli(&["status"]);
    sim.extra_env.clear();
    assert!(off.stdout.starts_with("Torpor is turned off for this folder.\n"), "{}", off.stdout);
}

#[test]
fn the_not_saved_list_is_short_unless_asked_for() {
    let mut sim = Sim::new();
    for i in 0..7 {
        let name = [b"bad".as_slice(), &[0xFF], format!("{i}.txt").as_bytes()].concat();
        std::fs::write(sim.proj().join(std::ffi::OsStr::from_bytes(&name)), "x").unwrap();
    }
    write_files(sim.proj(), &[("ok.txt", "fine\n")]);
    sim.session_start();
    sim.prompt("one");
    sim.stop();
    let short = sim.cli(&["status"]).stdout;
    assert!(short.contains("(its name is not valid text)"), "{short}");
    assert!(short.contains(" and 2 more: torpor status --all.\n"), "{short}");
    let all = sim.cli(&["status", "--all"]).stdout;
    assert!(!all.contains(" more: torpor status --all"), "{all}");
    assert_eq!(all.matches("(its name is not valid text)").count(), 7, "{all}");
    let v = json(&sim);
    assert_eq!(v["not_saved_total"], 7);
    assert_eq!(v["not_saved"].as_array().unwrap().len(), 5);
}

#[test]
fn a_fault_is_shown_once() {
    let mut sim = Sim::new();
    sim.session_start();
    sim.next_pid();
    sim.extra_env.push(("TORPOR_TEST_PANIC".into(), "1".into()));
    let o = sim.hook_unchecked("UserPromptSubmit", serde_json::json!({"prompt": "will fail"}));
    sim.extra_env.clear();
    assert_eq!(o.code, 0);
    let first = problems(&json(&sim));
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(first[0].starts_with("A Torpor hook crashed during UserPromptSubmit at "), "{first:?}");
    assert!(first[0].ends_with("Nothing was changed."), "{first:?}");
    assert!(problems(&json(&sim)).is_empty(), "a fault that was shown must not come back");
    let text = sim.cli(&["status"]).stdout;
    assert!(text.contains("Problems: none."), "{text}");
}

#[test]
fn the_disk_being_full_is_a_blocking_problem() {
    let mut sim = Sim::new();
    sim.session_start();
    sim.extra_env.push(("TORPOR_TEST_FREE_BYTES".into(), "800000000".into()));
    let v = json(&sim);
    let text = sim.cli(&["status"]).stdout;
    sim.extra_env.clear();
    assert_eq!(v["saving"], false);
    assert!(problems(&v).iter().any(|p| p == "Save points are paused: the disk is almost full (0.8 GB free)."), "{:?}", problems(&v));
    assert!(text.starts_with("Torpor is not saving this project right now.\n"), "{text}");
}

#[test]
fn a_folder_where_no_hook_ever_ran_says_why() {
    let sim = Sim::new();
    let o = sim.cli(&["status"]);
    assert_eq!(o.code, 5);
    assert!(o.stdout.starts_with("Torpor has no save points for this folder yet.\nProblems:\n"), "{}", o.stdout);
    assert!(o.stdout.contains("answer yes; hooks wait for that."), "{}", o.stdout);
    let j = sim.cli(&["status", "--json"]);
    assert_eq!(j.code, 5);
    let v: Value = serde_json::from_str(j.stdout.trim()).unwrap();
    assert_eq!(v["saving"], false);
    assert_eq!(v["points"], 0);
    // Nothing was made by asking.
    let made = std::fs::read_dir(&sim.env.thome).map(|d| d.flatten().any(|e| e.path().join("torpor.db").exists())).unwrap_or(false);
    assert!(!made, "asking for the status must not make a store");
}

#[test]
fn a_running_claude_code_session_without_hooks_is_named() {
    let mut sim = Sim::new();
    sim.session_start();
    sim.prompt("one");
    sim.stop();
    let sessions = claude_dir(&sim).join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let me = std::process::id();
    let cwd = sim.proj().to_string_lossy().into_owned();
    // Counted: alive, in this folder, no heartbeat.
    std::fs::write(sessions.join("111.json"), serde_json::json!({"pid": me, "cwd": cwd, "sessionId": "ghost-1", "name": "ignored"}).to_string()).unwrap();
    // Not counted: the session that does run hooks, a dead process, another folder, a broken file.
    std::fs::write(sessions.join("222.json"), serde_json::json!({"pid": me, "cwd": cwd, "sessionId": sim.sid}).to_string()).unwrap();
    std::fs::write(sessions.join("333.json"), serde_json::json!({"pid": 2_000_000_000i64, "cwd": cwd, "sessionId": "ghost-dead"}).to_string()).unwrap();
    std::fs::write(sessions.join("444.json"), serde_json::json!({"pid": me, "cwd": "/", "sessionId": "ghost-elsewhere"}).to_string()).unwrap();
    std::fs::write(sessions.join("555.json"), "{not json").unwrap();
    // A `.key` file must never be opened: reading a pipe nobody writes to would hang, so a helper lets go after 4 s.
    let fifo = sessions.join("111.key");
    let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let unblock = fifo.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(4));
        let _ = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(unblock);
    });

    let t0 = std::time::Instant::now();
    let v = json(&sim);
    assert!(t0.elapsed() < Duration::from_secs(3), "status waited on a .key file");
    let p = problems(&v);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].starts_with("1 Claude Code session in this folder has not run any Torpor hook, so nothing from it is being saved."), "{p:?}");
    assert_eq!(v["saving"], false);
}

#[test]
fn a_session_whose_hooks_went_quiet_is_named() {
    let mut sim = Sim::new();
    sim.session_start();
    sim.prompt("one");
    sim.stop();
    let transcript = sim.transcript.clone();
    let touch = |ahead: u64| {
        if !transcript.exists() {
            std::fs::write(&transcript, "{}\n").unwrap();
        }
        let f = std::fs::OpenOptions::new().write(true).open(&transcript).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(ahead)).unwrap();
    };
    // The transcript moved a little after the last hook: normal (the last message is written after Stop).
    touch(20);
    assert!(problems(&json(&sim)).is_empty());
    // Minutes later: Claude Code kept working and no hook ran.
    touch(400);
    let v = json(&sim);
    let p = problems(&v);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].starts_with("No Torpor hooks ran in this session since "), "{p:?}");
    assert_eq!(v["saving"], false);
    // A hook that runs again clears it.
    sim.prompt("two");
    sim.stop();
    touch(20);
    assert!(problems(&json(&sim)).is_empty());
}

#[test]
fn the_reach_follows_the_claude_code_cleanup_setting() {
    let mut sim = Sim::new();
    sim.session_start();
    sim.prompt("one");
    sim.stop();
    let settings = claude_dir(&sim).join("settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "{\"cleanupPeriodDays\": 14}").unwrap();
    let t = sim.cli(&["status"]).stdout;
    assert!(t.contains("Save points are kept for 14 days (your Claude Code cleanup setting).\n"), "{t}");
    std::fs::write(&settings, "{\"cleanupPeriodDays\": 1}").unwrap();
    let t = sim.cli(&["status"]).stdout;
    assert!(t.contains("Claude Code is set to keep chats for 1 day(s); Torpor keeps save points for at least 2 days.\n"), "{t}");
    assert!(t.contains("Save points are kept for 2 days"), "{t}");
    std::fs::write(&settings, "not json at all").unwrap();
    assert!(sim.cli(&["status"]).stdout.contains("kept for 30 days"), "an unreadable settings file falls back to 30");
}

#[test]
fn a_lost_key_file_is_reported() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[(".env", "TOKEN=abc123\n"), (".npmrc", "x=1\n")]);
    sim.session_start();
    sim.prompt("one");
    sim.stop();
    assert!(sim.env.key.exists());
    std::fs::remove_file(&sim.env.key).unwrap();
    let v = json(&sim);
    let p = problems(&v);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].starts_with("2 secret files saved before "), "{p:?}");
    assert!(p[0].ends_with(" can't be restored: their key file is gone."), "{p:?}");
}
