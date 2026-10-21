//! `torpor undo K` (spec 6.4 and the fixed undo cases of A2): one turn comes out, later work stays,
//! and any conflict means nothing at all is written.
mod common;
use common::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn start(files: &[(&str, &str)]) -> Sim {
    let mut s = Sim::new();
    write_files(s.proj(), files);
    s.session_start();
    s
}

/// One finished turn: prompt, whatever `f` does, stop.
fn turn(s: &mut Sim, f: impl FnOnce(&mut Sim)) {
    s.prompt("go");
    f(s);
    s.stop();
}

fn read(s: &Sim, rel: &str) -> String {
    fs::read_to_string(s.path(rel)).unwrap()
}

fn absent(s: &Sim, rel: &str) -> bool {
    fs::symlink_metadata(s.path(rel)).is_err()
}

fn exec(s: &Sim, rel: &str) -> bool {
    fs::metadata(s.path(rel)).unwrap().permissions().mode() & 0o111 != 0
}

/// Twelve numbered lines with some of them replaced (1-based).
fn lines_with(changes: &[(usize, &str)]) -> String {
    (1..=12)
        .map(|i| changes.iter().find(|(n, _)| *n == i).map(|(_, t)| t.to_string()).unwrap_or(format!("l{i}")) + "\n")
        .collect()
}

/// The save point named by "Done. To undo this: torpor back --id N".
fn undo_id(o: &Out) -> String {
    let line = o.stdout.lines().last().unwrap_or("");
    line.strip_prefix("Done. To undo this: torpor back --id ").unwrap_or_else(|| panic!("no undo hint in {:?}", o.stdout)).to_string()
}

fn start_point(s: &Sim, n: i64) -> i64 {
    s.q_i64(&format!("SELECT start_point FROM turns WHERE n={n}"))
}

/// A refused undo changes nothing: same folder, no new save point, no restore on record.
fn assert_untouched(s: &Sim, before: &std::collections::BTreeMap<String, OEnt>, points: i64, restores: i64) {
    assert_eq!(&oracle(s.proj()), before, "a refused undo must not write");
    assert_eq!(s.max_point(), points, "a refused undo must not leave a save point");
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM restores"), restores, "a refused undo must not leave a restore row");
}

#[test]
fn clean_undo_takes_one_turn_out_and_undo_of_undo_comes_back() {
    let mut s = start(&[("a.txt", "A1\n"), ("b.txt", "B1\n")]);
    turn(&mut s, |s| {
        s.edit("a.txt", "A2\n");
        s.write_tool("new.txt", "new\n");
    });
    turn(&mut s, |s| {
        s.edit("b.txt", "B2\n");
    });
    let before = oracle(s.proj());
    let mark = s.max_point();
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let id = undo_id(&o);
    assert_eq!(
        o.stdout,
        format!("Undoing turn 1 only. Later work stays.\n  a.txt: turn 1's changes removed\n  new.txt: removed (turn 1 created it)\nDone. To undo this: torpor back --id {id}\n")
    );
    assert_eq!(read(&s, "a.txt"), "A1\n");
    assert_eq!(read(&s, "b.txt"), "B2\n", "turn 2 stays");
    assert!(absent(&s, "new.txt"));
    s.verify_last_point();
    let ch = s.changes_since(mark);
    assert_eq!(ch.iter().map(|(p, c)| format!("{p}:{c}")).collect::<Vec<_>>(), ["a.txt:restore", "new.txt:restore"]);
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM turns"), 2, "an undo is not a turn");
    assert_store_sound(&s.env);
    // Undo of the undo.
    let b = s.cli(&["back", "--id", &id]);
    assert_eq!(b.code, 0, "{}{}", b.stdout, b.stderr);
    assert_eq!(oracle(s.proj()), before);
    s.verify_last_point();
}

#[test]
fn undo_merges_changes_on_different_lines() {
    let mut s = start(&[("f.txt", &lines_with(&[]))]);
    turn(&mut s, |s| {
        s.edit("f.txt", &lines_with(&[(2, "turn one two")]));
    });
    turn(&mut s, |s| {
        s.edit("f.txt", &lines_with(&[(2, "turn one two"), (9, "turn two nine")]));
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("  f.txt: turn 1's changes removed, turn 2's kept\n"), "{}", o.stdout);
    assert_eq!(read(&s, "f.txt"), lines_with(&[(9, "turn two nine")]));
    s.verify_last_point();
}

#[test]
fn undo_merge_with_an_outside_edit_on_other_lines() {
    let mut s = start(&[("f.txt", &lines_with(&[]))]);
    turn(&mut s, |s| {
        s.edit("f.txt", &lines_with(&[(2, "turn one two")]));
    });
    // Someone else edits line 10 between the turns; the next start point records it as an outside change.
    s.outside_edit("f.txt", &lines_with(&[(2, "turn one two"), (10, "typed by hand")]));
    turn(&mut s, |s| {
        s.edit("g.txt", "g\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("  f.txt: turn 1's changes removed, the later changes kept\n"), "{}", o.stdout);
    assert_eq!(read(&s, "f.txt"), lines_with(&[(10, "typed by hand")]));
}

#[test]
fn same_line_conflict_writes_nothing() {
    let mut s = start(&[("f.txt", &lines_with(&[])), ("g.txt", "g1\n")]);
    turn(&mut s, |s| {
        s.edit("f.txt", &lines_with(&[(3, "turn one three")]));
        s.edit("g.txt", "g2\n"); // clean on its own, but a conflict elsewhere stops everything
    });
    turn(&mut s, |s| {
        s.edit("f.txt", &lines_with(&[(3, "turn two three")]));
    });
    let (before, points, restores) = (oracle(s.proj()), s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    let first = start_point(&s, 1);
    assert_eq!(
        o.stdout,
        format!("Can't undo turn 1 cleanly, so nothing was changed.\n  f.txt: turn 2 changed the same lines\nInstead: torpor back --id {first} (also drops turn 2), or ask the AI to undo it by hand.\n")
    );
    assert_untouched(&s, &before, points, restores);
    assert_eq!(read(&s, "g.txt"), "g2\n");
    // The suggestion works: the start point of turn 1 is the state before it.
    let b = s.cli(&["back", "--id", &first.to_string()]);
    assert_eq!(b.code, 0, "{}{}", b.stdout, b.stderr);
    assert_eq!(read(&s, "f.txt"), lines_with(&[]));
}

#[test]
fn conflict_names_the_turns_and_suggests_going_back_to_the_turn_before() {
    let mut s = start(&[("f.txt", &lines_with(&[])), ("h.txt", "h\n")]);
    turn(&mut s, |s| {
        s.edit("h.txt", "h one\n");
    });
    turn(&mut s, |s| {
        s.edit("f.txt", &lines_with(&[(5, "turn two five")]));
    });
    turn(&mut s, |s| {
        s.edit("f.txt", &lines_with(&[(5, "turn three five")]));
    });
    turn(&mut s, |s| {
        s.edit("f.txt", &lines_with(&[(5, "turn four five")]));
    });
    let (before, points, restores) = (oracle(s.proj()), s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "2"]);
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert_eq!(
        o.stdout,
        "Can't undo turn 2 cleanly, so nothing was changed.\n  f.txt: turns 3 and 4 changed the same lines\nInstead: torpor back --turn 1 (also drops turns 3-4), or ask the AI to undo it by hand.\n"
    );
    assert_untouched(&s, &before, points, restores);
}

#[test]
fn a_created_file_is_removed_only_when_nobody_changed_it_since() {
    let mut s = start(&[("keep.txt", "k\n")]);
    turn(&mut s, |s| {
        s.write_tool("c1.txt", "one\n");
        s.write_tool("c2.txt", "two\n");
    });
    turn(&mut s, |s| {
        s.edit("keep.txt", "k2\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(absent(&s, "c1.txt") && absent(&s, "c2.txt"), "{}", o.stdout);
    assert_eq!(read(&s, "keep.txt"), "k2\n");

    // Same story, but turn 2 edits one of the two files: nothing is removed, not even the untouched one.
    let mut s = start(&[("keep.txt", "k\n")]);
    turn(&mut s, |s| {
        s.write_tool("c1.txt", "one\n");
        s.write_tool("c2.txt", "two\n");
    });
    turn(&mut s, |s| {
        s.edit("c2.txt", "two, edited\n");
    });
    let (before, points, restores) = (oracle(s.proj()), s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("\n  c2.txt: turn 1 created it, then it changed (turn 2)\n"), "{}", o.stdout);
    assert!(!o.stdout.contains("c1.txt"), "{}", o.stdout);
    assert_untouched(&s, &before, points, restores);
    assert!(s.path("c1.txt").exists());
}

#[test]
fn folders_made_by_the_turn_go_when_they_end_up_empty() {
    let mut s = start(&[("x.txt", "x\n")]);
    turn(&mut s, |s| {
        assert_eq!(s.bash("mkdir -p new/a/b && echo one > new/a/b/c.txt && echo two > new/f.txt"), 0);
    });
    turn(&mut s, |s| {
        s.write_tool("new/keep.txt", "later\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(absent(&s, "new/a") && absent(&s, "new/f.txt"), "{}", o.stdout);
    assert_eq!(read(&s, "new/keep.txt"), "later\n", "a file from a later turn keeps its folder");
    s.verify_last_point();

    let mut s = start(&[("x.txt", "x\n")]);
    turn(&mut s, |s| {
        assert_eq!(s.bash("mkdir -p new/a/b && echo one > new/a/b/c.txt && echo two > new/f.txt"), 0);
    });
    turn(&mut s, |s| {
        s.edit("x.txt", "x2\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(absent(&s, "new"), "the whole new folder is gone: {}", o.stdout);
    s.verify_last_point();
}

#[test]
fn a_deleted_file_or_tree_comes_back() {
    let mut s = start(&[("old.txt", "old\n"), ("src/main.rs", "m\n"), ("src/deep/x.rs", "x\n"), ("src/deep/y.rs", "y\n")]);
    turn(&mut s, |s| {
        assert_eq!(s.bash("rm -f old.txt && rm -rf src/deep"), 0);
    });
    turn(&mut s, |s| {
        s.edit("src/main.rs", "m2\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    for f in ["  old.txt: brought back (turn 1 deleted it)\n", "  src/deep/x.rs: brought back (turn 1 deleted it)\n", "  src/deep/y.rs: brought back (turn 1 deleted it)\n"] {
        assert!(o.stdout.contains(f), "{f:?} missing in {}", o.stdout);
    }
    assert_eq!((read(&s, "old.txt"), read(&s, "src/deep/x.rs"), read(&s, "src/deep/y.rs")), ("old\n".into(), "x\n".into(), "y\n".into()));
    assert_eq!(read(&s, "src/main.rs"), "m2\n");
    s.verify_last_point();
}

#[test]
fn a_deleted_file_that_was_made_again_is_kept() {
    let mut s = start(&[("old.txt", "old\n"), ("z.txt", "z\n")]);
    turn(&mut s, |s| {
        assert_eq!(s.bash("rm old.txt"), 0);
    });
    turn(&mut s, |s| {
        s.write_tool("old.txt", "written again\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(
        o.stdout,
        "Undoing turn 1 only. Later work stays.\nNothing needed changing.\nNot touched: old.txt (kept as it is, turn 1 deleted it and it was created again later).\n"
    );
    assert_eq!(read(&s, "old.txt"), "written again\n");
}

#[test]
fn binary_files_are_whole_file_or_nothing() {
    let put = |n: u8| format!("printf '\\000\\001\\002\\377blob{n}' > data.bin");
    let mut s = start(&[("z.txt", "z\n")]);
    std::fs::write(s.path("data.bin"), [0u8, 9, 9, 9]).unwrap();
    s.prompt("go");
    assert_eq!(s.bash(&put(1)), 0);
    s.stop();
    turn(&mut s, |s| {
        s.edit("z.txt", "z2\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(fs::read(s.path("data.bin")).unwrap(), [0u8, 9, 9, 9], "the whole old file is back");
    s.verify_last_point();

    // Changed again later: Torpor does not try to merge bytes.
    let mut s = start(&[("z.txt", "z\n")]);
    std::fs::write(s.path("data.bin"), [0u8, 9, 9, 9]).unwrap();
    s.prompt("go");
    assert_eq!(s.bash(&put(1)), 0);
    s.stop();
    turn(&mut s, |s| {
        assert_eq!(s.bash(&put(2)), 0);
    });
    let (before, points, restores) = (oracle(s.proj()), s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("\n  data.bin: it is not plain text and it changed after turn 1 (turn 2)\n"), "{}", o.stdout);
    assert_untouched(&s, &before, points, restores);
}

#[test]
fn the_exec_bit_goes_back_unless_someone_changed_it_since() {
    // Only the permission changed in turn 1, nothing after it.
    let mut s = start(&[("run.sh", "echo hi\n"), ("z.txt", "z\n")]);
    turn(&mut s, |s| {
        assert_eq!(s.bash("chmod +x run.sh"), 0);
    });
    turn(&mut s, |s| {
        s.edit("z.txt", "z2\n");
    });
    assert!(exec(&s, "run.sh"));
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("  run.sh: turn 1's permission change removed\n"), "{}", o.stdout);
    assert!(!exec(&s, "run.sh"));
    s.verify_last_point();

    // Turn 2 edited the content, so the content merges and the exec bit still goes.
    let mut s = start(&[("run.sh", "echo hi\n")]);
    turn(&mut s, |s| {
        assert_eq!(s.bash("chmod +x run.sh"), 0);
    });
    turn(&mut s, |s| {
        s.edit("run.sh", "echo hi again\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(read(&s, "run.sh"), "echo hi again\n");
    assert!(!exec(&s, "run.sh"), "{}", o.stdout);

    // Someone turned it off again already: nothing to do.
    let mut s = start(&[("run.sh", "echo hi\n")]);
    turn(&mut s, |s| {
        assert_eq!(s.bash("chmod +x run.sh"), 0);
    });
    turn(&mut s, |s| {
        assert_eq!(s.bash("chmod -x run.sh"), 0);
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!((o.code, o.stdout.contains("Nothing needed changing.")), (0, true), "{}{}", o.stdout, o.stderr);

    // Content and permission in the same turn, nothing after it: both come back.
    let mut s = start(&[("run.sh", "echo hi\n"), ("z.txt", "z\n")]);
    turn(&mut s, |s| {
        s.edit("run.sh", "echo changed\n");
        assert_eq!(s.bash("chmod +x run.sh"), 0);
    });
    turn(&mut s, |s| {
        s.edit("z.txt", "z2\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(read(&s, "run.sh"), "echo hi\n");
    assert!(!exec(&s, "run.sh"));
}

#[test]
fn links_and_kind_changes_come_back() {
    let mut s = start(&[("a.txt", "A\n"), ("b.txt", "B\n"), ("z.txt", "z\n")]);
    std::os::unix::fs::symlink("a.txt", s.path("link")).unwrap();
    turn(&mut s, |s| {
        assert_eq!(s.bash("ln -sfn b.txt link && ln -s a.txt link2 && rm b.txt && ln -s a.txt b.txt"), 0);
    });
    turn(&mut s, |s| {
        s.edit("z.txt", "z2\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(fs::read_link(s.path("link")).unwrap().to_str(), Some("a.txt"));
    assert!(absent(&s, "link2"));
    assert_eq!(read(&s, "b.txt"), "B\n");
    assert!(fs::symlink_metadata(s.path("b.txt")).unwrap().file_type().is_file(), "b.txt is a plain file again");
    s.verify_last_point();
}

#[test]
fn secret_files_come_back_but_are_never_merged() {
    let mut s = start(&[(".env", "TOKEN=one\n"), ("z.txt", "z\n")]);
    turn(&mut s, |s| {
        s.edit(".env", "TOKEN=two\n");
    });
    turn(&mut s, |s| {
        s.edit("z.txt", "z2\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(read(&s, ".env"), "TOKEN=one\n");
    assert_eq!(fs::metadata(s.path(".env")).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!o.stdout.contains("TOKEN"), "{}", o.stdout);
    s.verify_last_point();

    let mut s = start(&[(".env", "TOKEN=one\n")]);
    turn(&mut s, |s| {
        s.edit(".env", "TOKEN=two\n");
    });
    turn(&mut s, |s| {
        s.edit(".env", "TOKEN=three\n");
    });
    let (before, points, restores) = (oracle(s.proj()), s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("\n  .env: it is not plain text and it changed after turn 1 (turn 2)\n"), "{}", o.stdout);
    assert!(!o.stdout.contains("TOKEN"), "{}", o.stdout);
    assert_untouched(&s, &before, points, restores);
}

#[test]
fn a_change_made_outside_the_chat_on_the_same_lines_is_a_conflict() {
    let mut s = start(&[("f.txt", &lines_with(&[]))]);
    turn(&mut s, |s| {
        s.edit("f.txt", &lines_with(&[(3, "turn one three")]));
    });
    s.outside_edit("f.txt", &lines_with(&[(3, "typed by hand")]));
    turn(&mut s, |s| {
        s.edit("g.txt", "g\n");
    });
    let (before, points, restores) = (oracle(s.proj()), s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("\n  f.txt: it was also changed outside the chat, on the same lines as turn 1\n"), "{}", o.stdout);
    assert_untouched(&s, &before, points, restores);
}

#[test]
fn a_file_changed_and_then_deleted_is_a_conflict() {
    let mut s = start(&[("f.txt", "one\n"), ("z.txt", "z\n")]);
    turn(&mut s, |s| {
        s.edit("f.txt", "two\n");
    });
    turn(&mut s, |s| {
        assert_eq!(s.bash("rm f.txt"), 0);
    });
    let (before, points, restores) = (oracle(s.proj()), s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("\n  f.txt: turn 1 changed it, then it was deleted (turn 2)\n"), "{}", o.stdout);
    assert_untouched(&s, &before, points, restores);
}

#[test]
fn many_conflicts_are_listed_ten_at_a_time() {
    let names: Vec<String> = (0..13).map(|i| format!("c{i:02}.txt")).collect();
    let files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "base\n")).collect();
    let mut s = start(&files);
    turn(&mut s, |s| {
        for n in &names {
            s.edit(n, "one\n");
        }
    });
    turn(&mut s, |s| {
        for n in &names {
            s.edit(n, "two\n");
        }
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert_eq!(o.stdout.lines().filter(|l| l.contains("turn 2 changed the same lines")).count(), 10, "{}", o.stdout);
    assert!(o.stdout.contains("\n  and 3 more\n"), "{}", o.stdout);
}

#[test]
fn dry_run_reports_and_changes_nothing() {
    let mut s = start(&[("a.txt", "A1\n"), ("b.txt", "B1\n")]);
    turn(&mut s, |s| {
        s.edit("a.txt", "A2\n");
        s.write_tool("new.txt", "new\n");
    });
    turn(&mut s, |s| {
        s.edit("b.txt", "B2\n");
    });
    let (before, points, restores) = (oracle(s.proj()), s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "1", "--dry-run"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(
        o.stdout,
        "Would undo turn 1 only. Later work stays.\n  a.txt: turn 1's changes removed\n  new.txt: removed (turn 1 created it)\nNothing was changed (dry run).\n"
    );
    assert_untouched(&s, &before, points, restores);
    // The real thing does what the dry run said.
    let o = s.cli(&["undo", "--dry-run", "1"]);
    assert_eq!(o.code, 0);
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!((read(&s, "a.txt"), absent(&s, "new.txt")), ("A1\n".to_string(), true));
}

#[test]
fn turns_that_cannot_be_undone_say_why() {
    let mut s = start(&[("a.txt", "A1\n")]);
    turn(&mut s, |s| {
        s.edit("a.txt", "A2\n");
    });
    turn(&mut s, |_| {});
    let (before, points, restores) = (oracle(s.proj()), s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "2"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "Turn 2 changed no files, so there is nothing to undo.\n"));
    let o = s.cli(&["undo", "99"]);
    assert_eq!((o.code, o.stdout.as_str(), o.stderr.as_str()), (5, "There is no turn 99.\n", ""));
    assert_untouched(&s, &before, points, restores);
    // A turn that is still running.
    s.prompt("not finished");
    let (before, points, restores) = (oracle(s.proj()), s.max_point(), s.q_i64("SELECT COUNT(*) FROM restores"));
    let o = s.cli(&["undo", "3"]);
    assert_eq!((o.code, o.stdout.as_str()), (3, "Turn 3 is still running. Wait until it ends, then undo it.\n"));
    assert_untouched(&s, &before, points, restores);
    assert!(s.path("a.txt").exists());
}

#[test]
fn bad_arguments_are_a_usage_error() {
    let mut s = start(&[("a.txt", "A1\n")]);
    turn(&mut s, |s| {
        s.edit("a.txt", "A2\n");
    });
    for args in [&["undo"][..], &["undo", "0"], &["undo", "-1"], &["undo", "abc"], &["undo", "1", "2"], &["undo", "1", "--force"]] {
        let o = s.cli(args);
        assert_eq!((o.code, o.stdout.as_str(), o.stderr.as_str()), (2, "", "Usage: torpor undo K [--dry-run]\n"), "{args:?}");
    }
    assert_eq!(read(&s, "a.txt"), "A2\n");
}

#[test]
fn databases_are_not_rewound_by_undo() {
    let mut s = start(&[("app.db", "version one\n"), ("z.txt", "z\n")]);
    turn(&mut s, |s| {
        s.edit("app.db", "version two\n");
    });
    turn(&mut s, |s| {
        s.edit("z.txt", "z2\n");
    });
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains(&format!("Not touched: app.db ({})", torpor::restore::DB_NOTE)), "{}", o.stdout);
    assert_eq!(read(&s, "app.db"), "version two\n");
}

#[test]
fn undoing_an_older_turn_after_a_newer_undo_still_works() {
    let mut s = start(&[("a.txt", "A\n"), ("b.txt", "B\n"), ("c.txt", "C\n")]);
    for (f, v) in [("a.txt", "A1\n"), ("b.txt", "B1\n"), ("c.txt", "C1\n")] {
        turn(&mut s, |s| {
            s.edit(f, v);
        });
    }
    let o = s.cli(&["undo", "3"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let o = s.cli(&["undo", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!((read(&s, "a.txt"), read(&s, "b.txt"), read(&s, "c.txt")), ("A\n".into(), "B1\n".into(), "C\n".into()));
    // Undoing turn 3 again finds nothing left to take out.
    let o = s.cli(&["undo", "3"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("Nothing needed changing."), "{}", o.stdout);
    assert_store_sound(&s.env);
}
