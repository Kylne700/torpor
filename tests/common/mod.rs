//! Test harness (spec section 10): runs the built `torpor` binary like Claude Code would, with JSON on stdin and
//! a temp store, key file, project and HOME. Every hook answer is checked against the output contract (spec 3.3).
//! `oracle` is an independent walker (std + blake3) that says what the project really holds.
#![allow(dead_code)]

pub mod app;

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_torpor");

// ---------------------------------------------------------------- environment

/// One project, one fake HOME, one store, one key file. Shared by every session of a test.
pub struct Env {
    pub _tmp: tempfile::TempDir,
    pub proj: PathBuf,
    pub home: PathBuf,
    pub thome: PathBuf,
    pub key: PathBuf,
    /// The ports this test's apps may use (`TORPOR_TEST_PORTS`): tests that start apps at the same moment must not pick the same port.
    pub ports: String,
}

static NEXT_WINDOW: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

impl Env {
    pub fn new() -> Env {
        let tmp = tempfile::Builder::new().prefix("torpor-sim").tempdir().unwrap();
        Env::in_dir(tmp)
    }

    pub fn in_dir(tmp: tempfile::TempDir) -> Env {
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let (proj, home, thome) = (base.join("proj"), base.join("home"), base.join("torpor-home"));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let key = home.join(".config").join("torpor").join("key");
        // 220 windows of 100 ports below the range the system hands out itself; the pid keeps two test runs at once from sharing the first ones.
        let w = (NEXT_WINDOW.fetch_add(1, std::sync::atomic::Ordering::Relaxed) * 37 + std::process::id()) % 220;
        Env { _tmp: tmp, proj, home, thome, key, ports: format!("{}-{}", 10_000 + w * 100, 10_100 + w * 100) }
    }
}

pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

impl Out {
    pub fn json(&self) -> Option<Value> {
        if self.stdout.trim().is_empty() { None } else { serde_json::from_str(self.stdout.trim()).ok() }
    }
    /// systemMessage, or additionalContext, or the deny reason: the one sentence the hook said.
    pub fn text(&self) -> String {
        let Some(v) = self.json() else { return String::new() };
        v.get("systemMessage")
            .or_else(|| v.pointer("/hookSpecificOutput/additionalContext"))
            .or_else(|| v.pointer("/hookSpecificOutput/permissionDecisionReason"))
            .or_else(|| v.get("reason"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    }
}

/// Command for the binary with the safe environment every test uses.
pub fn base_cmd(env: &Env) -> Command {
    cmd_with(Path::new(BIN), env)
}

/// Same safe environment for any program (the launcher, a copy of the binary).
pub fn cmd_with(program: &Path, env: &Env) -> Command {
    let mut c = Command::new(program);
    c.env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDECODE")
        .env_remove("TORPOR_OFF")
        .env_remove("TORPOR_FAULT")
        .env_remove("TORPOR_TEST_PANIC")
        .env_remove("TORPOR_TEST_SLEEP_MS")
        .env_remove("TORPOR_TEST_FREE_BYTES")
        .env_remove("TORPOR_TEST_MODE")
        .env("TORPOR_HOME", &env.thome)
        .env("TORPOR_KEY_FILE", &env.key)
        .env("CLAUDE_PROJECT_DIR", &env.proj)
        .env("HOME", &env.home)
        .env("XDG_CONFIG_HOME", env.home.join(".config"))
        .env("CLAUDE_CODE_MANAGED_SETTINGS_PATH", env.home.join("managed"))
        .env("TORPOR_TEST_NO_GC", "1")
        .env("TORPOR_TEST_PORTS", &env.ports)
        .current_dir(&env.proj);
    c
}

pub fn run_cmd(mut c: Command, stdin: &[u8]) -> Out {
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let t0 = Instant::now();
    let mut child = c.spawn().unwrap();
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin);
    }
    let o = child.wait_with_output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        elapsed: t0.elapsed(),
    }
}

// ---------------------------------------------------------------- output contract (spec 3.3, S2 section 12)

fn has_forbidden(v: &Value, event: &str, top: bool) -> Option<String> {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if ["continue", "suppressOutput", "stopReason"].contains(&k.as_str()) {
                    return Some(format!("key {k}"));
                }
                // The one block there is: a Stop hook that stops the AI (S2 gate). Nothing else may carry a decision.
                if k == "decision" && !(top && event == "Stop" && x.as_str() == Some("block")) {
                    return Some(format!("key {k}"));
                }
                if let Some(f) = has_forbidden(x, event, false) {
                    return Some(f);
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(|x| has_forbidden(x, event, false)),
        Value::String(s) if s == "allow" => Some("value \"allow\"".into()),
        _ => None,
    }
}

/// Every line of a message Torpor says starts with "Torpor" (the user must see who is talking).
fn assert_torpor_lines(event: &str, what: &str, s: &str) {
    assert!(s.chars().count() < 10_000, "{event}: {what} too long");
    for l in s.lines() {
        assert!(l.starts_with("Torpor"), "{event}: {what} line does not start with Torpor: {l}");
    }
    assert!(!s.is_empty(), "{event}: empty {what}");
}

/// Panics when a hook answer breaks the contract: one JSON object at most, allowed keys for this event, no
/// `allow`, strings under 10,000 characters, the first Stop line within 160, nothing on stderr, exit 0.
/// Allowed: Stop `systemMessage` (lines) or the block `decision`+`reason`; UserPromptSubmit and SessionStart
/// `additionalContext` (and a `systemMessage` for the user); PostToolUse `additionalContext`; PreToolUse deny or ask.
pub fn check_contract(event: &str, o: &Out) {
    assert_eq!(o.code, 0, "{event}: exit code {} (stderr: {})", o.code, o.stderr);
    assert!(o.stderr.is_empty(), "{event}: wrote to stderr: {}", o.stderr);
    if o.stdout.is_empty() {
        return;
    }
    assert!(o.stdout.ends_with('\n') && o.stdout.matches('\n').count() == 1, "{event}: stdout is not one line: {:?}", o.stdout);
    let v: Value = serde_json::from_str(o.stdout.trim_end()).unwrap_or_else(|e| panic!("{event}: stdout is not JSON ({e}): {:?}", o.stdout));
    let obj = v.as_object().unwrap_or_else(|| panic!("{event}: stdout is not an object"));
    if let Some(f) = has_forbidden(&v, event, true) {
        panic!("{event}: forbidden output ({f}): {}", o.stdout);
    }
    let keys: BTreeSet<&str> = obj.keys().map(|k| k.as_str()).collect();
    let allowed: &[&str] = match event {
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => &["hookSpecificOutput"],
        "UserPromptSubmit" | "SessionStart" => &["hookSpecificOutput", "systemMessage"],
        "Stop" => &["systemMessage", "decision", "reason"],
        _ => &[],
    };
    for k in &keys {
        assert!(allowed.contains(k), "{event}: key {k} is not allowed in {}", o.stdout);
    }
    if let Some(s) = obj.get("systemMessage") {
        let s = s.as_str().expect("systemMessage is a string");
        assert_torpor_lines(event, "systemMessage", s);
        if event == "Stop" {
            let first = s.lines().next().unwrap_or("");
            // The save-point line keeps the spec's 160; a promise note carries a reason of its own (a few hundred characters at most).
            let note = ["Torpor: promise #", "Torpor: new promise #", "Torpor: the AI disputes promise #"].iter().any(|p| first.starts_with(p));
            let max = if note { 700 } else { 160 };
            assert!(first.chars().count() <= max, "Stop line over {max} characters: {first}");
        }
    }
    // A block comes with its reason and nothing else; a reason never comes alone.
    assert_eq!(keys.contains("decision"), keys.contains("reason"), "{event}: decision and reason go together: {}", o.stdout);
    if let Some(r) = obj.get("reason") {
        let r = r.as_str().expect("reason is a string");
        assert_torpor_lines(event, "reason", r);
        assert!(r.split_whitespace().count() <= 100 && r.lines().count() == 1, "{event}: a block reason is one line of at most 100 words: {r}");
        assert!(!keys.contains("systemMessage"), "{event}: a block carries only its reason: {}", o.stdout);
    }
    if let Some(h) = obj.get("hookSpecificOutput") {
        let h = h.as_object().expect("hookSpecificOutput is an object");
        assert_eq!(h.get("hookEventName").and_then(|x| x.as_str()), Some(event), "{event}: nested hookEventName");
        let ok: &[&str] = if event == "PreToolUse" { &["hookEventName", "permissionDecision", "permissionDecisionReason"] } else { &["hookEventName", "additionalContext"] };
        for k in h.keys() {
            assert!(ok.contains(&k.as_str()), "{event}: nested key {k} is not allowed");
        }
        if event == "PreToolUse" {
            let d = h.get("permissionDecision").and_then(|x| x.as_str());
            assert!(matches!(d, Some("deny") | Some("ask")), "PreToolUse may only deny or ask, never allow: {d:?}");
            let r = h.get("permissionDecisionReason").and_then(|x| x.as_str()).unwrap_or_else(|| panic!("PreToolUse: no reason in {}", o.stdout));
            assert!(r.chars().count() < 10_000 && r.starts_with("Torpor"), "PreToolUse: bad reason {r}");
        } else {
            let c = h.get("additionalContext").and_then(|x| x.as_str()).unwrap_or_else(|| panic!("{event}: no additionalContext in {}", o.stdout));
            assert!(c.chars().count() < 10_000 && c.starts_with("Torpor"), "{event}: bad additionalContext {c}");
        }
    }
}

// ---------------------------------------------------------------- oracle

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OEnt {
    /// d folder, f file, l symlink
    pub kind: char,
    pub exec: bool,
    /// BLAKE3 hex of the content (file) or of the link target (symlink)
    pub hash: String,
}

const EXCL: &[&str] = &[
    "node_modules", ".next", ".nuxt", ".svelte-kit", ".output", ".vercel", ".turbo", ".parcel-cache", ".cache", "dist", "build", "__pycache__", ".pytest_cache", ".mypy_cache", ".gradle",
    "coverage", ".terraform", "Pods", "DerivedData", ".dart_tool", ".torpor-stage",
];

fn excluded(dir: &Path, name: &str, is_dir: bool) -> bool {
    if name == ".git" {
        return true;
    }
    if is_dir {
        return EXCL.contains(&name)
            || (name == "target" && dir.join("Cargo.toml").exists())
            || ((name == "venv" || name == ".venv") && dir.join(name).join("pyvenv.cfg").exists());
    }
    (name.starts_with(".torpor-") && name.ends_with(".tmp")) || name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal")
}

/// What the project folder holds right now, with the same hard excludes as Torpor (no ignore files in test projects).
pub fn oracle(root: &Path) -> BTreeMap<String, OEnt> {
    fn walk(dir: &Path, rel: &str, out: &mut BTreeMap<String, OEnt>) {
        let mut items: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        items.sort_by_key(|e| e.file_name());
        for e in items {
            let name = e.file_name().to_string_lossy().into_owned();
            let md = std::fs::symlink_metadata(e.path()).unwrap();
            if excluded(dir, &name, md.is_dir()) {
                continue;
            }
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let exec = md.permissions().mode() & 0o111 != 0;
            if md.is_dir() {
                out.insert(path.clone(), OEnt { kind: 'd', exec: true, hash: String::new() });
                walk(&e.path(), &path, out);
            } else if md.file_type().is_symlink() {
                let t = std::fs::read_link(e.path()).unwrap();
                use std::os::unix::ffi::OsStrExt;
                out.insert(path, OEnt { kind: 'l', exec: false, hash: blake3::hash(t.as_os_str().as_bytes()).to_hex().to_string() });
            } else if md.is_file() {
                out.insert(path, OEnt { kind: 'f', exec, hash: blake3::hash(&std::fs::read(e.path()).unwrap()).to_hex().to_string() });
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, "", &mut out);
    out
}

/// Files and contents of the project at one moment (regular files only); the harness uses it to put things back.
pub type Snap = BTreeMap<String, (Vec<u8>, bool)>;

pub fn snapshot(root: &Path) -> Snap {
    oracle(root)
        .into_iter()
        .filter(|(_, e)| e.kind == 'f')
        .map(|(p, e)| {
            let b = std::fs::read(root.join(&p)).unwrap();
            (p, (b, e.exec))
        })
        .collect()
}

pub fn restore_snapshot(root: &Path, s: &Snap) {
    for (p, e) in oracle(root) {
        if e.kind == 'f' && !s.contains_key(&p) {
            std::fs::remove_file(root.join(&p)).unwrap();
        }
    }
    for (p, (bytes, exec)) in s {
        let abs = root.join(p);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, bytes).unwrap();
        std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(if *exec { 0o755 } else { 0o644 })).unwrap();
    }
}

// ---------------------------------------------------------------- the simulated session

pub struct Sim {
    pub env: Arc<Env>,
    pub sid: String,
    pub transcript: PathBuf,
    pub extra_env: Vec<(String, String)>,
    pub permission_mode: String,
    /// Run hooks through this program (the launcher) instead of the built binary.
    pub program: Option<PathBuf>,
    /// (event, wall time) of every checked hook call; tool events are labelled `Event(Tool)`.
    pub timings: Vec<(String, Duration)>,
    /// (event, stdout) of every checked hook call that answered with something: what Claude Code was given (B9 reads it).
    pub tape: Vec<(String, String)>,
    prompt_n: u32,
    tool_n: u32,
    pub pid: String,
    n: u32,
    /// Called with (false, label) just before and (true, label) just after every hook or command-line run
    /// (A6 uses it to look at file-system events around each Torpor process, not around the harness's own writes).
    pub observer: std::cell::RefCell<Option<Box<dyn FnMut(bool, &str)>>>,
}

impl Sim {
    pub fn new() -> Sim {
        Sim::with_env(Arc::new(Env::new()), 1)
    }

    pub fn with_env(env: Arc<Env>, n: u32) -> Sim {
        let sid = format!("sess-{n:04}-0000-4000-8000-00000000000{n}");
        let transcript = env.home.join(format!("transcript-{n}.jsonl"));
        Sim { env, sid, transcript, extra_env: Vec::new(), permission_mode: "default".into(), program: None, timings: Vec::new(), tape: Vec::new(), prompt_n: 0, tool_n: 0, pid: String::new(), n, observer: Default::default() }
    }

    /// A second Claude Code session in the same project and store.
    pub fn second_session(&self) -> Sim {
        let mut s = Sim::with_env(self.env.clone(), self.n + 1);
        s.extra_env = self.extra_env.clone();
        s
    }

    pub fn proj(&self) -> &Path {
        &self.env.proj
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.env.proj.join(rel)
    }

    fn cmd(&self) -> Command {
        let mut c = match &self.program {
            Some(p) => cmd_with(p, &self.env),
            None => base_cmd(&self.env),
        };
        for (k, v) in &self.extra_env {
            c.env(k, v);
        }
        c
    }

    /// Send one hook event. `extra` is merged into the standard payload. The answer is contract-checked.
    pub fn hook(&mut self, event: &str, extra: Value) -> Out {
        let o = self.hook_unchecked(event, extra);
        check_contract(event, &o);
        self.timings.push((event.to_string(), o.elapsed));
        if !o.stdout.trim().is_empty() {
            self.tape.push((event.to_string(), o.stdout.clone()));
        }
        o
    }

    pub fn payload(&self, event: &str, extra: Value) -> Value {
        let mut p = json!({
            "session_id": self.sid,
            "transcript_path": self.transcript,
            "cwd": self.env.proj,
            "hook_event_name": event,
            "permission_mode": self.permission_mode,
        });
        if event != "SessionStart" && !self.pid.is_empty() {
            p["prompt_id"] = json!(self.pid);
        }
        if let (Some(m), Some(e)) = (p.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                m.insert(k.clone(), v.clone());
            }
        }
        p
    }

    fn observed(&self, label: &str, c: Command, stdin: &[u8]) -> Out {
        if let Some(o) = self.observer.borrow_mut().as_mut() {
            o(false, label);
        }
        let out = run_cmd(c, stdin);
        if let Some(o) = self.observer.borrow_mut().as_mut() {
            o(true, label);
        }
        out
    }

    pub fn hook_unchecked(&mut self, event: &str, extra: Value) -> Out {
        let p = self.payload(event, extra);
        let mut c = self.cmd();
        c.args(["hook", event]);
        self.observed(event, c, p.to_string().as_bytes())
    }

    /// Run the command line (not a hook) in the project folder.
    pub fn cli(&self, args: &[&str]) -> Out {
        let mut c = self.cmd();
        c.args(args);
        self.observed(&format!("torpor {}", args.join(" ")), c, b"")
    }

    /// The command line with something on stdin (the status line gets Claude Code's JSON there).
    pub fn cli_stdin(&self, args: &[&str], stdin: &str) -> Out {
        let mut c = self.cmd();
        c.args(args);
        self.observed(&format!("torpor {}", args.join(" ")), c, stdin.as_bytes())
    }

    /// The command line with extra environment variables for this one run (fault injection).
    pub fn cli_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Out {
        self.cli_env_stdin(args, envs, "")
    }

    pub fn cli_env_stdin(&self, args: &[&str], envs: &[(&str, &str)], stdin: &str) -> Out {
        let mut c = self.cmd();
        c.args(args);
        for (k, v) in envs {
            c.env(k, v);
        }
        self.observed(&format!("torpor {}", args.join(" ")), c, stdin.as_bytes())
    }

    pub fn status(&self) -> Out {
        self.cli(&["status"])
    }

    // ---- events

    pub fn session_start(&mut self) -> Out {
        self.hook("SessionStart", json!({"source": "startup"}))
    }

    /// A typed prompt: starts a turn. The transcript gets the matching user line (turnOrigin human, typed).
    pub fn prompt(&mut self, text: &str) -> Out {
        self.prompt_with(text, "human", "typed")
    }

    /// A fresh prompt id (what Claude Code gives every typed prompt); returns it.
    pub fn next_pid(&mut self) -> String {
        self.prompt_n += 1;
        self.pid = format!("prompt-{}-{}", self.n, self.prompt_n);
        self.pid.clone()
    }

    pub fn prompt_with(&mut self, text: &str, origin: &str, source: &str) -> Out {
        self.next_pid();
        let line = json!({"type": "user", "uuid": format!("u-{}-{}", self.n, self.prompt_n), "promptId": self.pid, "turnOrigin": origin, "promptSource": source, "isSidechain": false});
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.transcript).unwrap();
        writeln!(f, "{line}").unwrap();
        self.hook("UserPromptSubmit", json!({"prompt": text}))
    }

    fn tool_id(&mut self) -> String {
        self.tool_n += 1;
        format!("toolu_{}_{}", self.n, self.tool_n)
    }

    fn agent_fields(agent: Option<&str>) -> Value {
        match agent {
            Some(a) => json!({"agent_id": a, "agent_type": "general-purpose"}),
            None => json!({}),
        }
    }

    fn tool_event(&mut self, event: &str, tool: &str, input: &Value, id: &str, agent: Option<&str>, more: Value) -> Out {
        let mut e = json!({"tool_name": tool, "tool_input": input, "tool_use_id": id});
        for (k, v) in Self::agent_fields(agent).as_object().unwrap().iter().chain(more.as_object().unwrap().iter()) {
            e[k] = v.clone();
        }
        let o = self.hook(event, e);
        if let Some(t) = self.timings.last_mut() {
            t.0 = format!("{event}({tool})");
        }
        o
    }

    /// An edit-family tool call: PreToolUse, the write, PostToolUse. Returns the last answer (the deny, if there was one).
    pub fn edit_tool(&mut self, tool: &str, rel: &str, content: &str, agent: Option<&str>) -> Out {
        let abs = self.path(rel);
        let input = json!({"file_path": abs});
        let id = self.tool_id();
        let pre = self.tool_event("PreToolUse", tool, &input, &id, agent, json!({}));
        if pre.json().is_some() {
            return pre;
        }
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, content).unwrap();
        self.tool_event("PostToolUse", tool, &input, &id, agent, json!({"tool_response": {"filePath": abs, "success": true}}))
    }

    pub fn edit(&mut self, rel: &str, content: &str) -> Out {
        self.edit_tool("Edit", rel, content, None)
    }

    /// The write and only the PostToolUse half of an edit-tool call: for tests that need a thousand edits and look at what that half says.
    pub fn quick_edit(&mut self, rel: &str, content: &str) -> Out {
        let abs = self.path(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, content).unwrap();
        let id = self.tool_id();
        self.tool_event("PostToolUse", "Write", &json!({"file_path": abs}), &id, None, json!({"tool_response": {"filePath": abs, "success": true}}))
    }

    pub fn write_tool(&mut self, rel: &str, content: &str) -> Out {
        self.edit_tool("Write", rel, content, None)
    }

    pub fn subagent_edit(&mut self, agent: &str, rel: &str, content: &str) -> Out {
        self.edit_tool("Write", rel, content, Some(agent))
    }

    /// A Bash tool call: PreToolUse, `sh -c`, then PostToolUse or PostToolUseFailure. Returns the exit code.
    pub fn bash_tool(&mut self, cmd: &str, agent: Option<&str>) -> i32 {
        let input = json!({"command": cmd});
        let id = self.tool_id();
        let pre = self.tool_event("PreToolUse", "Bash", &input, &id, agent, json!({}));
        if pre.json().is_some() {
            return -1;
        }
        let o = Command::new("sh").arg("-c").arg(cmd).current_dir(&self.env.proj).stdin(Stdio::null()).output().unwrap();
        let code = o.status.code().unwrap_or(-1);
        if code == 0 {
            let resp = json!({"stdout": String::from_utf8_lossy(&o.stdout), "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false});
            self.tool_event("PostToolUse", "Bash", &input, &id, agent, json!({"tool_response": resp}));
        } else {
            // Claude Code's own text: the exit code on the first line, then what the command printed.
            let shown = format!("{}{}", String::from_utf8_lossy(&o.stderr), String::from_utf8_lossy(&o.stdout));
            self.tool_event("PostToolUseFailure", "Bash", &input, &id, agent, json!({"error": format!("Exit code {code}\n{shown}"), "is_interrupt": false}));
        }
        code
    }

    pub fn bash(&mut self, cmd: &str) -> i32 {
        self.bash_tool(cmd, None)
    }

    /// Only the PreToolUse of a Bash call: what Torpor answers before the command would run. Nothing is run.
    pub fn pre_bash(&mut self, cmd: &str) -> Out {
        let id = self.tool_id();
        self.tool_event("PreToolUse", "Bash", &json!({"command": cmd}), &id, None, json!({}))
    }

    pub fn subagent_bash(&mut self, agent: &str, cmd: &str) -> i32 {
        self.bash_tool(cmd, Some(agent))
    }

    pub fn subagent_stop(&mut self, agent: &str) -> Out {
        self.hook("SubagentStop", json!({"agent_id": agent, "agent_type": "general-purpose", "agent_transcript_path": self.transcript, "stop_hook_active": false}))
    }

    /// A change nobody told Torpor about (another program, the user, git).
    pub fn outside_edit(&self, rel: &str, content: &str) {
        let abs = self.path(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, content).unwrap();
    }

    pub fn outside_remove(&self, rel: &str) {
        std::fs::remove_file(self.path(rel)).unwrap();
    }

    pub fn snapshot(&self) -> Snap {
        snapshot(&self.env.proj)
    }

    /// Put files back to a harness snapshot without any hook (a restore done by something else).
    pub fn rewind_to(&self, s: &Snap) {
        restore_snapshot(&self.env.proj, s);
    }

    pub fn stop(&mut self) -> Out {
        self.hook("Stop", json!({"stop_hook_active": false, "last_assistant_message": "done"}))
    }

    pub fn stop_with_background(&mut self, tasks: Value) -> Out {
        self.hook("Stop", json!({"stop_hook_active": false, "last_assistant_message": "done", "background_tasks": tasks}))
    }

    pub fn session_end(&mut self, reason: &str) -> Out {
        self.hook("SessionEnd", json!({"reason": reason}))
    }

    // ---- compaction (spec 21)

    /// One line in the fake transcript.
    pub fn transcript_line(&self, line: &Value) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.transcript).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    /// The compact_boundary record Claude Code writes when it compacts a conversation. A subagent's carries its agentId.
    pub fn boundary(&self, uuid: &str, agent: Option<&str>) {
        let mut l = json!({"type": "system", "subtype": "compact_boundary", "uuid": uuid});
        if let Some(a) = agent {
            l["agentId"] = json!(a);
            l["isSidechain"] = json!(true);
        }
        self.transcript_line(&l);
    }

    /// The attachment Claude Code writes into the transcript for a hook's additionalContext.
    pub fn attach(&self, hook: &str, text: &str) {
        self.transcript_line(&json!({"type": "attachment", "attachment": {"type": "hook_additional_context", "hookName": hook, "content": [text]}}));
    }

    /// A compaction of the main conversation: PreCompact, the boundary in the transcript, then SessionStart(compact). The answer is returned;
    /// the attachment is NOT written (call `attach` to play Claude Code putting the text in front of the AI).
    pub fn compaction(&mut self, uuid: &str) -> Out {
        self.hook("PreCompact", json!({"trigger": "auto"}));
        self.boundary(uuid, None);
        self.hook("SessionStart", json!({"source": "compact"}))
    }

    /// The additionalContext of an answer, if it has one.
    pub fn context_of(o: &Out) -> Option<String> {
        o.json()?.get("hookSpecificOutput")?.get("additionalContext")?.as_str().map(str::to_string)
    }

    // ---- the store, read from outside

    /// The workspace folder inside the store (the only one with a torpor.db).
    pub fn ws_dir(&self) -> PathBuf {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.env.thome)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.join("torpor.db").exists())
            .collect();
        assert_eq!(found.len(), 1, "expected one workspace in the store, found {found:?}");
        found.pop().unwrap()
    }

    pub fn conn(&self) -> rusqlite::Connection {
        let c = rusqlite::Connection::open_with_flags(self.ws_dir().join("torpor.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        c.busy_timeout(Duration::from_secs(5)).unwrap();
        c
    }

    pub fn q_i64(&self, sql: &str) -> i64 {
        self.conn().query_row(sql, [], |r| r.get::<_, Option<i64>>(0)).unwrap().unwrap_or(0)
    }

    pub fn q_str(&self, sql: &str) -> Option<String> {
        self.conn().query_row(sql, [], |r| r.get::<_, Option<String>>(0)).unwrap_or(None)
    }

    pub fn max_point(&self) -> i64 {
        self.q_i64("SELECT MAX(id) FROM points")
    }

    pub fn last_visible_point(&self) -> i64 {
        self.q_i64("SELECT MAX(id) FROM points WHERE visible=1")
    }

    /// (path, class) of every change recorded after point `mark`. Panics when a path shows up twice.
    pub fn changes_since(&self, mark: i64) -> BTreeMap<String, String> {
        let c = self.conn();
        let mut st = c.prepare("SELECT path,class FROM changes WHERE point_id>?1 ORDER BY point_id").unwrap();
        let mut out = BTreeMap::new();
        for r in st.query_map([mark], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap() {
            let (p, cl) = r.unwrap();
            assert!(out.insert(p.clone(), cl).is_none(), "path {p} changed twice since point {mark}");
        }
        out
    }

    /// Fault kinds recorded in the store plus the global log, oldest first.
    pub fn faults(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.env.thome.join("faults.log").exists() {
            for l in std::fs::read_to_string(self.env.thome.join("faults.log")).unwrap().lines() {
                v.push(l.to_string());
            }
        }
        v
    }

    /// The saved tree of one point must equal the project as it is on disk: same paths, same exec bits,
    /// every file's stored bytes equal to the file's bytes (read back, decrypted when secret).
    pub fn verify_point(&self, point_id: i64) {
        use torpor::store::{Key, Ws};
        let ws = Ws::open_in(&self.env.thome, &self.env.proj, false).unwrap();
        let tree: Vec<u8> = ws.conn.query_row("SELECT tree_id FROM points WHERE id=?1", [point_id], |r| r.get(0)).unwrap();
        let tree: [u8; 32] = tree.try_into().unwrap();
        let flat = torpor::capture::flatten_tree(&ws, &ws.conn, &tree).unwrap();
        let disk = oracle(&self.env.proj);
        let key = std::fs::read_to_string(&self.env.key).ok().and_then(|s| {
            let mut it = s.split_whitespace();
            (it.next() == Some("torpor-key-v1")).then(|| ())?;
            let id = it.next()?.to_string();
            let bytes = torpor::util::unhex32(it.next()?)?;
            Some(Key { bytes, id })
        });
        let mut bad: Vec<String> = Vec::new();
        let mut seen = BTreeSet::new();
        for (path, e) in &flat {
            seen.insert(path.clone());
            let Some(o) = disk.get(path) else {
                bad.push(format!("saved but not on disk: {path} ({})", e.kind));
                continue;
            };
            let id: Option<[u8; 32]> = torpor::util::unhex32(&e.id);
            match e.kind {
                'd' => {
                    if o.kind != 'd' {
                        bad.push(format!("{path}: saved as folder, on disk {}", o.kind));
                    }
                }
                'f' | 's' | 'l' => {
                    if (e.kind == 'l') != (o.kind == 'l') || o.kind == 'd' {
                        bad.push(format!("{path}: saved as {}, on disk {}", e.kind, o.kind));
                        continue;
                    }
                    if e.kind != 'l' && (e.mode == 755) != o.exec {
                        bad.push(format!("{path}: exec bit differs (saved {}, disk {})", e.mode, o.exec));
                    }
                    let want = std::fs::read(self.env.proj.join(path)).ok();
                    let want = if e.kind == 'l' {
                        use std::os::unix::ffi::OsStrExt;
                        std::fs::read_link(self.env.proj.join(path)).ok().map(|t| t.as_os_str().as_bytes().to_vec())
                    } else {
                        want
                    };
                    let got = match (e.kind, id) {
                        ('s', Some(id)) => ws.read_secret(&ws.conn, &id, key.as_ref()).ok(),
                        (_, Some(id)) => ws.read_blob(&ws.conn, &id).ok(),
                        _ => None,
                    };
                    if want != got {
                        bad.push(format!("{path}: saved bytes differ from the file (kind {})", e.kind));
                    }
                    if e.kind == 'f' && e.id != o.hash {
                        bad.push(format!("{path}: saved id {} but BLAKE3 of the file is {}", e.id, o.hash));
                    }
                }
                other => bad.push(format!("{path}: not saved ({other}:{})", e.id)),
            }
        }
        for p in disk.keys() {
            if !seen.contains(p) {
                bad.push(format!("on disk but not saved: {p}"));
            }
        }
        assert!(bad.is_empty(), "point {point_id} does not match the project:\n  {}", bad.join("\n  "));
    }

    pub fn verify_last_point(&self) {
        self.verify_point(self.last_visible_point());
    }
}

/// Write `files` (relative path, content) into a project folder.
pub fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (p, c) in files {
        let abs = root.join(p);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, c).unwrap();
    }
}

// ---------------------------------------------------------------- plugin layout (launcher tests)

pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The launcher's own mapping of uname to a dist folder.
pub fn dist_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("macos", "aarch64") => "darwin-arm64",
        _ => "unsupported",
    }
}

/// A copy of the plugin layout under `dir`: launcher, manifest, and either a copy of the built binary in
/// `dist/<target>/` or an empty `dist/`. Returns the plugin root.
pub fn make_plugin_root(dir: &Path, with_binary: bool) -> PathBuf {
    let root = dir.join("plugin-root");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
    std::fs::copy(manifest_dir().join("bin/torpor"), root.join("bin/torpor")).unwrap();
    std::fs::copy(manifest_dir().join(".claude-plugin/plugin.json"), root.join(".claude-plugin/plugin.json")).unwrap();
    if with_binary {
        let d = root.join("dist").join(dist_target());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::copy(BIN, d.join("torpor")).unwrap();
        std::fs::set_permissions(d.join("torpor"), std::fs::Permissions::from_mode(0o755)).unwrap();
    } else {
        std::fs::create_dir_all(root.join("dist")).unwrap();
    }
    root
}

/// Copy a folder tree (files, folders, symlinks, modes) with std only.
pub fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let md = std::fs::symlink_metadata(&src).unwrap();
        if md.is_dir() {
            copy_tree(&src, &dst);
        } else if md.file_type().is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src).unwrap(), &dst).unwrap();
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// Empty a folder but keep the folder itself (the open project directory of a running harness).
pub fn clear_dir(dir: &Path) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let md = std::fs::symlink_metadata(&p).unwrap();
        if md.is_dir() {
            std::fs::remove_dir_all(&p).unwrap();
        } else {
            std::fs::remove_file(&p).unwrap();
        }
    }
}

/// The store is intact: SQLite says ok, and every file a live point names is stored and readable.
pub fn assert_store_sound(env: &Env) {
    use torpor::store::Ws;
    let ws = Ws::open_in(&env.thome, &env.proj, false).unwrap();
    let ic: String = ws.conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
    assert_eq!(ic, "ok", "PRAGMA integrity_check");
    let mut st = ws.conn.prepare("SELECT id,tree_id FROM points WHERE dropped=0").unwrap();
    let rows: Vec<(i64, Vec<u8>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect();
    for (id, tree) in rows {
        let tree: [u8; 32] = tree.try_into().unwrap();
        let flat = torpor::capture::flatten_tree(&ws, &ws.conn, &tree).unwrap_or_else(|e| panic!("point {id}: tree unreadable: {e}"));
        for (path, e) in flat {
            if matches!(e.kind, 'f' | 'l' | 's') {
                let bid = torpor::util::unhex32(&e.id).unwrap();
                assert!(ws.blob_exists(&ws.conn, &bid), "point {id}: {path} names a missing blob");
                let loose: i64 = ws.conn.query_row("SELECT loose FROM blobs WHERE id=?1", [&bid[..]], |r| r.get(0)).unwrap();
                if loose != 0 {
                    assert!(ws.loose_path(&bid).exists(), "point {id}: {path}: loose file missing");
                }
            }
        }
    }
}
