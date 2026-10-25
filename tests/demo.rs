//! The offline demo (demo/run.sh) tells the whole story with the real binary: it must keep doing so.
use std::process::Command;

#[test]
fn the_demo_runs_offline_and_tells_the_story() {
    let o = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/demo/run.sh"))
        .env("TORPOR_BIN", env!("CARGO_BIN_EXE_torpor"))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        o.status.success(),
        "{out}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    for line in [
        "rewind left 1 shell-changed file from turn 4: server.js",
        "Turn 4 broke it",
        "Checked first in a scratch copy, still passing: #1 (login works).",
        "Undoing turn 2 breaks promise #1 (login works), so nothing was changed.",
        "\"decision\":\"block\"",
        "the turn is allowed to end",
        "demo done",
    ] {
        assert!(out.contains(line), "missing {line:?} in:\n{out}");
    }
}
