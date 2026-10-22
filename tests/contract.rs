//! The output contract (spec 3.3 and the "contract" row of section 10): every sentence Torpor can print in S1a, with
//! its exact text, in the right JSON shape for its event, and nothing at all for the events that must stay silent.
//! Every answer also passes `check_contract` inside the harness (allowed keys per event, nested hookEventName,
//! no `allow`, strings under 10,000 characters, Stop line within 160).
mod common;
use common::app::{SERVER, add, broken_server, login, story};
use common::*;
use rusqlite::params;
use serde_json::json;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

const FINISHED_AT: i64 = 1_760_000_000_000_000_000;
const INTERRUPTED: &str = "Torpor: a restore was interrupted and some files are mixed. Ask the user to run torpor recover before editing.";
const PROTECTED: &str = "Torpor: this file belongs to Torpor's save points; the AI can't change it.";

/// A second, writable connection: the harness one is read-only.
fn rw(sim: &Sim) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(sim.ws_dir().join("torpor.db")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c
}

/// What `torpor back` (S1b) will leave behind: one row in `restores`.
fn add_restore(sim: &Sim, status: &str, target: i64, files: &[&str]) -> i64 {
    let c = rw(sim);
    let report = json!({ "files": files }).to_string();
    c.execute(
        "INSERT INTO restores(kind,target_point,started_at,finished_at,status,report_json) VALUES('back',?1,?2,?2,?3,?4)",
        params![target, FINISHED_AT, status, report],
    )
    .unwrap();
    c.last_insert_rowid()
}

fn silent(label: &str, o: &Out) {
    assert!(o.stdout.is_empty(), "{label}: expected no output, got {:?}", o.stdout);
}

/// A project with one finished turn; returns the sim and the id of that turn's end point.
fn one_turn() -> (Sim, i64) {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("src/a.ts", "a1\n"), ("src/b.ts", "b1\n")]);
    sim.session_start();
    sim.prompt("first");
    sim.write_tool("src/a.ts", "a2\n");
    sim.stop();
    let end = sim.last_visible_point();
    (sim, end)
}

// ---------------------------------------------------------------- restore notices

#[test]
fn restore_notice_is_shown_once_per_session_and_per_restore() {
    let (mut a, end) = one_turn();
    let mut b = a.second_session();
    b.session_start();
    b.prompt("b before the restore");
    let at = torpor::util::local_hhmm(FINISHED_AT);
    let five = ["src/a.ts", "src/b.ts", "src/c.ts", "src/d.ts", "src/e.ts"];
    add_restore(&a, "done", end, &five);
    let want = format!("Torpor: files were restored to save point {end} (end of turn 1) at {at}: src/a.ts, src/b.ts and 3 more. Re-read files before editing them.");

    let o = a.prompt("second");
    assert_eq!(o.text(), want);
    let v = o.json().unwrap();
    assert_eq!(v.pointer("/hookSpecificOutput/hookEventName").and_then(|x| x.as_str()), Some("UserPromptSubmit"));
    silent("same session, next prompt", &a.prompt("third"));

    // Another session that was already running has not heard of it yet.
    assert_eq!(b.prompt("hello").text(), want);
    silent("second session, next prompt", &b.prompt("again"));

    // A session that starts after the restore reads the files fresh: nothing to tell.
    let mut c = b.second_session();
    c.session_start();
    silent("session started after the restore", &c.prompt("new"));

    // A new restore is announced again, with all names when there are three or fewer.
    add_restore(&a, "partial", end, &["x/y.rs", "z.rs"]);
    assert_eq!(a.prompt("fourth").text(), format!("Torpor: files were restored to save point {end} (end of turn 1) at {at}: x/y.rs, z.rs. Re-read files before editing them."));
    add_restore(&a, "done", end, &[]);
    assert_eq!(a.prompt("fifth").text(), format!("Torpor: files were restored to save point {end} (end of turn 1) at {at}. Re-read files before editing them."));
    add_restore(&a, "done", end, &["one.txt"]);
    assert_eq!(a.prompt("sixth").text(), format!("Torpor: files were restored to save point {end} (end of turn 1) at {at}: one.txt. Re-read files before editing them."));
    // Nothing but a prompt carries the notice.
    add_restore(&a, "done", end, &["late.txt"]);
    silent("PreToolUse", &a.hook("PreToolUse", json!({"tool_name": "Write", "tool_input": {"file_path": a.path("q.txt")}, "tool_use_id": "t"})));
    silent("Stop", &a.stop());
}

#[test]
fn interrupted_restore_is_announced_until_it_is_recovered() {
    let (mut a, end) = one_turn();
    let id = add_restore(&a, "applying", end, &[]);
    assert_eq!(a.prompt("second").text(), INTERRUPTED);
    assert_eq!(a.prompt("third").text(), INTERRUPTED, "the warning stays until recover has run");
    let mut b = a.second_session();
    b.session_start();
    assert_eq!(b.prompt("other session").text(), INTERRUPTED);
    // Every other event stays silent while it is pending.
    silent("edit", &a.edit("src/b.ts", "b2\n"));
    assert!(a.stop().text().starts_with("Torpor: save point"), "the Stop line is not replaced by the warning");
    // recover (S1b) marks it finished: the warning goes, the normal notice comes once.
    rw(&a).execute("UPDATE restores SET status='done', report_json='{\"files\":[\"src/b.ts\"]}' WHERE id=?1", [id]).unwrap();
    let at = torpor::util::local_hhmm(FINISHED_AT);
    assert_eq!(a.prompt("fourth").text(), format!("Torpor: files were restored to save point {end} (end of turn 1) at {at}: src/b.ts. Re-read files before editing them."));
    silent("after the notice", &a.prompt("fifth"));
}

// ---------------------------------------------------------------- the store is not the AI's to edit

fn pre(sim: &mut Sim, tool: &str, key: &str, path: &str) -> Out {
    sim.hook("PreToolUse", json!({"tool_name": tool, "tool_input": {key: path}, "tool_use_id": "toolu_c"}))
}

fn assert_denied(o: &Out, what: &str) {
    let v = o.json().unwrap_or_else(|| panic!("{what}: not denied (no output)"));
    assert_eq!(v.pointer("/hookSpecificOutput/permissionDecision").and_then(|x| x.as_str()), Some("deny"), "{what}");
    assert_eq!(o.text(), PROTECTED, "{what}");
}

#[test]
fn edits_into_the_store_and_the_key_are_denied() {
    use std::os::unix::fs::symlink;
    let (mut s, _) = one_turn();
    let thome = s.env.thome.clone();
    let ws = s.ws_dir();
    let base = thome.parent().unwrap().to_path_buf();
    let p = |x: &std::path::Path| x.to_string_lossy().into_owned();
    // Links an attacker (or a confused model) could have made inside the project.
    symlink(&thome, s.path("link")).unwrap();
    symlink(&ws, s.path("deeplink")).unwrap();
    symlink(&s.env.key, s.path("dangling")).unwrap(); // the key file does not exist yet
    assert!(!s.env.key.exists());

    for mode in ["default", "bypassPermissions", "acceptEdits"] {
        s.permission_mode = mode.into();
        let cases: Vec<(&str, &str, String)> = vec![
            ("Write", "file_path", p(&thome.join("FORMAT"))),
            ("Edit", "file_path", p(&ws.join("torpor.db"))),
            ("MultiEdit", "file_path", p(&thome.join("new/deep/file.txt"))),
            ("NotebookEdit", "notebook_path", p(&thome.join("x.ipynb"))),
            ("Write", "file_path", p(&s.env.key)),
            ("Write", "file_path", "../torpor-home/FORMAT".into()),
            ("Write", "file_path", p(&s.path("../torpor-home/./FORMAT"))),
            ("Write", "file_path", "link/FORMAT".into()),
            ("Write", "file_path", "link/sub/new.txt".into()),
            ("Write", "file_path", "deeplink/../FORMAT".into()), // the kernel goes through the link, a text fold does not
            ("Write", "file_path", "dangling".into()),           // writing through it would create the key file
        ];
        for (tool, key, path) in &cases {
            assert_denied(&pre(&mut s, tool, key, path), &format!("{mode}: {tool} {path}"));
        }
    }
    s.permission_mode = "default".into();
    assert!(!s.env.key.exists(), "a denied write must not have created the key file");

    // Everything else is left alone, including names that only look similar.
    let ok: Vec<(&str, &str, String)> = vec![
        ("Write", "file_path", p(&s.path("src/new.ts"))),
        ("Edit", "file_path", "src/a.ts".into()),
        ("Write", "file_path", p(&base.join("other/x.txt"))),
        ("Write", "file_path", p(&base.join("torpor-home-2/x"))),
        ("NotebookEdit", "notebook_path", p(&s.path("n.ipynb"))),
    ];
    for (tool, key, path) in &ok {
        silent(&format!("{tool} {path}"), &pre(&mut s, tool, key, path));
    }
    // The rule covers the edit family only; shell commands are S1b's (section 7).
    silent("Read", &pre(&mut s, "Read", "file_path", &p(&thome.join("FORMAT"))));
}

// ---------------------------------------------------------------- the end-of-turn line

#[test]
fn stop_line_text_suffix_and_quiet() {
    let mut s = Sim::new();
    write_files(s.proj(), &[("a.txt", "A1\n"), ("b.txt", "B1\n"), ("c.txt", "C1\n"), ("d.txt", "D1\n")]);
    s.session_start();

    s.prompt("t1");
    s.edit("a.txt", "A2\n");
    s.write_tool("b.txt", "B2\n");
    s.bash("echo C2 > c.txt");
    let o = s.stop();
    let end = s.last_visible_point();
    assert_eq!(o.text(), format!("Torpor: save point {end} · turn 1 changed 3 files (2 edits, 1 shell) · undo it: torpor undo 1"));
    assert_eq!(o.json().unwrap().as_object().unwrap().len(), 1, "the line is the only key");

    s.prompt("t2");
    s.edit("a.txt", "A3\n");
    let o = s.stop();
    let end = s.last_visible_point();
    assert_eq!(o.text(), format!("Torpor: save point {end} · turn 2 changed 1 file (1 edit) · undo it: torpor undo 2"));

    s.prompt("t3");
    s.edit("a.txt", "A4\n");
    s.bash("echo D2 > d.txt");
    s.subagent_edit("agent-1", "b.txt", "B3\n");
    let o = s.stop();
    let end = s.last_visible_point();
    assert_eq!(o.text(), format!("Torpor: save point {end} · turn 3 changed 3 files (1 edit, 1 shell, 1 subagent) · undo it: torpor undo 3"));

    // A turn that changed nothing prints nothing; so does an edit that put the same bytes back.
    s.prompt("t4");
    silent("empty turn", &s.stop());
    s.prompt("t5");
    s.edit("a.txt", "A4\n");
    silent("same bytes", &s.stop());

    // A file that is too big to keep is counted in the suffix.
    s.prompt("t6");
    s.edit("a.txt", "A5\n");
    s.bash("head -c 3000000 /dev/zero > big.bin");
    let o = s.stop();
    let end = s.last_visible_point();
    assert_eq!(o.text(), format!("Torpor: save point {end} · turn 6 changed 2 files (1 edit, 1 shell) · undo it: torpor undo 6 · 1 file not saved (torpor status)"));
    assert!(o.text().chars().count() <= 160);

    // quiet on: nothing is printed, the save point is still made.
    let q = s.cli(&["quiet", "on"]);
    assert_eq!((q.code, q.stdout.as_str()), (0, "The end-of-turn line is now off.\n"));
    let before = s.max_point();
    s.prompt("t7");
    s.edit("a.txt", "A6\n");
    silent("quiet on", &s.stop());
    assert!(s.max_point() > before);
    let q = s.cli(&["quiet", "off"]);
    assert_eq!((q.code, q.stdout.as_str()), (0, "The end-of-turn line is now on.\n"));
    s.prompt("t8");
    s.edit("a.txt", "A7\n");
    let end = s.max_point() + 1; // the Stop point is the next one made
    assert_eq!(s.stop().text(), format!("Torpor: save point {end} · turn 8 changed 1 file (1 edit) · undo it: torpor undo 8"));
    assert_eq!(s.cli(&["quiet", "maybe"]).code, 2);
}

// ---------------------------------------------------------------- SessionStart problems

#[test]
fn session_start_says_when_the_disk_is_almost_full() {
    let mut s = Sim::new();
    write_files(s.proj(), &[("a.txt", "A1\n")]);
    s.extra_env.push(("TORPOR_TEST_FREE_BYTES".into(), "800000000".into()));
    assert_eq!(s.session_start().text(), "Torpor: save points are paused: the disk is almost full (0.8 GB free).");
    // The rest of the session stays quiet and keeps going.
    silent("prompt", &s.prompt("p"));
    silent("edit", &s.edit("a.txt", "A2\n"));
    silent("stop", &s.stop());
    let st = s.status();
    assert_eq!(st.code, 0);
    assert!(st.stdout.contains("disk"), "status should name the disk problem: {}", st.stdout);
    // With room again the same session start has nothing to say.
    s.extra_env.clear();
    silent("session start with room", &s.second_session().session_start());
}

#[test]
fn session_start_says_when_the_key_file_is_gone() {
    let mut s = Sim::new();
    write_files(s.proj(), &[(".env", "TOKEN=abc123\n"), ("a.txt", "A1\n")]);
    s.session_start();
    s.prompt("one");
    s.write_tool("a.txt", "A2\n");
    s.stop();
    assert!(s.env.key.exists(), "the key is made with the first secret file");
    silent("key present", &s.second_session().session_start());
    std::fs::remove_file(&s.env.key).unwrap();
    let mut t = s.second_session();
    assert_eq!(t.session_start().text(), "Torpor: 1 secret file saved earlier can't be restored: their key file is gone.");
}

#[test]
fn project_folders_that_are_refused() {
    let mut s = Sim::new();
    let home = s.env.home.to_string_lossy().into_owned();
    let msg = "Torpor does not record your whole home folder. Start Claude Code inside a project folder to get save points.";
    for dir in [home.as_str(), "/"] {
        s.extra_env = vec![("CLAUDE_PROJECT_DIR".into(), dir.into())];
        assert_eq!(s.session_start().text(), msg, "{dir}");
        // Said once, at SessionStart; nothing else is ever printed or recorded.
        silent("prompt", &s.prompt("p"));
        silent("tool", &s.hook("PreToolUse", json!({"tool_name": "Write", "tool_input": {"file_path": "/tmp/x"}, "tool_use_id": "t"})));
        silent("stop", &s.stop());
    }
    assert!(!s.env.thome.exists(), "nothing may be created for a refused folder");
    // The store itself is not a project; that one is silent even at SessionStart.
    s.extra_env.clear();
    s.session_start();
    s.extra_env = vec![("CLAUDE_PROJECT_DIR".into(), s.env.thome.to_string_lossy().into_owned())];
    let mut t = s.second_session();
    t.extra_env = s.extra_env.clone();
    silent("store as project", &t.session_start());
    silent("store as project, prompt", &t.prompt("p"));
}

#[test]
fn a_newer_store_is_left_alone() {
    let (mut s, _) = one_turn();
    let points = s.q_i64("SELECT COUNT(*) FROM points");
    rw(&s).execute_batch("PRAGMA user_version=99").unwrap();
    let want = "Torpor: This project's records were written by a newer Torpor. Update the plugin.";
    check_newer(&mut s, want, points);
}

#[test]
fn a_newer_format_file_is_left_alone() {
    let (mut s, _) = one_turn();
    let points = s.q_i64("SELECT COUNT(*) FROM points");
    std::fs::write(s.env.thome.join("FORMAT"), "torpor-store 2\n").unwrap();
    let want = "Torpor: This project's records were written by a newer Torpor. Update the plugin.";
    check_newer(&mut s, want, points);
}

fn check_newer(s: &mut Sim, want: &str, points: i64) {
    let mut t = s.second_session();
    assert_eq!(t.session_start().text(), want);
    silent("prompt", &t.prompt("p"));
    silent("edit", &t.edit("src/b.ts", "changed\n"));
    silent("bash", &{
        t.bash("echo x > src/c.ts");
        t.hook("PostToolUse", json!({"tool_name": "Bash", "tool_input": {"command": "true"}, "tool_use_id": "t9"}))
    });
    silent("stop", &t.stop());
    silent("session end", &t.session_end("other"));
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM points"), points, "nothing may be recorded");
    let st = s.status();
    assert_eq!(st.code, 3, "{}", st.stderr);
    assert!(st.stderr.contains("newer Torpor"), "{}", st.stderr);
    assert!(st.stdout.is_empty());
}

// ---------------------------------------------------------------- switches and silent events

#[test]
fn the_off_switches_silence_everything() {
    let (mut s, _) = one_turn();
    let mark = s.max_point();
    let run = |s: &mut Sim, label: &str| {
        silent(label, &s.prompt("off"));
        silent(label, &s.edit("src/b.ts", &format!("{label}\n")));
        silent(label, &s.stop());
        assert_eq!(s.max_point(), mark, "{label}: nothing may be saved while off");
    };
    s.extra_env.push(("TORPOR_OFF".into(), "1".into()));
    run(&mut s, "TORPOR_OFF=1");
    s.extra_env.clear();
    let c = rw(&s);
    c.execute("INSERT INTO meta(key,value) VALUES('off','1') ON CONFLICT(key) DO UPDATE SET value='1'", []).unwrap();
    run(&mut s, "meta.off");
    c.execute("UPDATE meta SET value='0' WHERE key='off'", []).unwrap();
    s.prompt("on again");
    s.edit("src/b.ts", "back on\n");
    s.stop();
    assert!(s.max_point() > mark, "saving resumes when the switch is off");
}

#[test]
fn events_that_must_stay_quiet_do() {
    let (mut s, end) = one_turn();
    // Worst case: a pending notice and an unfinished restore.
    add_restore(&s, "applying", end, &["a"]);
    add_restore(&s, "done", end, &["a"]);
    s.prompt("go");
    silent("PostToolUse(Edit)", &s.edit("src/a.ts", "x\n"));
    s.bash("echo hi > z.txt");
    s.bash("exit 3");
    silent("PostToolUseFailure", &s.hook("PostToolUseFailure", json!({"tool_name": "Bash", "tool_input": {"command": "false"}, "tool_use_id": "t5", "error": "Exit code 1", "is_interrupt": false})));
    silent("SubagentStop", &s.subagent_stop("agent-9"));
    silent("PreCompact", &s.hook("PreCompact", json!({"trigger": "auto", "custom_instructions": ""})));
    silent("StopFailure", &s.hook("StopFailure", json!({"error": "rate_limit"})));
    silent("SessionEnd", &s.session_end("logout"));
    // Unknown events and empty input: exit 0, nothing printed.
    silent("unknown event", &s.hook("SomethingNew", json!({})));
    let env = s.env.clone();
    for ev in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure", "Stop", "StopFailure", "SubagentStop", "PreCompact", "SessionEnd"] {
        let mut c = base_cmd(&env);
        c.args(["hook", ev]);
        let o = run_cmd(c, b"{}");
        check_contract(ev, &o);
    }
    assert!(!s.faults().iter().any(|l| l.contains(" panic ")), "no hook may panic on bare input: {:?}", s.faults());
}

#[test]
fn odd_input_is_survived() {
    let (mut s, _) = one_turn();
    // Wrong types, nulls and extra fields.
    let weird = json!({
        "session_id": 5, "prompt": {"x": 1}, "cwd": null, "prompt_id": [], "tool_name": 7, "tool_input": "text",
        "unknown_future_field": {"a": [1, 2, 3]}, "permission_mode": false,
    });
    for ev in ["UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop", "SessionEnd"] {
        let mut c = base_cmd(&s.env);
        c.args(["hook", ev]);
        check_contract(ev, &run_cmd(c, weird.to_string().as_bytes()));
    }
    // Not JSON, a JSON array, and more than 8 MiB.
    let big = vec![b'x'; 9 << 20];
    for (label, bytes) in [("garbage", b"{{garbage".to_vec()), ("array", b"[1,2]".to_vec()), ("empty", Vec::new()), ("9 MiB", big)] {
        let mut c = base_cmd(&s.env);
        c.args(["hook", "UserPromptSubmit"]);
        let o = run_cmd(c, &bytes);
        assert_eq!(o.code, 0, "{label}");
        silent(label, &o);
        assert!(o.stderr.is_empty(), "{label}: {}", o.stderr);
    }
    let f = s.faults();
    assert!(f.iter().filter(|l| l.contains(" parse ")).count() >= 4, "every unreadable input is a parse fault: {f:?}");
    assert!(!f.iter().any(|l| l.contains(" panic ")), "{f:?}");
    // The project still works afterwards.
    s.prompt("after");
    s.edit("src/b.ts", "fine\n");
    assert!(s.stop().text().contains("changed 1 file"));
}

// ---------------------------------------------------------------- the checker checks

fn out(code: i32, stdout: &str, stderr: &str) -> Out {
    Out { code, stdout: stdout.into(), stderr: stderr.into(), elapsed: Duration::ZERO }
}

#[test]
fn the_contract_checker_rejects_what_it_should() {
    let long = "x".repeat(10_000);
    let stop_long = format!("{{\"systemMessage\":\"Torpor: {}\"}}\n", "y".repeat(160));
    let bad: Vec<(&str, Out)> = vec![
        ("UserPromptSubmit", out(0, "{\"decision\":\"block\",\"reason\":\"x\"}\n", "")),
        ("UserPromptSubmit", out(0, "{\"continue\":false}\n", "")),
        ("UserPromptSubmit", out(0, "{\"suppressOutput\":true}\n", "")),
        ("PreToolUse", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\",\"permissionDecisionReason\":\"Torpor: x\"}}\n", "")),
        ("PreToolUse", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"UserPromptSubmit\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"Torpor: x\"}}\n", "")),
        ("PreToolUse", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"updatedInput\":{},\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"Torpor: x\"}}\n", "")),
        ("UserPromptSubmit", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"a\"}}\n{\"x\":1}\n", "")),
        ("UserPromptSubmit", out(0, &format!("{{\"hookSpecificOutput\":{{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"{long}\"}}}}\n"), "")),
        ("SessionStart", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"a\"}}\n", "")),
        ("SessionStart", out(0, "{\"systemMessage\":\"Something else\"}\n", "")),
        ("Stop", out(0, &stop_long, "")),
        ("Stop", out(0, "{\"systemMessage\":\"Torpor: ok\"}", "")),
        ("Stop", out(0, "not json\n", "")),
        ("Stop", out(0, "[1]\n", "")),
        ("Stop", out(2, "", "")),
        ("Stop", out(0, "", "oops")),
        ("PostToolUse", out(0, "{\"systemMessage\":\"Torpor: ok\"}\n", "")),
        ("PostToolUse", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"additionalContext\":\"edited a file\"}}\n", "")),
        ("SessionStart", out(0, "{\"decision\":\"block\",\"reason\":\"Torpor: x\"}\n", "")),
        ("Stop", out(0, "{\"decision\":\"block\"}\n", "")),
        ("Stop", out(0, "{\"reason\":\"Torpor: x\"}\n", "")),
        ("Stop", out(0, "{\"decision\":\"allow\",\"reason\":\"Torpor: x\"}\n", "")),
        ("Stop", out(0, "{\"decision\":\"block\",\"reason\":\"Fix the app\"}\n", "")),
        ("Stop", out(0, "{\"decision\":\"block\",\"reason\":\"Torpor: x\",\"systemMessage\":\"Torpor: y\"}\n", "")),
        ("Stop", out(0, "{\"systemMessage\":\"Torpor: one\\nsecond line\"}\n", "")),
        ("Stop", out(0, &format!("{{\"decision\":\"block\",\"reason\":\"Torpor: {long}\"}}\n"), "")),
        ("SessionEnd", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"SessionEnd\",\"additionalContext\":\"a\"}}\n", "")),
        // S2: a block reason of 101 words or on two lines, and a rebuild or a targeted line of 10,000 characters.
        ("Stop", out(0, &format!("{{\"decision\":\"block\",\"reason\":\"Torpor: {}\"}}\n", "word ".repeat(100)), "")),
        ("Stop", out(0, "{\"decision\":\"block\",\"reason\":\"Torpor: one\\nTorpor: two\"}\n", "")),
        ("PostToolUse", out(0, &format!("{{\"hookSpecificOutput\":{{\"hookEventName\":\"PostToolUse\",\"additionalContext\":\"Torpor: {long}\"}}}}\n"), "")),
        ("SessionStart", out(0, &format!("{{\"hookSpecificOutput\":{{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"Torpor (after compaction 1): {long}\"}}}}\n"), "")),
    ];
    for (i, (ev, o)) in bad.iter().enumerate() {
        assert!(catch_unwind(AssertUnwindSafe(|| check_contract(ev, o))).is_err(), "case {i} ({ev}) should have been rejected: {:?}", o.stdout);
    }
    let good: Vec<(&str, Out)> = vec![
        ("Stop", out(0, "", "")),
        ("Stop", out(0, "{\"systemMessage\":\"Torpor: save point 3 · turn 1 changed 1 file (1 edit) · undo it: torpor undo 1\"}\n", "")),
        ("SessionStart", out(0, "{\"systemMessage\":\"Torpor: save points are paused.\"}\n", "")),
        ("UserPromptSubmit", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"Torpor: x\"}}\n", "")),
        ("PreToolUse", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"Torpor: x\"}}\n", "")),
        ("PreToolUse", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"Torpor: x\"}}\n", "")),
        ("Stop", out(0, "{\"systemMessage\":\"Torpor: save point 3 · turn 1 changed 1 file\\nTorpor: new promise #2 \\\"login works\\\"\"}\n", "")),
        ("Stop", out(0, "{\"decision\":\"block\",\"reason\":\"Torpor: promise #2 fails now. Fix the app, not the check.\"}\n", "")),
        ("UserPromptSubmit", out(0, "{\"systemMessage\":\"Torpor: hi\",\"hookSpecificOutput\":{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"Torpor: x\"}}\n", "")),
        ("UserPromptSubmit", out(0, "{\"systemMessage\":\"Torpor: hi\"}\n", "")),
        ("SessionStart", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"Torpor (after compaction 1): rules\"}}\n", "")),
        ("PostToolUse", out(0, "{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"additionalContext\":\"Torpor: edit touches promises #2\"}}\n", "")),
    ];
    for (ev, o) in &good {
        check_contract(ev, o);
    }
}

// ---------------------------------------------------------------- S2: every kind of answer, produced for real

/// "Event/shape" of every answer on the tape that said something.
fn shapes(sim: &Sim) -> std::collections::BTreeSet<String> {
    let mut v = std::collections::BTreeSet::new();
    for (event, out) in &sim.tape {
        let j: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        for (path, name) in [("/decision", "block"), ("/systemMessage", "systemMessage"), ("/hookSpecificOutput/additionalContext", "additionalContext"), ("/hookSpecificOutput/permissionDecision", "permissionDecision")] {
            if j.pointer(path).is_some() {
                v.insert(format!("{event}/{name}"));
            }
        }
    }
    v
}

#[test]
fn every_s2_answer_kind_is_produced_and_passes_the_contract() {
    // Every answer of the Sim goes through `check_contract` (a bad one panics inside `hook`); here the S2 kinds are made to happen.
    let mut s = story();
    assert_eq!(add(&mut s, &login()).code, 0);
    s.stop();
    // Advisory: the edit gets the targeted line, the Stop a note for the person, the next prompt one line for the AI.
    s.prompt("add dark mode");
    s.edit("server.js", &broken_server());
    s.stop();
    s.prompt("what happened?");
    // Blocking: the next regression stops the AI.
    rw(&s).execute_batch("UPDATE promises SET tier='blocking' WHERE id=1").unwrap();
    s.edit("server.js", &(broken_server() + "// again\n"));
    let o = s.stop();
    assert_eq!(o.json().unwrap()["decision"], "block", "{}", o.stdout);
    // The AI fixes it, and the conversation is compacted.
    s.edit("server.js", SERVER);
    s.stop();
    s.compaction("compact-1");
    let seen = shapes(&s);
    for w in ["PostToolUse/additionalContext", "Stop/block", "Stop/systemMessage", "UserPromptSubmit/additionalContext", "SessionStart/additionalContext"] {
        assert!(seen.contains(w), "no {w} answer was produced: {seen:?}");
    }
}
