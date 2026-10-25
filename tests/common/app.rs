//! Helpers for the tests that run the fixture app (promises, the gate): a Sim session, the fixture server and its check files.
use super::*;
use std::time::{Duration, Instant};

pub const SERVER: &str = include_str!("../fixtures/app/server.js");

pub const START: &str = r#"{"cmd":"node server.js --port {port}","ready":"/login","timeout_s":30}"#;
pub const LOGIN_STEPS: &str = r#"[
  {"http":{"method":"POST","path":"/api/login","json":{"email":"kim@example.com"}},"expect":{"status":200},"why":"sign-in is accepted"},
  {"http":{"path":"/api/me"},"expect":{"status":200,"json_has":["email"]},"why":"after sign-in the account data shows"}]"#;

pub fn spec(name: &str, start: &str, env: &str, steps: &str) -> String {
    format!(
        r#"{{"v":1,"name":"{name}","why":"{name} for the user","start":{start},"env":{env},"steps":{steps}}}"#
    )
}

pub fn login() -> String {
    spec("login works", START, "{}", LOGIN_STEPS)
}

pub fn broken_server() -> String {
    SERVER.replace("const BROKEN = false;", "const BROKEN = true;")
}

/// Two finished turns: the AI writes the app with the seeded bug, then fixes it. `extra` are files the project already had.
pub fn two_turns(extra: &[(&str, &str)]) -> Sim {
    let mut s = Sim::new();
    write_files(s.proj(), extra);
    s.session_start();
    s.prompt("make a login page");
    s.write_tool("server.js", &broken_server());
    s.stop();
    s.prompt("login does not work, fix it");
    s.edit("server.js", SERVER);
    s.stop();
    s
}

/// Turn 3 is open: the user has just said it works.
pub fn story() -> Sim {
    let mut s = two_turns(&[]);
    s.prompt("great, login works now");
    s
}

/// A second, writable connection: the harness one is read-only.
pub fn rw(s: &Sim) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(s.ws_dir().join("torpor.db")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c
}

/// Poll until `f` holds (a detached Torpor process finished its work); panics after `secs`.
pub fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(secs),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What the AI does in a Bash call: Torpor sees the command before it runs (PreToolUse), then the command runs with CLAUDECODE set.
pub fn agent(s: &mut Sim, args: &[&str], stdin: &str) -> Out {
    s.pre_bash(&format!("torpor {}", args.join(" ")));
    s.cli_env_stdin(args, &[("CLAUDECODE", "1")], stdin)
}

pub fn add(s: &mut Sim, spec: &str) -> Out {
    agent(s, &["promise", "add", "-"], spec)
}

pub fn why(o: &Out) -> String {
    format!(
        "exit {}\nstdout: {}\nstderr: {}",
        o.code, o.stdout, o.stderr
    )
}

/// Every stored run of a check, one line each, for the message of a failed assertion.
pub fn runs(s: &Sim) -> String {
    s.q_str("SELECT group_concat(kind||' '||verdict||' step '||COALESCE(failed_step,'-')||': '||COALESCE(reason,''), char(10)) FROM promise_runs").unwrap_or_default()
}
