//! Packaging (spec 9): the launcher's version, hooks.json, and the launcher's install steps.
mod common;
use common::*;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

fn read(rel: &str) -> String {
    std::fs::read_to_string(manifest_dir().join(rel)).unwrap()
}

#[test]
fn launcher_version_equals_manifest_and_cargo() {
    let plugin: Value = serde_json::from_str(&read(".claude-plugin/plugin.json")).unwrap();
    let launcher = read("bin/torpor");
    let v = launcher.lines().find_map(|l| l.strip_prefix("V=")).expect("launcher has a V= line").trim().to_string();
    assert_eq!(plugin["version"].as_str(), Some(v.as_str()), "launcher V vs plugin.json version");
    assert_eq!(v, env!("CARGO_PKG_VERSION"), "launcher V vs Cargo version");
    assert_eq!(plugin["name"], "torpor");
    let mode = std::fs::metadata(manifest_dir().join("bin/torpor")).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "launcher must be executable");
}

#[test]
fn hooks_json_matches_the_spec() {
    let h: Value = serde_json::from_str(&read("hooks/hooks.json")).unwrap();
    let hooks = h["hooks"].as_object().unwrap();
    let table = [
        ("SessionStart", 10, false),
        ("UserPromptSubmit", 10, false),
        ("PreToolUse", 5, true),
        ("PostToolUse", 5, true),
        ("PostToolUseFailure", 5, true),
        ("Stop", 30, false),
        ("StopFailure", 10, false),
        ("SubagentStop", 5, false),
        ("PreCompact", 5, false),
        ("SessionEnd", 2, false),
    ];
    assert_eq!(hooks.len(), table.len(), "exactly the ten events");
    for (ev, timeout, matcher) in table {
        let groups = hooks[ev].as_array().unwrap_or_else(|| panic!("{ev} missing"));
        assert_eq!(groups.len(), 1, "{ev}");
        let g = &groups[0];
        assert_eq!(g.get("matcher").and_then(|m| m.as_str()), if matcher { Some("Bash|Edit|Write|MultiEdit|NotebookEdit") } else { None }, "{ev} matcher");
        let cmds = g["hooks"].as_array().unwrap();
        assert_eq!(cmds.len(), 1, "{ev}");
        let c = &cmds[0];
        assert_eq!(c["type"], "command", "{ev}");
        assert_eq!(c["command"], "${CLAUDE_PLUGIN_ROOT}/bin/torpor", "{ev}");
        assert_eq!(c["onFailure"], "continue", "{ev}");
        assert_eq!(c["args"], json!(["hook", ev]), "{ev}");
        assert_eq!(c["timeout"], timeout, "{ev} timeout");
        assert!(c.get("async").is_none() && c.get("asyncRewake").is_none(), "{ev}: no async");
    }
    // The timeout in hooks.json must always be longer than the binary's own deadline (spec 3.2).
    for (ev, timeout, _) in table {
        let own = torpor::hook::own_deadline_ms(ev).unwrap();
        assert!(own < timeout as u64 * 1000, "{ev}: own deadline {own} ms vs timeout {timeout} s");
    }
}

#[test]
fn launcher_installs_then_takes_the_fast_path() {
    let sim = Sim::new();
    let root = make_plugin_root(sim.env._tmp.path(), true);
    let v = env!("CARGO_PKG_VERSION");
    let launcher = root.join("bin/torpor");
    let version = |program: &std::path::Path| {
        let mut c = cmd_with(program, &sim.env);
        c.arg("version");
        run_cmd(c, b"")
    };

    // First run (a terminal command): copies the binary, writes the shim, runs the copy.
    let o = version(&launcher);
    assert_eq!((o.code, o.stdout.trim().to_string()), (0, format!("torpor {v}")), "stderr: {}", o.stderr);
    let installed = sim.env.thome.join("bin").join(v).join("torpor");
    assert!(installed.exists(), "binary installed at {installed:?}");
    assert_eq!(std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777, 0o755);
    let shim_path = sim.env.thome.join("bin/torpor");
    let shim = std::fs::read_to_string(&shim_path).unwrap();
    assert!(shim.starts_with("#!/bin/sh\n") && shim.contains(&format!("{}/bin/{v}/torpor", sim.env.thome.display())), "shim: {shim}");
    assert_eq!(std::fs::metadata(&shim_path).unwrap().permissions().mode() & 0o111, 0o111);
    assert_eq!(std::fs::metadata(&sim.env.thome).unwrap().permissions().mode() & 0o777, 0o700, "the store folder is private");

    // Second run: the shipped binary is gone, the installed copy still answers (fast path), and so does the shim.
    std::fs::remove_dir_all(root.join("dist")).unwrap();
    let o = version(&launcher);
    assert_eq!((o.code, o.stdout.trim().to_string()), (0, format!("torpor {v}")), "fast path");
    let o = version(&shim_path);
    assert_eq!((o.code, o.stdout.trim().to_string()), (0, format!("torpor {v}")), "shim");
}

#[test]
fn launcher_hook_through_installed_copy_records_save_points() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A1\n")]);
    let root = make_plugin_root(sim.env._tmp.path(), true);
    let launcher = root.join("bin/torpor");
    for (ev, extra) in [("SessionStart", json!({"source": "startup"})), ("UserPromptSubmit", json!({"prompt": "hello"}))] {
        sim.pid = "p1".into();
        let mut c = cmd_with(&launcher, &sim.env);
        c.args(["hook", ev]);
        let o = run_cmd(c, sim.payload(ev, extra).to_string().as_bytes());
        check_contract(ev, &o);
    }
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM points"), 1, "the launcher ran the real hook");
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM turns"), 1);
}
