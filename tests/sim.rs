//! A1: attribution. Every change must get the class the spec says (4.3), in fast and slow mode.
mod common;
use common::*;
use serde_json::json;
use std::collections::BTreeMap;

fn want(rows: &[(&str, &str)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(p, c)| (p.to_string(), c.to_string()))
        .collect()
}

fn seed(sim: &Sim) {
    write_files(
        sim.proj(),
        &[
            ("a.txt", "A1\n"),
            ("b.txt", "B1\n"),
            ("c.txt", "C1\n"),
            ("d.txt", "D1\n"),
            ("notes.md", "notes\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("run.sh", "echo run\n"),
        ],
    );
}

/// The whole table in one session. `mode` is "fast" or "slow". In slow mode a turn with both a main-agent Bash and a
/// subagent Bash cannot be told apart (no scan between them), so the subagent Bash step is left out there.
fn full_scenario(mode: &str) {
    let mut sim = Sim::new();
    sim.extra_env.push(("TORPOR_TEST_MODE".into(), mode.into()));
    seed(&sim);
    sim.session_start();

    // ---- turn 1: every in-turn class
    sim.prompt("change many things");
    let mark = sim.max_point();
    sim.write_tool("a.txt", "A2\n");
    sim.edit("b.txt", "B2\n");
    assert_eq!(sim.bash("perl -pi -e 's/C1/C2/' c.txt"), 0);
    assert_eq!(sim.bash("rm d.txt"), 0);
    assert_eq!(sim.bash("echo x > e.txt"), 0);
    assert_eq!(sim.bash("chmod +x run.sh"), 0);
    assert_ne!(
        sim.bash("echo partial > f.txt; exit 3"),
        0,
        "a failing command still changed a file"
    );
    sim.subagent_edit("agent-1", "sub/s1.txt", "S1\n");
    let mut t1 = vec![
        ("a.txt", "edit"),
        ("b.txt", "edit"),
        ("c.txt", "shell"),
        ("d.txt", "shell"),
        ("e.txt", "shell"),
        ("f.txt", "shell"),
        ("run.sh", "shell"),
        ("sub/s1.txt", "subagent"),
    ];
    if mode == "fast" {
        assert_eq!(sim.subagent_bash("agent-1", "echo s2 > sub/s2.txt"), 0);
        t1.push(("sub/s2.txt", "subagent"));
    }
    let stop = sim.stop();
    assert_eq!(sim.changes_since(mark), want(&t1), "turn 1 ({mode})");
    let line = stop.text();
    assert!(
        line.starts_with("Torpor: save point ")
            && line.contains(&format!("turn 1 changed {} files", t1.len()))
            && line.contains("undo it: torpor undo 1")
            && line.ends_with(" · saved before rm d.txt"),
        "stop line: {line}"
    );
    sim.verify_last_point();
    let tool_points = sim.q_i64("SELECT COUNT(*) FROM points WHERE kind='tool'");
    if mode == "fast" {
        assert!(
            tool_points >= 4,
            "fast mode scans after every Bash ({tool_points} tool points)"
        );
    } else {
        assert_eq!(tool_points, 0, "slow mode makes no per-Bash save points");
    }

    // ---- a change nobody announced, between turns: outside
    sim.outside_edit("notes.md", "edited by hand\n");
    let mark = sim.max_point();
    sim.prompt("turn two");
    assert_eq!(
        sim.changes_since(mark),
        want(&[("notes.md", "outside")]),
        "outside edit ({mode})"
    );
    sim.write_tool("a.txt", "A3\n");
    sim.stop();
    sim.verify_last_point();

    // ---- files put back to an earlier save point, between turns: rewind
    let snap = sim.snapshot();
    sim.prompt("turn three");
    sim.write_tool("a.txt", "A4\n");
    sim.write_tool("new.txt", "N\n");
    assert_eq!(sim.bash("rm c.txt"), 0);
    sim.stop();
    sim.rewind_to(&snap);
    let mark = sim.max_point();
    sim.prompt("turn four");
    assert_eq!(
        sim.changes_since(mark),
        want(&[
            ("a.txt", "rewind"),
            ("new.txt", "rewind"),
            ("c.txt", "rewind")
        ]),
        "rewind ({mode})"
    );
    assert!(
        sim.q_str("SELECT note FROM points WHERE note LIKE 'rewind:%' ORDER BY id DESC LIMIT 1")
            .is_some(),
        "the rewind note names the save point"
    );
    // turn four ends with a background task still running
    sim.stop_with_background(json!([{"id": "bg-1", "command": "npm run dev"}]));

    // ---- changed while a background task ran: background
    sim.outside_edit("bg.log", "tick\n");
    let mark = sim.max_point();
    sim.prompt("turn five");
    assert_eq!(
        sim.changes_since(mark),
        want(&[("bg.log", "background")]),
        "background ({mode})"
    );
    sim.stop();
    sim.verify_last_point();

    // ---- the same kind of change after a turn without background tasks is outside
    sim.outside_edit("late.txt", "late\n");
    let mark = sim.max_point();
    sim.prompt("turn six");
    assert_eq!(
        sim.changes_since(mark),
        want(&[("late.txt", "outside")]),
        "outside after background ended ({mode})"
    );

    // ---- a second session works while turn six is open: its shell edit is shared, its Write is an edit
    let mark = sim.max_point();
    let mut b = sim.second_session();
    b.session_start();
    b.prompt("other window");
    assert_eq!(b.bash("echo g > g.txt"), 0);
    b.write_tool("h.txt", "H\n");
    b.stop();
    sim.stop();
    assert_eq!(
        sim.changes_since(mark),
        want(&[("g.txt", "shared"), ("h.txt", "edit")]),
        "two sessions ({mode})"
    );
    sim.verify_last_point();
    sim.session_end("other");
}

#[test]
fn a1_all_classes_fast() {
    full_scenario("fast");
}

#[test]
fn a1_all_classes_slow() {
    full_scenario("slow");
}

/// A turn with only Bash commands: every change is shell, in both scan modes.
fn bash_only(mode: &str) {
    let mut sim = Sim::new();
    sim.extra_env.push(("TORPOR_TEST_MODE".into(), mode.into()));
    seed(&sim);
    sim.session_start();
    sim.prompt("shell only");
    let mark = sim.max_point();
    for cmd in [
        "perl -pi -e 's/A1/A9/' a.txt", // not sed -i: BSD sed (macOS) needs a suffix argument
        "rm b.txt",
        "echo new > created.txt",
        "mv c.txt moved.txt",
        "mkdir -p deep/er && echo 1 > deep/er/f.txt",
        "printf 'x' >> d.txt",
        "chmod +x notes.md",
        "cp src/main.rs src/copy.rs",
    ] {
        assert_eq!(sim.bash(cmd), 0, "{cmd}");
    }
    sim.stop();
    let got = sim.changes_since(mark);
    let expect = want(&[
        ("a.txt", "shell"),
        ("b.txt", "shell"),
        ("created.txt", "shell"),
        ("c.txt", "shell"),
        ("moved.txt", "shell"),
        ("deep/er/f.txt", "shell"),
        ("d.txt", "shell"),
        ("notes.md", "shell"),
        ("src/copy.rs", "shell"),
    ]);
    assert_eq!(got, expect, "bash only ({mode})");
    sim.verify_last_point();
}

#[test]
fn a1_bash_only_fast() {
    bash_only("fast");
}

#[test]
fn a1_bash_only_slow() {
    bash_only("slow");
}
