//! A9 (spec section 7): save points before destructive commands, and ask or deny for commands that reach outside.
//! Remote commands are only ever sent to the hook as text (`pre_bash`); the sim never runs them.
mod common;
use common::*;
use serde_json::{json, Value};

const PROTECTED: &str = "Torpor: this file belongs to Torpor's save points; the AI can't change it.";

fn answer(decision: &str, reason: String) -> Value {
    json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": decision, "permissionDecisionReason": reason}})
}

fn ask(rule: &str) -> Value {
    answer("ask", format!("Torpor: this command can delete data outside this computer ({rule}). Save points cannot bring that back. Approve only if you meant it."))
}

fn deny(rule: &str) -> Value {
    answer(
        "deny",
        format!("Torpor stopped this because this session skips permission prompts and the command can delete data outside this computer ({rule}). Save points cannot undo it. If the user wants it, they can run it in their own terminal."),
    )
}

fn pre_bash_points(sim: &Sim) -> i64 {
    sim.q_i64("SELECT COUNT(*) FROM points WHERE kind='pre_bash'")
}

fn started() -> Sim {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("src/a.rs", "A\n"), ("src/deep/b.rs", "B\n"), ("keep.txt", "K\n"), ("old.txt", "O\n")]);
    sim.session_start();
    sim.prompt("work");
    sim
}

#[test]
fn rm_rf_gets_a_save_point_and_back_one_brings_it_home() {
    let mut sim = started();
    sim.edit("keep.txt", "K2\n");
    let before = oracle(sim.proj());
    assert_eq!(sim.bash("rm -rf src"), 0);
    assert!(!sim.path("src").exists());
    assert_eq!(pre_bash_points(&sim), 1);
    let pb = sim.q_i64("SELECT id FROM points WHERE kind='pre_bash'");
    assert_eq!(sim.q_i64(&format!("SELECT visible FROM points WHERE id={pb}")), 1, "the save point is one the user can go back to");
    assert!(sim.q_str(&format!("SELECT note FROM points WHERE id={pb}")).unwrap().contains("rm -rf src"));
    // The end-of-turn line says where the command was saved.
    let stop = sim.stop();
    let line = stop.json().unwrap()["systemMessage"].as_str().unwrap().to_string();
    assert!(line.contains(" · saved before rm -rf src"), "{line}");
    // `back 1` is that point, and the folder is exactly what it was.
    let o = sim.cli(&["back", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.starts_with(&format!("Going back to save point {pb} ")), "{}", o.stdout);
    assert_eq!(oracle(sim.proj()), before, "src is back and nothing else moved");
    sim.verify_point(pb);
}

#[test]
fn harmless_commands_make_no_such_point() {
    let mut sim = started();
    for c in ["cat keep.txt", "echo hi > out.txt", "mkdir -p x/y", "ls -la", "printf 'one line' > note.txt", "cp keep.txt copy.txt", "echo 'rm is a word here'"] {
        assert_eq!(sim.bash(c), 0, "{c}");
    }
    assert_eq!(pre_bash_points(&sim), 0);
    sim.edit("keep.txt", "K3\n");
    assert_eq!(pre_bash_points(&sim), 0, "an edit tool never makes one");
}

#[test]
fn other_local_triggers_each_get_one_point() {
    let mut sim = started();
    write_files(sim.proj(), &[("pkg/package.json", r#"{"scripts":{"clean":"rm -rf dist"}}"#), ("dist/x.js", "x\n")]);
    let mut n = 0;
    for c in ["mv old.txt older.txt", "bash -c 'rm older.txt'", "find dist -name '*.js' -delete", "truncate -s 0 keep.txt", "env FOO=1 rm -f nothing"] {
        sim.bash(c);
        n += 1;
        assert_eq!(pre_bash_points(&sim), n, "{c}");
    }
    sim.pre_bash("npm run clean");
    assert_eq!(pre_bash_points(&sim), n, "the script lives in a folder below, where npm would not look");
    write_files(sim.proj(), &[("package.json", r#"{"scripts":{"clean":"rm -rf dist"}}"#)]);
    sim.pre_bash("npm run clean");
    assert_eq!(pre_bash_points(&sim), n + 1, "the script is read from package.json");
}

#[test]
fn remote_commands_ask_in_prompting_sessions() {
    for (cmd, rule) in [
        ("terraform destroy", "terraform destroy"),
        ("terraform apply -auto-approve", "terraform apply -auto-approve"),
        ("psql -c \"DROP TABLE users\"", "SQL DROP TABLE"),
        ("git push --force origin main", "git push --force"),
        ("prisma migrate reset --force", "prisma migrate reset"),
    ] {
        for mode in ["default", "acceptEdits", "plan"] {
            let mut sim = started();
            sim.permission_mode = mode.into();
            let o = sim.pre_bash(cmd);
            assert_eq!(o.json().unwrap(), ask(rule), "{cmd} in {mode}");
            assert_eq!(pre_bash_points(&sim), 1, "a save point is written even when the answer is ask: {cmd}");
        }
    }
}

#[test]
fn remote_commands_are_denied_where_nobody_can_be_asked() {
    for mode in ["bypassPermissions", "dontAsk", "auto", "somethingNew", ""] {
        let mut sim = started();
        sim.permission_mode = mode.into();
        assert_eq!(sim.pre_bash("terraform destroy").json().unwrap(), deny("terraform destroy"), "{mode:?}");
        assert_eq!(sim.pre_bash("psql -c \"DROP TABLE users\"").json().unwrap(), deny("SQL DROP TABLE"), "{mode:?}");
        assert_eq!(pre_bash_points(&sim), 0, "a denied command changes nothing, so no save point");
    }
    // A local delete is never denied, whatever the mode.
    let mut sim = started();
    sim.permission_mode = "bypassPermissions".into();
    assert_eq!(sim.bash("rm -rf src"), 0);
    assert_eq!(pre_bash_points(&sim), 1);
}

#[test]
fn torpors_own_files_cannot_be_changed_from_the_shell() {
    let mut sim = started();
    let store = sim.env.thome.display().to_string();
    let key = sim.env.key.display().to_string();
    let keydir = sim.env.key.parent().unwrap().display().to_string();
    for c in [
        format!("rm -rf {store}"),
        format!("mv {store} {store}.old"),
        format!("cp {key} stolen.txt"),
        format!("echo x > {key}"),
        format!("rm -rf {keydir}"),
        format!("sqlite3 {store}/ws/x/db 'delete from points'"),
        format!("cd {store} && rm -rf ws"),
        format!("bash -c 'truncate -s 0 {key}'"),
    ] {
        assert_eq!(sim.pre_bash(&c).json().unwrap(), answer("deny", PROTECTED.into()), "{c}");
    }
    assert!(sim.env.thome.exists(), "the sim never runs a command Torpor answered");
    assert_eq!(pre_bash_points(&sim), 0);
    // Looking is fine, and so is running Torpor itself.
    for c in [format!("ls {store}"), format!("{store}/bin/torpor status > out.txt"), "ls .torpor-nothing".to_string()] {
        assert!(sim.pre_bash(&c).json().is_none(), "{c}");
    }
}

#[test]
fn off_on_forget_retain_are_for_the_user_only() {
    let mut sim = started();
    for (c, name) in [
        ("torpor off", "torpor off"),
        ("torpor on", "torpor on"),
        ("~/.torpor/bin/torpor forget --all", "torpor forget"),
        ("sudo torpor retain 90", "torpor retain"),
        ("cd . && TORPOR_HOME=/x torpor forget", "torpor forget"),
        ("torpor undo 2 --force", "torpor undo --force"),
        ("bash -c 'cd . && torpor undo --force 2'", "torpor undo --force"),
    ] {
        let want = answer("deny", format!("Torpor: only the user can do this, in their own terminal ({name})."));
        assert_eq!(sim.pre_bash(c).json().unwrap(), want, "{c}");
    }
    for c in ["torpor status", "torpor back 1", "torpor undo 2 --dry-run", "torpor quiet on", "torpor quiet off", "torpor gc", "echo torpor off"] {
        assert!(sim.pre_bash(c).json().is_none(), "{c}");
    }
}

#[test]
fn protections_hold_while_torpor_is_off_and_nothing_else_happens() {
    let mut sim = started();
    sim.extra_env.push(("TORPOR_OFF".into(), "1".into()));
    let store = sim.env.thome.display().to_string();
    assert_eq!(sim.pre_bash(&format!("rm -rf {store}")).json().unwrap(), answer("deny", PROTECTED.into()));
    let user_only = answer("deny", "Torpor: only the user can do this, in their own terminal (torpor forget).".into());
    assert_eq!(sim.pre_bash("torpor forget").json().unwrap(), user_only);
    let before = sim.max_point();
    assert!(sim.pre_bash("rm -rf src").json().is_none());
    assert!(sim.pre_bash("terraform destroy").json().is_none(), "paused: no questions either");
    assert_eq!(sim.max_point(), before);
    assert_eq!(pre_bash_points(&sim), 0);
    // The edit tools still cannot reach the store.
    let o = sim.edit_tool("Write", &format!("{store}/x.txt"), "x", None);
    assert_eq!(o.json().unwrap(), answer("deny", PROTECTED.into()));
}

#[test]
fn only_bash_commands_are_read() {
    let mut sim = started();
    let id = "toolu_x";
    let o = sim.hook("PreToolUse", json!({"tool_name": "Read", "tool_input": {"file_path": "rm -rf src"}, "tool_use_id": id}));
    assert!(o.json().is_none());
    let o = sim.hook("PreToolUse", json!({"tool_name": "Bash", "tool_input": {}, "tool_use_id": id}));
    assert!(o.json().is_none());
    assert_eq!(pre_bash_points(&sim), 0);
}

#[test]
fn a_busy_store_loses_neither_the_deny_nor_the_question() {
    // A restore holds the store: the hook cannot save anything, but what it says about the command stays.
    let mut sim = started();
    let store = sim.env.thome.display().to_string();
    let mut ws = torpor::store::Ws::open_in(&sim.env.thome, &sim.env.proj, false).unwrap();
    ws.lock_exclusive(1000).unwrap();
    assert_eq!(sim.pre_bash(&format!("rm -rf {store}")).json().unwrap(), answer("deny", PROTECTED.into()));
    assert_eq!(sim.pre_bash("terraform destroy").json().unwrap(), ask("terraform destroy"));
    sim.permission_mode = "bypassPermissions".into();
    assert_eq!(sim.pre_bash("terraform destroy").json().unwrap(), deny("terraform destroy"));
    ws.unlock();
    assert_eq!(pre_bash_points(&sim), 0, "nothing could be saved while the store was held");
}
