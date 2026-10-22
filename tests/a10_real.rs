//! A10 (spec section 10) and A8 (a) and (b): the ONE real Claude Code run. Both tests are ignored, so `cargo test`
//! never spends anything. Two steps, so the asserts can be repeated without another real run:
//!
//!   CARGO_TARGET_DIR=target/dist cargo build --release     # the plain binary that ships, never the test build
//!   TORPOR_A10_BIN=target/dist/release/torpor TORPOR_A10_DIR=<new folder> \
//!       cargo test --test a10_real a10_run -- --ignored --exact --nocapture
//!   TORPOR_A10_DIR=<the same folder> cargo test --test a10_real a10_assert -- --ignored --exact --nocapture
//!
//! `a10_run` builds a temp project (a git repo with a project-local Stop hook), runs `claude -p` once (haiku, 0.50 USD
//! cap, the user's normal plugins and settings loaded) with the plain binary copied to `dist/linux-x64/torpor`, and keeps
//! everything in the folder. `a10_assert` reads that folder, drives the INSTALLED binary (`back`, `back 1`, `undo 1`),
//! checks every A10 / A8(a) / A8(b) assert, and writes `a10-log.txt` (turn origin and probe fields included) there.
//! The settings file is only hashed (sha256), never printed. Nothing in this file writes outside the given folder,
//! except the binary copy under `dist/`, which `a10_run` removes again.
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PROMPT: &str = "Do exactly these steps and nothing else. 1) Use the Write tool to create a.txt containing hello. 2) Run: sed -i 's/one/two/' keep.txt 3) Run: rm -f old.txt 4) Run: mkdir -p src && echo x > src/x.txt 5) Reply DONE.";
// A8(b): a project-local Stop hook that leaves a marker next to the project (outside it, so Torpor never sees it).
const STOP_HOOK: &str = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo ran > \"$CLAUDE_PROJECT_DIR/../stop-marker.txt\""}]}]}}"#;

fn t_dir() -> PathBuf {
    PathBuf::from(std::env::var("TORPOR_A10_DIR").expect("set TORPOR_A10_DIR to a folder (see the top of this file)"))
}

fn code_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// sha256 of the user's settings.json, or "absent". The file is never printed.
fn settings_hash() -> String {
    let p = PathBuf::from(std::env::var("HOME").expect("HOME")).join(".claude").join("settings.json");
    if !p.is_file() {
        return "absent".into();
    }
    let o = Command::new("sha256sum").arg(&p).output().expect("sha256sum");
    assert!(o.status.success(), "sha256sum failed");
    String::from_utf8_lossy(&o.stdout).split_whitespace().next().unwrap_or("").to_string()
}

/// Every entry under `root` with size, mtime and content hash: "untouched" means this text is equal.
fn snapshot(root: &Path) -> String {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        let mut ents: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        ents.sort_by_key(|e| e.file_name());
        for e in ents {
            let p = e.path();
            let md = fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(base).unwrap().display().to_string();
            if md.is_dir() {
                out.push(format!("d {rel}"));
                walk(&p, base, out);
            } else if md.is_file() {
                out.push(format!("f {rel} {} {}.{} {}", md.len(), md.mtime(), md.mtime_nsec(), blake3::hash(&fs::read(&p).unwrap()).to_hex()));
            } else {
                out.push(format!("o {rel}"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.join("\n")
}

#[test]
#[ignore = "the one real Claude Code run (Max subscription, haiku, 0.50 USD cap); see the top of this file"]
fn a10_run() {
    let t = t_dir();
    assert!(!t.exists() || fs::read_dir(&t).unwrap().next().is_none(), "TORPOR_A10_DIR must be new or empty");
    let bin = PathBuf::from(std::env::var("TORPOR_A10_BIN").expect("TORPOR_A10_BIN = the plain release binary"));
    assert!(bin.is_file(), "TORPOR_A10_BIN is not a file");
    fs::create_dir_all(&t).unwrap();
    let t = fs::canonicalize(&t).unwrap();
    let proj = t.join("proj");
    fs::create_dir_all(proj.join(".claude")).unwrap();
    assert!(Command::new("git").args(["init", "-q"]).current_dir(&proj).status().unwrap().success());
    fs::write(proj.join("keep.txt"), "one\n").unwrap();
    fs::write(proj.join("old.txt"), "old\n").unwrap();
    fs::write(proj.join(".claude/settings.json"), STOP_HOOK).unwrap();
    // The plugin's launcher looks for dist/linux-x64/torpor. dist/ is gitignored, and this copy is removed after the run.
    let dist = code_dir().join("dist").join("linux-x64");
    fs::create_dir_all(&dist).unwrap();
    fs::copy(&bin, dist.join("torpor")).unwrap();
    fs::set_permissions(dist.join("torpor"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(t.join("bin.blake3"), blake3::hash(&fs::read(&bin).unwrap()).to_hex().to_string()).unwrap();
    fs::write(t.join("settings.before"), settings_hash()).unwrap();
    fs::write(t.join("git.before"), snapshot(&proj.join(".git"))).unwrap();

    let mut cmd = Command::new("claude");
    cmd.current_dir(&proj)
        .args(["-p", "--model", "haiku", "--plugin-dir"])
        .arg(code_dir())
        .args(["--allowedTools", "Bash,Edit,Write,Read", "--max-budget-usd", "0.50"])
        .args(["--strict-mcp-config", "--mcp-config", r#"{"mcpServers":{}}"#, "--output-format", "json"])
        .env("TORPOR_HOME", t.join("home"))
        .env("TORPOR_KEY_FILE", t.join("key"))
        .stdin(Stdio::piped())
        .stdout(fs::File::create(t.join("claude-out.json")).unwrap())
        .stderr(fs::File::create(t.join("claude-err.txt")).unwrap());
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
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if started.elapsed() > Duration::from_secs(300) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    std::thread::sleep(Duration::from_secs(3)); // a detached cleanup run may still be finishing
    let _ = fs::remove_file(dist.join("torpor"));
    let _ = fs::remove_dir(&dist);
    let _ = fs::remove_dir(code_dir().join("dist"));
    fs::write(t.join("settings.after"), settings_hash()).unwrap();
    fs::write(t.join("git.after-run"), snapshot(&proj.join(".git"))).unwrap();
    fs::write(t.join("run.txt"), format!("exit={:?} seconds={}\n", status.map(|s| s.code()), started.elapsed().as_secs())).unwrap();
    println!("claude finished: {:?} after {} s; artifacts are in the folder you gave", status, started.elapsed().as_secs());
    assert!(status.is_some_and(|s| s.success()), "claude did not finish cleanly: see claude-err.txt and claude-out.json in the folder");
}

// ---------------------------------------------------------------- the asserts

struct Cli {
    code: i32,
    out: String,
    err: String,
}

fn cli(t: &Path, args: &[&str]) -> Cli {
    let bin = t.join(format!("home/bin/{}/torpor", env!("CARGO_PKG_VERSION")));
    assert!(bin.is_file(), "the launcher did not install the binary into the store");
    for attempt in 0..4 {
        let o = Command::new(&bin)
            .args(args)
            .current_dir(t.join("proj"))
            .env_remove("CLAUDECODE")
            .env("TORPOR_HOME", t.join("home"))
            .env("TORPOR_KEY_FILE", t.join("key"))
            .env("CLAUDE_PROJECT_DIR", t.join("proj"))
            .output()
            .expect("run torpor");
        let r = Cli { code: o.status.code().unwrap_or(-1), out: String::from_utf8_lossy(&o.stdout).into(), err: String::from_utf8_lossy(&o.stderr).into() };
        if r.code != 0 && r.err.contains("is busy") && attempt < 3 {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        return r;
    }
    unreachable!()
}

fn ws_dir(t: &Path) -> PathBuf {
    let found: Vec<PathBuf> = fs::read_dir(t.join("home")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.join("torpor.db").is_file()).collect();
    assert_eq!(found.len(), 1, "expected one workspace in the store, found {}", found.len());
    found[0].clone()
}

/// All columns as text (NULL, numbers, text, blobs as hex).
fn rows(t: &Path, q: &str) -> Vec<Vec<String>> {
    let db = Connection::open_with_flags(ws_dir(t).join("torpor.db"), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut st = db.prepare(q).unwrap();
    let n = st.column_count();
    st.query_map([], |r| {
        (0..n)
            .map(|i| {
                Ok(match r.get_ref(i)? {
                    ValueRef::Null => "NULL".to_string(),
                    ValueRef::Integer(v) => v.to_string(),
                    ValueRef::Real(v) => v.to_string(),
                    ValueRef::Text(b) => String::from_utf8_lossy(b).into_owned(),
                    ValueRef::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
            })
            .collect()
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

fn one(t: &Path, q: &str) -> String {
    rows(t, q).first().and_then(|r| r.first().cloned()).unwrap_or_else(|| panic!("no row for: {q}"))
}

fn read(t: &Path, rel: &str) -> Option<String> {
    fs::read_to_string(t.join("proj").join(rel)).ok()
}

struct Log(fs::File);

impl Log {
    fn say(&mut self, s: impl AsRef<str>) {
        println!("{}", s.as_ref());
        writeln!(self.0, "{}", s.as_ref()).unwrap();
    }
}

/// The newest point must be a "restored" one whose tree is the given point's tree.
fn assert_tree_is(t: &Path, point: &str, what: &str) {
    assert_eq!(one(t, "SELECT kind FROM points ORDER BY id DESC LIMIT 1"), "restored", "{what}: the last point is not a restored one");
    let want = one(t, &format!("SELECT tree_id FROM points WHERE id={point}"));
    let got = one(t, "SELECT tree_id FROM points ORDER BY id DESC LIMIT 1");
    assert_eq!(got, want, "{what}: the tree hash after the restore is not the hash of point {point}");
}

#[test]
#[ignore = "reads the folder of one finished a10_run; see the top of this file"]
fn a10_assert() {
    let t = fs::canonicalize(t_dir()).expect("TORPOR_A10_DIR does not exist: run a10_run first");
    let mut log = Log(fs::File::create(t.join("a10-log.txt")).unwrap());
    let run = fs::read_to_string(t.join("run.txt")).unwrap();
    log.say(format!("run: {}", run.trim()));

    // The launcher installed the plain binary that was given.
    let installed = blake3::hash(&fs::read(t.join(format!("home/bin/{}/torpor", env!("CARGO_PKG_VERSION")))).expect("installed binary")).to_hex().to_string();
    assert_eq!(installed, fs::read_to_string(t.join("bin.blake3")).unwrap(), "the installed binary is not the plain release binary");

    // Heartbeats for the six events.
    let beats: Vec<String> = rows(&t, "SELECT event || ' x' || SUM(count) FROM heartbeat GROUP BY event ORDER BY event").into_iter().map(|r| r[0].clone()).collect();
    log.say(format!("heartbeats: {}", beats.join(", ")));
    for ev in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop", "SessionEnd"] {
        assert!(beats.iter().any(|b| b.starts_with(&format!("{ev} "))), "no heartbeat for {ev}");
    }

    // One turn, with the turn origin and the probe fields in the log.
    let turns = rows(&t, "SELECT n, origin, origin_by, preview, start_point, end_point, end_kind, bash_count FROM turns ORDER BY n");
    log.say(format!("turns (n, origin, origin_by, preview, start_point, end_point, end_kind, bash_count): {turns:?}"));
    assert_eq!(turns.len(), 1, "expected exactly 1 turn");
    let (start, end) = (turns[0][4].clone(), turns[0][5].clone());
    log.say(format!("sessions (source, permission_mode, ended, end_reason): {:?}", rows(&t, "SELECT source, permission_mode, ended_at IS NOT NULL, end_reason FROM sessions")));
    log.say(format!("tool_events (tool, exit_code, failed, paths, cmd): {:?}", rows(&t, "SELECT tool_name, exit_code, failed, paths_json, cmd_preview FROM tool_events ORDER BY id")));
    log.say(format!("points (id, kind, visible, note): {:?}", rows(&t, "SELECT id, kind, visible, note FROM points ORDER BY id")));
    log.say(format!("faults: {:?}", rows(&t, "SELECT at, event, kind FROM faults")));
    assert!(rows(&t, "SELECT 1 FROM faults").is_empty(), "Torpor recorded faults during the run");

    // The classes of the turn, and a pre_bash point before rm.
    let show = cli(&t, &["show", "1", "--json"]);
    assert_eq!(show.code, 0, "show 1: {}", show.err);
    let v: Value = serde_json::from_str(&show.out).unwrap();
    let files: Vec<(String, String)> = v["files"].as_array().unwrap().iter().map(|f| (f["path"].as_str().unwrap().to_string(), f["class"].as_str().unwrap().to_string())).collect();
    log.say(format!("show 1 files (path, class): {files:?}"));
    for (p, c) in [("a.txt", "edit"), ("keep.txt", "shell"), ("old.txt", "shell"), ("src/x.txt", "shell")] {
        assert!(files.iter().any(|(fp, fc)| fp == p && fc == c), "{p} should be a {c} change of turn 1");
    }
    let extra: Vec<&(String, String)> = files.iter().filter(|(p, _)| !["a.txt", "keep.txt", "old.txt", "src/x.txt", "src", "src/"].contains(&p.as_str())).collect();
    assert!(extra.is_empty(), "unexpected changed files: {extra:?}");
    let pb_id = one(&t, "SELECT id FROM points WHERE kind='pre_bash' AND note LIKE '%rm -f old.txt%' ORDER BY id LIMIT 1").parse::<i64>().expect("a pre_bash point for rm -f old.txt");
    let rm_change = one(&t, "SELECT MIN(point_id) FROM changes WHERE path='old.txt'").parse::<i64>().unwrap();
    assert!(pb_id < rm_change, "the pre_bash point {pb_id} is not before the point that recorded old.txt going away ({rm_change})");
    let who = cli(&t, &["log", "--json"]);
    log.say(format!("log --json who: {}", serde_json::from_str::<Value>(&who.out).unwrap()[0]["who"]));

    // A repeat of this test starts from the folder the previous one left (the start state): bring it to the end state first.
    let b = cli(&t, &["back", "--turn", "1"]);
    assert_eq!(b.code, 0, "back --turn 1: {}{}", b.out, b.err);
    // Going back to the start gives the initial tree, going back one gives the end tree.
    let b = cli(&t, &["back", "--id", &start]);
    assert_eq!(b.code, 0, "back --id {start}: {}{}", b.out, b.err);
    assert_tree_is(&t, &start, "back --id <start>");
    assert_eq!(read(&t, "keep.txt").as_deref(), Some("one\n"));
    assert!(read(&t, "old.txt").is_some() && read(&t, "a.txt").is_none() && read(&t, "src/x.txt").is_none(), "the initial files are not all back");
    let b = cli(&t, &["back", "1"]);
    assert_eq!(b.code, 0, "back 1: {}{}", b.out, b.err);
    assert_tree_is(&t, &end, "back 1");
    assert_eq!(read(&t, "keep.txt").as_deref(), Some("two\n"));
    assert!(read(&t, "old.txt").is_none() && read(&t, "src/x.txt").as_deref() == Some("x\n"));
    assert_eq!(read(&t, "a.txt").map(|s| s.trim().to_string()).as_deref(), Some("hello"));
    // Undo of the only turn gives the initial tree again. (A copy of the folder would be another workspace, so the same folder is used; it holds the end state again.)
    let u = cli(&t, &["undo", "1"]);
    assert_eq!(u.code, 0, "undo 1: {}{}", u.out, u.err);
    assert_tree_is(&t, &start, "undo 1");
    assert_eq!(read(&t, "keep.txt").as_deref(), Some("one\n"));
    assert!(read(&t, "old.txt").is_some() && read(&t, "a.txt").is_none() && read(&t, "src/x.txt").is_none());
    let st = cli(&t, &["status"]);
    log.say(format!("status:\n{}", st.out));
    assert!(st.out.contains("Problems: none."), "status does not say Problems: none.");

    // A8 (a): the settings file did not change. And .git is untouched.
    let (before, after) = (fs::read_to_string(t.join("settings.before")).unwrap(), fs::read_to_string(t.join("settings.after")).unwrap());
    assert_eq!(before, after, "the user's settings.json changed during the run");
    assert_eq!(settings_hash(), before, "the user's settings.json changed after the run");
    log.say(format!("settings.json sha256 equal before and after: {}", &before[..before.len().min(12)]));
    let git_before = fs::read_to_string(t.join("git.before")).unwrap();
    assert_eq!(git_before, fs::read_to_string(t.join("git.after-run")).unwrap(), ".git changed during the run");
    assert_eq!(git_before, snapshot(&t.join("proj/.git")), ".git changed during back, back 1 or undo");

    // A8 (b): the project-local Stop hook ran, and the transcript has no hook errors from Torpor's hooks.
    assert!(t.join("stop-marker.txt").is_file(), "the project-local Stop hook left no marker");
    let tp = one(&t, "SELECT transcript_path FROM sessions ORDER BY first_seen LIMIT 1");
    let transcript = fs::read_to_string(&tp).expect("this run's own transcript");
    let (mut kinds, mut torpor_errors, mut other_errors, mut context_mode) = (std::collections::BTreeMap::<String, usize>::new(), Vec::new(), 0, 0);
    for line in transcript.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["type"] != "attachment" {
            continue;
        }
        let a = &v["attachment"];
        let kind = a["type"].as_str().unwrap_or("?").to_string();
        if !kind.starts_with("hook_") {
            continue;
        }
        let text = a.to_string();
        let ours = text.contains("/bin/torpor");
        if text.contains("context-mode") {
            context_mode += 1;
        }
        *kinds.entry(format!("{kind}{}", if ours { " (torpor)" } else { "" })).or_default() += 1;
        let bad_kind = ["error", "blocking", "timeout", "cancel", "stopped"].iter().any(|w| kind.contains(w));
        let bad_exit = a["exitCode"].as_i64().is_some_and(|c| c != 0);
        if bad_kind || bad_exit {
            if ours {
                torpor_errors.push(text.chars().take(300).collect::<String>());
            } else {
                other_errors += 1;
            }
        }
    }
    log.say(format!("transcript hook attachments: {kinds:?}; mentioning context-mode: {context_mode}; other hooks' errors: {other_errors}"));
    assert!(torpor_errors.is_empty(), "hook error attachments from Torpor: {torpor_errors:?}");
}
