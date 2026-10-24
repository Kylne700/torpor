//! A10-block: ONE real Claude Code run that shows the gate blocking end to end. All three tests are ignored, so `cargo
//! test` never spends anything.
//!
//! `a10_block_seed` is the free rehearsal (no Claude): it seeds a project whose login promise is blocking, then checks
//! that a scripted edit makes the simulated Stop block. `a10_block_run` seeds the same way and runs `claude -p` once
//! (haiku, 0.50 USD cap, the user's normal plugins and settings loaded) on a COPY of the plugin that holds the plain
//! binary, keeps the folder (project, store, plugin copy, payloads.txt, claude-out.json) and prints the report.
//! `a10_block_report` prints the same report again from a kept folder: whether Stop blocked, whether every continuation
//! kept the prompt_id, how many times the chain blocked, the cost and the permission mode. The block text itself is in
//! the run's own transcript (its path is in the payloads).
//!
//!   CARGO_TARGET_DIR=target/dist cargo build --release     # the plain binary that ships, never the test build
//!   TMPDIR=/tmp/claude-1000 cargo test --test a10_block a10_block_seed -- --ignored --exact --nocapture
//!   TMPDIR=/tmp/claude-1000 TORPOR_A10_BIN=target/dist/release/torpor \
//!       cargo test --test a10_block a10_block_run -- --ignored --exact --nocapture
//!   TORPOR_A10_DIR=<the kept folder> cargo test --test a10_block a10_block_report -- --ignored --exact --nocapture
mod common;

use common::app::*;
use common::*;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// Torpor blocks a second time only when the project changed since the first block, so the prompt makes the AI touch the
// file after each report. That is how the cap of two blocks is reached.
const PROMPT: &str = "Do exactly this. 1) With the Edit tool, change the line `const BROKEN = false;` in server.js to `const BROKEN = true;`. 2) Reply DONE. If a hook then reports a problem, do not undo or fix anything. Instead add one new comment line at the end of server.js (`// note 1` the first time, `// note 2` the second time) and reply DONE again. Never touch another file.";

// Project-local hooks that keep what Claude Code sends, one line per event, next to the project (outside it, so Torpor never sees it).
const CAPTURE: &str = r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"{ printf 'UserPromptSubmit '; cat; echo; } >> \"$CLAUDE_PROJECT_DIR/../payloads.txt\""}]}],"Stop":[{"hooks":[{"type":"command","command":"{ printf 'Stop '; cat; echo; } >> \"$CLAUDE_PROJECT_DIR/../payloads.txt\""}]}]}}"#;

/// The login promise made blocking, in a project that holds the capture hooks; the turn that saved it has ended.
fn seeded() -> Sim {
    let mut s = two_turns(&[(".claude/settings.json", CAPTURE)]);
    s.prompt("great, login works now");
    let o = add(&mut s, &login());
    assert_eq!(o.code, 0, "{}", why(&o));
    rw(&s).execute_batch("UPDATE promises SET tier='blocking' WHERE id=1").unwrap();
    s.stop();
    s
}

#[test]
#[ignore = "the free rehearsal of a10_block_run: no Claude, only the simulated session; see the top of this file"]
fn a10_block_seed() {
    let mut s = seeded();
    assert_eq!(s.q_str("SELECT tier FROM promises WHERE id=1").as_deref(), Some("blocking"));
    s.prompt("make the login page broken");
    s.edit("server.js", &broken_server());
    let o = s.stop();
    let v = o.json().unwrap_or_else(|| panic!("the Stop did not block: {}", why(&o)));
    assert_eq!(v["decision"], "block", "{}", why(&o));
    println!("rehearsal block text: {}", v["reason"].as_str().unwrap_or(""));
}

#[test]
#[ignore = "the one real A10-block Claude Code run (Max subscription, haiku, 0.50 USD cap); see the top of this file"]
fn a10_block_run() {
    let bin = PathBuf::from(std::env::var("TORPOR_A10_BIN").expect("TORPOR_A10_BIN = the plain release binary"));
    assert!(bin.is_file(), "TORPOR_A10_BIN is not a file");
    let s = seeded();
    let (proj, base) = (s.env.proj.clone(), s.env.proj.parent().unwrap().to_path_buf());
    assert!(base.starts_with("/tmp/claude-1000"), "run with TMPDIR=/tmp/claude-1000 (the folder is {})", base.display());
    plugin_copy(&base.join("plugin"), &bin);
    let runs0 = s.q_i64("SELECT COALESCE(MAX(id),0) FROM promise_runs");
    println!("folder (kept): {}", base.display());

    let mut cmd = Command::new("claude");
    cmd.current_dir(&proj)
        .args(["-p", "--model", "haiku", "--plugin-dir"])
        .arg(base.join("plugin"))
        .args(["--allowedTools", "Bash,Edit,Write,Read", "--max-budget-usd", "0.50"])
        .args(["--strict-mcp-config", "--mcp-config", r#"{"mcpServers":{}}"#, "--output-format", "json"])
        .env("TORPOR_HOME", &s.env.thome)
        .env("TORPOR_KEY_FILE", &s.env.key)
        .stdin(Stdio::piped())
        .stdout(fs::File::create(base.join("claude-out.json")).unwrap())
        .stderr(fs::File::create(base.join("claude-err.txt")).unwrap());
    // A fresh terminal run: nothing of the surrounding Claude Code session may leak in (its session id, socket, token).
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("CLAUDE") && k != "CLAUDE_CONFIG_DIR" {
            cmd.env_remove(k);
        }
    }
    let started = Instant::now();
    let mut child = cmd.spawn().expect("start claude");
    child.stdin.take().unwrap().write_all(PROMPT.as_bytes()).unwrap();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break Some(st);
        }
        if started.elapsed() > Duration::from_secs(600) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    std::thread::sleep(Duration::from_secs(3)); // a detached cleanup run may still be finishing
    println!("claude: {:?} after {} s", status, started.elapsed().as_secs());

    fs::write(base.join("seed.txt"), runs0.to_string()).unwrap();
    report(&base);
    std::mem::forget(s); // keep the folder: the store and the payloads may be needed to read the result again
    assert!(status.is_some_and(|st| st.success()), "claude did not finish cleanly: see claude-err.txt and claude-out.json in the folder");
}

#[test]
#[ignore = "prints the report of a kept a10_block_run folder (TORPOR_A10_DIR); see the top of this file"]
fn a10_block_report() {
    report(&PathBuf::from(std::env::var("TORPOR_A10_DIR").expect("TORPOR_A10_DIR = the folder a10_block_run kept")));
}

/// What happened in the kept folder `base`: what Claude Code sent (the capture hooks), its result, and what Torpor stored.
fn report(base: &Path) {
    let text = fs::read_to_string(base.join("payloads.txt")).unwrap_or_default();
    let (mut prompt_ids, mut stop_ids) = (Vec::new(), Vec::new());
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (event, json) = line.split_once(' ').unwrap_or((line, ""));
        let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
        let pid = v["prompt_id"].as_str().unwrap_or("(none)").to_string();
        println!("payload {event}: prompt_id={pid} permission_mode={} stop_hook_active={} session={}", v["permission_mode"], v["stop_hook_active"], v["session_id"]);
        println!("payload {event}: transcript_path={}", v["transcript_path"]);
        if event == "Stop" { stop_ids.push(pid) } else { prompt_ids.push(pid) }
    }
    let same = prompt_ids.len() == 1 && !stop_ids.is_empty() && stop_ids.iter().all(|p| *p == prompt_ids[0] && p != "(none)");
    let out: Value = serde_json::from_str(&fs::read_to_string(base.join("claude-out.json")).unwrap_or_default()).unwrap_or(Value::Null);
    println!("claude result: subtype={} is_error={} num_turns={} cost_usd={} stop_reason={}", out["subtype"], out["is_error"], out["num_turns"], out["total_cost_usd"], out["stop_reason"]);
    println!("claude result text: {}", out["result"].as_str().unwrap_or("").chars().take(300).collect::<String>());
    println!("claude permission_denials: {}", out["permission_denials"]);

    // What Torpor stored for it (the store is opened read-only).
    let db = fs::read_dir(base.join("torpor-home")).unwrap().map(|e| e.unwrap().path()).find(|p| p.join("torpor.db").is_file()).expect("no workspace in the store").join("torpor.db");
    let c = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let qs = |sql: &str| c.query_row(sql, [], |r| r.get::<_, Option<String>>(0)).ok().flatten().unwrap_or_else(|| "-".into());
    let qi = |sql: &str| c.query_row(sql, [], |r| r.get::<_, Option<i64>>(0)).ok().flatten().unwrap_or(0);
    let runs0: i64 = fs::read_to_string(base.join("seed.txt")).unwrap_or_default().trim().parse().unwrap_or(0);
    println!("turns after the seed (n > 3): {}", qs("SELECT group_concat(n||' '||substr(session_id,1,10)||' '||COALESCE(substr(prompt_id,1,12),'-')||' '||end_kind, ' | ') FROM turns WHERE n > 3"));
    println!("receipts (turn:selected/ran/outcome): {}", qs("SELECT group_concat(turn_id||':'||selected||'/'||ran||'/'||outcome, ' | ') FROM (SELECT * FROM receipts ORDER BY id DESC LIMIT 8)"));
    println!("gate_chain: {}", qs("SELECT group_concat(substr(session_id,1,10)||' '||substr(prompt_id,1,12)||' promise '||promise_id||' blocks '||blocks, ' | ') FROM gate_chain"));
    println!("promise runs since the seed: {}", qs(&format!("SELECT group_concat(kind||'='||verdict, ' ') FROM promise_runs WHERE id > {runs0}")));
    println!("promise tier now: {}", qs("SELECT tier FROM promises WHERE id=1"));
    println!("faults: {}", qs("SELECT group_concat(event||':'||kind, ' | ') FROM faults"));
    let blocks = qi("SELECT COALESCE(SUM(blocks),0) FROM gate_chain");
    let installed = base.join("torpor-home/bin/2.3.0/torpor");
    let shipped = base.join("plugin/dist").join(dist_target()).join("torpor");
    println!("launcher installed the plain binary: {}", installed.is_file() && fs::read(&installed).unwrap() == fs::read(&shipped).unwrap());
    let server = fs::read_to_string(base.join("proj/server.js")).unwrap_or_default();
    println!("server.js now: {}", server.lines().filter(|l| l.contains("const BROKEN") || l.starts_with("// note")).collect::<Vec<_>>().join(" / "));
    println!("VERDICT stop_events={} blocks={} same_prompt_id={same} cap_held={}", stop_ids.len(), blocks, blocks <= 2);
}
