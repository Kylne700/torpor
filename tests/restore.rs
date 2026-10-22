//! Restore (spec 6.1 to 6.3): `torpor back`, its report, modes, databases, refusals and the pinned save point.
mod common;
use common::*;
use std::os::unix::fs::PermissionsExt;

fn two_turns() -> (Sim, std::collections::BTreeMap<String, OEnt>, std::collections::BTreeMap<String, OEnt>) {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A1\n"), ("b.txt", "B1\n"), ("keep/k.txt", "K\n"), ("run.sh", "echo run\n")]);
    sim.session_start();
    sim.prompt("turn one");
    sim.edit("a.txt", "A2\n");
    sim.stop();
    let end1 = oracle(sim.proj());
    sim.prompt("turn two");
    sim.edit("a.txt", "A3\n");
    assert_eq!(sim.bash("rm b.txt"), 0);
    assert_eq!(sim.bash("mkdir -p new/deep && echo n > new/deep/n.txt"), 0);
    sim.edit("c.txt", "C\n");
    sim.stop();
    let end2 = oracle(sim.proj());
    assert_ne!(end1, end2);
    (sim, end1, end2)
}

#[test]
fn back_one_then_back_again() {
    let (sim, end1, end2) = two_turns();
    // Turn two ran `rm b.txt`, which has its own save point, so the end of turn 1 is asked for by turn.
    let o = sim.cli(&["back", "--turn", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(oracle(sim.proj()), end1, "the folder is the end of turn 1");
    assert!(o.stdout.starts_with("Going back to save point "), "{}", o.stdout);
    assert!(o.stdout.contains("Changes 4 files: 1 restored, 2 removed (c.txt, new/deep/n.txt)") || o.stdout.contains("Changes 3 files"), "{}", o.stdout);
    assert!(o.stdout.contains("brought back (b.txt)"), "{}", o.stdout);
    let done = o.stdout.lines().last().unwrap();
    assert!(done.starts_with("Done. To undo this: torpor back --id "), "{done}");
    let pre: i64 = done.rsplit(' ').next().unwrap().parse().unwrap();
    assert_eq!(sim.q_i64(&format!("SELECT pinned FROM points WHERE id={pre}")), 1, "the pre-restore point is pinned");
    assert_eq!(sim.q_str(&format!("SELECT kind FROM points WHERE id={pre}")).as_deref(), Some("pre_restore"));
    // The new point is a fresh scan of what the folder really is, with class restore.
    sim.verify_last_point();
    assert_eq!(sim.q_str("SELECT kind FROM points ORDER BY id DESC LIMIT 1").as_deref(), Some("restored"));
    assert!(sim.q_i64("SELECT COUNT(*) FROM changes WHERE class='restore'") > 0);
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM restores WHERE status='done'"), 1);
    // Nothing is left behind.
    assert!(!sim.proj().join(".torpor-stage").exists());
    assert_eq!(std::fs::read_dir(sim.ws_dir().join("journal")).unwrap().count(), 0);
    // Undo of the restore is `back 1` straight after it.
    let o2 = sim.cli(&["back", "1"]);
    assert_eq!(o2.code, 0, "{}{}", o2.stdout, o2.stderr);
    assert_eq!(oracle(sim.proj()), end2, "back 1 undid the restore");
    // And the printed hint works too.
    let o3 = sim.cli(&["back", "--id", &pre.to_string()]);
    assert_eq!(o3.code, 0, "{}{}", o3.stdout, o3.stderr);
    assert_eq!(oracle(sim.proj()), end2);
}

#[test]
fn back_by_turn_and_by_point_and_refusals() {
    let (mut sim, end1, _) = two_turns();
    // The turn that is still running cannot be a target.
    sim.prompt("turn three");
    let open = sim.cli(&["back", "--turn", "3"]);
    assert_eq!(open.code, 3, "{}{}", open.stdout, open.stderr);
    assert!(open.stdout.contains("still running"), "{}", open.stdout);
    sim.stop();
    let o = sim.cli(&["back", "--turn", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(oracle(sim.proj()), end1);
    assert!(o.stdout.contains("end of turn 1"), "{}", o.stdout);
    for (args, code) in [(vec!["back", "--turn", "99"], 5), (vec!["back", "--id", "99999"], 5), (vec!["back", "99"], 5), (vec!["back"], 2), (vec!["back", "0"], 2), (vec!["back", "1", "2"], 2), (vec!["back", "--turn"], 2)] {
        let o = sim.cli(&args);
        assert_eq!(o.code, code, "{args:?}: {}{}", o.stdout, o.stderr);
    }
}

#[test]
fn dry_run_changes_nothing_and_leaves_no_point() {
    let (sim, _, end2) = two_turns();
    let points = sim.q_i64("SELECT COUNT(*) FROM points");
    let o = sim.cli(&["back", "1", "--dry-run"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("Would go back to") && o.stdout.contains("Would change") && o.stdout.contains("dry run"), "{}", o.stdout);
    assert!(!o.stdout.contains("torpor back --id"), "a dry run prints no pre-restore id: {}", o.stdout);
    assert_eq!(oracle(sim.proj()), end2);
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM points"), points, "the rolled-back pre-restore point is gone");
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM restores"), 0);
}

#[test]
fn nothing_to_do_leaves_no_pinned_point() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A\n")]);
    sim.session_start();
    sim.prompt("t1");
    sim.stop();
    let o = sim.cli(&["back", "1"]);
    assert_eq!(o.code, 5, "{}{}", o.stdout, o.stderr);
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM points WHERE pinned=1"), 0);
    // The same state again: a point whose tree equals the live tree is no state to go back to.
    let (sim2, _, _) = two_turns();
    let id = sim2.q_i64("SELECT MAX(id) FROM points WHERE kind='end'");
    let o2 = sim2.cli(&["back", "--id", &id.to_string()]);
    assert_eq!(o2.code, 0);
    assert!(o2.stdout.contains("already match"), "{}", o2.stdout);
    assert_eq!(sim2.q_i64("SELECT COUNT(*) FROM points WHERE pinned=1"), 0);
}

#[test]
fn modes_secrets_symlinks_and_empty_folders_come_back() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A\n"), (".env", "TOKEN=abc123\n"), ("bin/run.sh", "echo hi\n"), ("data/blob.bin", "\u{0}\u{1}\u{2}")]);
    std::fs::set_permissions(sim.path("bin/run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("a.txt", sim.path("link")).unwrap();
    std::os::unix::fs::symlink("a.txt", sim.path("link2")).unwrap();
    std::fs::create_dir_all(sim.path("empty/inner")).unwrap();
    sim.session_start();
    sim.prompt("turn one");
    sim.edit("a.txt", "A2\n");
    sim.stop();
    let before = oracle(sim.proj());
    sim.prompt("turn two");
    // link2 points somewhere else now: the restore has to point it back (a link that still exists is replaced, not created).
    assert_eq!(sim.bash("rm -rf bin empty link .env data; ln -sfn .env link2"), 0);
    sim.stop();
    assert!(!sim.path(".env").exists());
    let o = sim.cli(&["back", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(oracle(sim.proj()), before, "{}", o.stdout);
    assert_eq!(std::fs::read_to_string(sim.path(".env")).unwrap(), "TOKEN=abc123\n");
    let mode = |p: &str| std::fs::symlink_metadata(sim.path(p)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(".env"), 0o600, "secret files come back private");
    assert_eq!(mode("bin/run.sh") & 0o111, 0o111, "the exec bit comes back");
    assert!(std::fs::symlink_metadata(sim.path("link")).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_link(sim.path("link2")).unwrap().to_str(), Some("a.txt"));
    assert!(sim.path("empty/inner").is_dir());
}

#[test]
fn a_file_that_exists_keeps_its_mode_and_takes_the_exec_bit_from_the_tree() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("s.sh", "echo 1\n"), ("g.txt", "g1\n")]);
    std::fs::set_permissions(sim.path("s.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(sim.path("g.txt"), std::fs::Permissions::from_mode(0o664)).unwrap();
    sim.session_start();
    sim.prompt("one");
    sim.edit("x.txt", "x\n");
    sim.stop();
    sim.prompt("two");
    sim.edit("s.sh", "echo 2\n");
    assert_eq!(sim.bash("chmod 600 s.sh g.txt; echo g2 > g.txt"), 0);
    sim.stop();
    let o = sim.cli(&["back", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let mode = |p: &str| std::fs::metadata(sim.path(p)).unwrap().permissions().mode() & 0o777;
    // s.sh was 755 in the tree: the exec bit is set on top of the live read/write bits (0600 -> 0700).
    assert_eq!(mode("s.sh"), 0o700);
    // g.txt was 644 in the tree: no exec bit, live bits kept.
    assert_eq!(mode("g.txt"), 0o600);
    assert_eq!(std::fs::read_to_string(sim.path("s.sh")).unwrap(), "echo 1\n");
}

#[test]
fn databases_are_left_alone_unless_asked() {
    let mut sim = Sim::new();
    let dbp = sim.path("dev.db");
    let db = |n: i64| {
        let c = rusqlite::Connection::open(&dbp).unwrap();
        c.execute_batch("CREATE TABLE IF NOT EXISTS t(x INTEGER)").unwrap();
        c.execute("INSERT INTO t VALUES(?1)", [n]).unwrap();
    };
    let rows = |p: &std::path::Path| rusqlite::Connection::open(p).unwrap().query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0)).unwrap();
    write_files(sim.proj(), &[("a.txt", "A1\n")]);
    db(1);
    sim.session_start();
    sim.prompt("one");
    sim.edit("a.txt", "A2\n");
    sim.stop();
    sim.prompt("two");
    sim.edit("a.txt", "A3\n");
    db(2);
    sim.stop();
    let o = sim.cli(&["back", "--turn", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(std::fs::read_to_string(sim.path("a.txt")).unwrap(), "A2\n");
    assert_eq!(rows(&sim.path("dev.db")), 2, "the live database is not rewound");
    assert!(o.stdout.contains("Not touched: dev.db (database content is not rewound; if the schema changed, run your migrations)."), "{}", o.stdout);
    let o2 = sim.cli(&["back", "--turn", "1", "--with-data"]);
    assert_eq!(o2.code, 0, "{}{}", o2.stdout, o2.stderr);
    assert_eq!(rows(&sim.path("dev.db")), 1, "--with-data rewinds it");
    // A missing database is restored without the flag.
    sim.prompt("three");
    assert_eq!(sim.bash("rm dev.db"), 0);
    sim.stop();
    let o3 = sim.cli(&["back", "1"]);
    assert_eq!(o3.code, 0, "{}{}", o3.stdout, o3.stderr);
    assert!(sim.path("dev.db").exists(), "{}", o3.stdout);
    // An open database (write-ahead log) is refused with --with-data.
    std::fs::write(sim.path("dev.db-wal"), b"pending").unwrap();
    let o4 = sim.cli(&["back", "--turn", "2", "--with-data"]);
    assert_eq!(o4.code, 3, "{}{}", o4.stdout, o4.stderr);
    assert!(o4.stdout.contains("Stop your app first"), "{}", o4.stdout);
}

#[test]
fn claude_code_settings_files_are_never_rewritten_and_the_report_names_them() {
    let mut sim = Sim::new();
    write_files(sim.proj(), &[("a.txt", "A1\n"), (".claude/settings.json", "{\"v\":1}\n"), (".claude/commands/x.md", "X1\n")]);
    sim.session_start();
    sim.prompt("one");
    sim.edit("a.txt", "A2\n");
    sim.edit(".claude/settings.json", "{\"v\":2}\n");
    sim.edit(".claude/settings.local.json", "{\"l\":1}\n");
    sim.edit(".claude/commands/x.md", "X2\n");
    sim.stop();
    sim.prompt("two");
    sim.edit("a.txt", "A3\n");
    sim.edit(".claude/settings.json", "{\"v\":3}\n");
    assert_eq!(sim.bash("rm .claude/settings.local.json"), 0);
    sim.edit(".claude/commands/x.md", "X3\n");
    sim.stop();
    let note = "it is a Claude Code settings file, which Torpor never changes";
    let said = format!("Not touched: .claude/settings.json ({note}), .claude/settings.local.json ({note}).");
    let before = oracle(sim.proj());
    let o = sim.cli(&["back", "--turn", "1", "--dry-run"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains(&said) && o.stdout.contains("Would change 2 files"), "{}", o.stdout);
    assert_eq!(oracle(sim.proj()), before, "a dry run writes nothing");
    let o = sim.cli(&["back", "--turn", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains(&said), "{}", o.stdout);
    let read = |p: &str| std::fs::read_to_string(sim.path(p)).unwrap();
    assert_eq!((read("a.txt"), read(".claude/commands/x.md")), ("A2\n".to_string(), "X2\n".to_string()), "the other files went back");
    assert_eq!(read(".claude/settings.json"), "{\"v\":3}\n", "the changed settings file was not rewritten");
    assert!(!sim.path(".claude/settings.local.json").exists(), "the deleted settings file was not brought back");
}

#[test]
fn recover_with_nothing_pending() {
    let (sim, _, end2) = two_turns();
    let o = sim.cli(&["recover"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "Nothing to recover.\n"), "{}", o.stderr);
    assert_eq!(oracle(sim.proj()), end2);
}

#[test]
fn files_changed_since_are_not_overwritten_by_a_stale_plan() {
    // The planner compares the live files with the saved copies, so a file edited outside any turn is part of the live state.
    let (sim, end1, _) = two_turns();
    sim.outside_edit("a.txt", "typed by hand\n");
    let o = sim.cli(&["back", "--turn", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(oracle(sim.proj()), end1);
    // The hand-typed version is saved in the pre-restore point, so it can be brought back.
    let pre: i64 = o.stdout.lines().last().unwrap().rsplit(' ').next().unwrap().parse().unwrap();
    let o2 = sim.cli(&["back", "--id", &pre.to_string()]);
    assert_eq!(o2.code, 0);
    assert_eq!(std::fs::read_to_string(sim.path("a.txt")).unwrap(), "typed by hand\n");
}
