//! A4: fail-open. Five faults on UserPromptSubmit. Each must exit 0, print nothing, answer within the event's own
//! deadline plus 100 ms, and be named by `torpor status`.
mod common;
use common::*;
use std::time::Duration;

const UPS_DEADLINE: Duration = Duration::from_millis(5000 + 100);

/// A session with a working store and one finished turn, so the fault hits a project that was fine before.
fn working_sim() -> Sim {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A1\n")]);
    sim.session_start();
    sim.prompt("first turn");
    sim.write_tool("a.txt", "A2\n");
    sim.stop();
    sim
}

fn assert_silent(label: &str, o: &Out) {
    assert_eq!(o.code, 0, "{label}: exit code");
    assert!(o.stdout.is_empty(), "{label}: stdout must be empty, got {:?}", o.stdout);
    assert!(o.stderr.is_empty(), "{label}: stderr must be empty, got {:?}", o.stderr);
    assert!(o.elapsed <= UPS_DEADLINE, "{label}: took {:?}, over the deadline", o.elapsed);
}

fn turns(sim: &Sim) -> i64 {
    sim.q_i64("SELECT COUNT(*) FROM turns")
}

/// Run one UserPromptSubmit with an extra environment variable, then take it away again.
fn ups_with(sim: &mut Sim, key: &str, value: &str) -> Out {
    sim.next_pid();
    sim.extra_env.push((key.into(), value.into()));
    let o = sim.hook_unchecked("UserPromptSubmit", serde_json::json!({"prompt": "will fail"}));
    sim.extra_env.pop();
    o
}

fn status_says(sim: &Sim, what: &str) {
    let s = sim.status();
    assert!(s.stdout.contains(what), "status must name the fault ({what:?}); it said:\n{}", s.stdout);
}

#[test]
fn a4_1_launcher_without_binary() {
    let sim = Sim::new();
    let root = make_plugin_root(sim.env._tmp.path(), false);
    let payload = sim.payload("UserPromptSubmit", serde_json::json!({"prompt": "hello"}));
    let mut c = cmd_with(&root.join("bin/torpor"), &sim.env);
    c.args(["hook", "UserPromptSubmit"]);
    let o = run_cmd(c, payload.to_string().as_bytes());
    assert_silent("launcher", &o);
    let log = std::fs::read_to_string(sim.env.thome.join("faults.log")).unwrap();
    assert!(log.contains(" - - launcher no binary for "), "faults.log: {log:?}");
    let s = sim.status();
    assert!(s.stdout.contains("launcher could not start"), "status said:\n{}", s.stdout);
    assert!(s.stdout.contains("no binary for"), "status said:\n{}", s.stdout);
    // The same launcher used from a terminal says so plainly and fails.
    let o = run_cmd(cmd_with(&root.join("bin/torpor"), &sim.env), b"");
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("Torpor has no build for this computer ("), "{}", o.stderr);
}

#[test]
fn a4_2_panic() {
    let mut sim = working_sim();
    let before = turns(&sim);
    let o = ups_with(&mut sim, "TORPOR_TEST_PANIC", "1");
    assert_silent("panic", &o);
    assert_eq!(turns(&sim), before, "a crashed hook leaves no half-written turn");
    status_says(&sim, "crashed");
    // Torpor keeps working afterwards.
    let ok = sim.prompt("second turn");
    assert_eq!(ok.code, 0);
    assert_eq!(turns(&sim), before + 1);
}

#[test]
fn a4_3_timeout() {
    let mut sim = working_sim();
    let o = ups_with(&mut sim, "TORPOR_TEST_SLEEP_MS", "10000");
    assert_silent("timeout", &o);
    assert!(o.elapsed >= Duration::from_millis(4900), "the watchdog fired early: {:?}", o.elapsed);
    status_says(&sim, "ran out of time");
    let ok = sim.prompt("after the timeout");
    assert_eq!(ok.code, 0);
}

#[test]
fn a4_4_garbage_stdin() {
    let sim = working_sim();
    let before = turns(&sim);
    let mut c = base_cmd(&sim.env);
    c.args(["hook", "UserPromptSubmit"]);
    let o = run_cmd(c, b"{{garbage");
    assert_silent("garbage", &o);
    assert_eq!(turns(&sim), before);
    status_says(&sim, "could not read what Claude Code sent");
}

#[test]
fn a4_5_disk_full() {
    let mut sim = working_sim();
    let points = sim.q_i64("SELECT COUNT(*) FROM points");
    let o = ups_with(&mut sim, "TORPOR_TEST_FREE_BYTES", "0");
    assert_silent("disk", &o);
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM points"), points, "no save point is made on a full disk");
    status_says(&sim, "disk is almost full");
    // With room again, save points continue.
    sim.prompt("room again");
    assert!(sim.q_i64("SELECT COUNT(*) FROM points") > points, "save points continue once there is room");
    sim.verify_last_point();
}

/// More fail-open edges the spec names (3.1, 3.2): unknown event, an oversized payload, a payload that is not an object.
#[test]
fn a4_extra_unknown_event_oversize_and_shape() {
    let sim = working_sim();
    let mut c = base_cmd(&sim.env);
    c.args(["hook", "NoSuchEvent"]);
    assert_silent("unknown event", &run_cmd(c, b"{}"));

    let mut c = base_cmd(&sim.env);
    c.args(["hook", "UserPromptSubmit"]);
    let big = vec![b' '; 9 << 20];
    let o = run_cmd(c, &big);
    assert_silent("9 MiB payload", &o);
    status_says(&sim, "could not read what Claude Code sent");

    let mut c = base_cmd(&sim.env);
    c.args(["hook", "Stop"]);
    assert_silent("array payload", &run_cmd(c, b"[1,2,3]"));

    // Lenient JSON: unknown fields and a missing session id are fine.
    let mut c = base_cmd(&sim.env);
    c.args(["hook", "PreCompact"]);
    assert_silent("lenient", &run_cmd(c, br#"{"future_field":{"x":[1,2]}}"#));
}
