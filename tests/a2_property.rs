//! A2 (spec 10): random histories of turns (create, modify, delete, rename, binary, empty, symlink, a case pair,
//! the exec bit) followed by random `back N`, `back --turn K`, `back --id P` and `undo K`. After every restore the
//! folder must equal the state that was recorded for the save point it went to, `back 1` must undo a `back`, and
//! `back --id <pre-undo point>` must undo an `undo`. 4 tests x 64 histories = 256 histories, fixed seeds.
mod common;
use common::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
use std::collections::{BTreeMap, BTreeSet};

const TEXT: [&str; 6] = [
    "a.txt",
    "b.txt",
    "docs/c.md",
    "src/d.rs",
    "src/deep/e.rs",
    "run.sh",
];
const BINS: [&str; 2] = ["data.bin", "docs/img.bin"];
const LINKS: [&str; 2] = ["ln1", "docs/ln2"];
const ALL: usize = TEXT.len() + BINS.len();
const POOL: usize = ALL + LINKS.len();

fn name(i: usize) -> &'static str {
    let i = i % POOL;
    if i < TEXT.len() {
        TEXT[i]
    } else if i < ALL {
        BINS[i - TEXT.len()]
    } else {
        LINKS[i - ALL]
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// A whole new text file, written by the Write tool or by the shell.
    Text {
        file: usize,
        ver: u8,
        tool: bool,
    },
    /// One line of a text file changes (the file is made first when it is missing).
    Line {
        file: usize,
        line: usize,
        val: u8,
        tool: bool,
    },
    Binary {
        file: usize,
        ver: u8,
    },
    Empty {
        file: usize,
    },
    Delete {
        file: usize,
    },
    Rename {
        from: usize,
        to: usize,
    },
    Link {
        name: usize,
        to: usize,
    },
    Chmod {
        file: usize,
        on: bool,
    },
    CasePair {
        ver: u8,
    },
}

#[derive(Clone, Debug)]
enum Act {
    Back(usize),
    BackTurn(usize),
    BackId(usize),
    Undo(usize),
}

#[derive(Clone, Debug)]
struct Case {
    turns: Vec<Vec<Op>>,
    acts: Vec<Act>,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..TEXT.len(), 0..8usize, 0u8..3, any::<bool>()).prop_map(|(file, line, val, tool)| Op::Line { file, line, val, tool }),
        3 => (0..TEXT.len(), 0u8..4, any::<bool>()).prop_map(|(file, ver, tool)| Op::Text { file, ver, tool }),
        2 => (0..BINS.len(), 0u8..3).prop_map(|(file, ver)| Op::Binary { file, ver }),
        1 => (0..ALL).prop_map(|file| Op::Empty { file }),
        3 => (0..POOL).prop_map(|file| Op::Delete { file }),
        3 => (0..ALL, 0..ALL).prop_map(|(from, to)| Op::Rename { from, to }),
        2 => (0..LINKS.len(), 0..=TEXT.len()).prop_map(|(name, to)| Op::Link { name, to }),
        2 => (0..ALL, any::<bool>()).prop_map(|(file, on)| Op::Chmod { file, on }),
        1 => (0u8..3).prop_map(|ver| Op::CasePair { ver }),
    ]
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        3 => (1usize..=6).prop_map(Act::Back),
        2 => (0usize..8).prop_map(Act::BackTurn),
        2 => (0usize..64).prop_map(Act::BackId),
        4 => (0usize..8).prop_map(Act::Undo),
    ]
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(prop::collection::vec(op(), 1..=4), 2..=4),
        prop::collection::vec(act(), 1..=3),
    )
        .prop_map(|(turns, acts)| Case { turns, acts })
}

// ---------------------------------------------------------------- doing the history

fn mkdir_for(rel: &str) -> String {
    rel.rsplit_once('/')
        .map(|(d, _)| format!("mkdir -p {d} && "))
        .unwrap_or_default()
}

fn body(ver: u8) -> String {
    (1..=8).map(|i| format!("line {i} v{ver}\n")).collect()
}

fn is_file(s: &Sim, rel: &str) -> bool {
    std::fs::symlink_metadata(s.path(rel)).is_ok_and(|m| m.is_file())
}

fn put_text(s: &mut Sim, rel: &str, text: &str, tool: bool) {
    if tool {
        s.edit(rel, text);
    } else {
        s.bash(&format!("{}printf '%s' '{text}' > {rel}", mkdir_for(rel)));
    }
}

fn apply(s: &mut Sim, op: &Op) {
    match op {
        Op::Text { file, ver, tool } => put_text(s, TEXT[*file], &body(*ver), *tool),
        Op::Line {
            file,
            line,
            val,
            tool,
        } => {
            let rel = TEXT[*file];
            let mut lines: Vec<String> = match std::fs::read_to_string(s.path(rel)) {
                Ok(t) if !t.is_empty() => t.lines().map(String::from).collect(),
                _ => (1..=8).map(|i| format!("line {i}")).collect(),
            };
            let n = lines.len();
            lines[line % n] = format!("edit {val}");
            put_text(s, rel, &(lines.join("\n") + "\n"), *tool);
        }
        Op::Binary { file, ver } => {
            let rel = BINS[*file];
            s.bash(&format!(
                "{}printf '\\000\\001\\377v{ver}' > {rel}",
                mkdir_for(rel)
            ));
        }
        Op::Empty { file } => {
            let rel = name(*file % ALL);
            s.bash(&format!("{}: > {rel}", mkdir_for(rel)));
        }
        Op::Delete { file } => {
            s.bash(&format!("rm -f {}", name(*file)));
        }
        Op::Rename { from, to } => {
            let (a, b) = (name(*from % ALL), name(*to % ALL));
            if a != b && is_file(s, a) {
                s.bash(&format!("{}mv {a} {b}", mkdir_for(b)));
            }
        }
        Op::Link { name: n, to } => {
            let (link, target) = (LINKS[*n], TEXT.get(*to).copied().unwrap_or("missing.txt"));
            s.bash(&format!("{}ln -sfn {target} {link}", mkdir_for(link)));
        }
        Op::Chmod { file, on } => {
            let rel = name(*file % ALL);
            if is_file(s, rel) {
                s.bash(&format!("chmod {}x {rel}", if *on { "+" } else { "-" }));
            }
        }
        Op::CasePair { ver } => {
            s.bash(&format!(
                "printf 'upper v{ver}' > Case.txt; printf 'lower v{ver}' > case.txt"
            ));
        }
    }
}

type Tree = BTreeMap<String, OEnt>;

/// What the random histories really exercised (printed, and checked so the generator cannot go degenerate).
#[derive(Default, Debug)]
struct Tally {
    cases: usize,
    ops: usize,
    shell_points: usize,
    back_changed: usize,
    back_same: usize,
    back_refused: usize,
    undo_done: usize,
    undo_nothing: usize,
    undo_refused: usize,
}

/// What the folder held when each save point was taken.
type Rec = BTreeMap<i64, Tree>;

fn note(s: &Sim, rec: &mut Rec, id: i64) {
    let now = oracle(s.proj());
    match rec.get(&id) {
        Some(old) => assert_eq!(
            old, &now,
            "save point {id} was seen with another folder state"
        ),
        None => {
            rec.insert(id, now);
        }
    }
}

fn turn_point(s: &Sim, n: usize, col: &str) -> i64 {
    s.q_i64(&format!("SELECT {col} FROM turns WHERE n={n}"))
}

/// The number after "save point " in a report.
fn point_in(o: &Out) -> Option<i64> {
    let rest = o.stdout.split("save point ").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn undo_hint(o: &Out) -> Option<i64> {
    o.stdout
        .lines()
        .last()?
        .strip_prefix("Done. To undo this: torpor back --id ")?
        .parse()
        .ok()
}

// ---------------------------------------------------------------- checking the restores

fn restore_step(s: &mut Sim, rec: &mut Rec, t: &mut Tally, args: &[&str]) {
    let before = oracle(s.proj());
    let points = s.max_point();
    let o = s.cli(args);
    let after = oracle(s.proj());
    let ctx = format!(
        "torpor {} -> exit {}\n{}{}",
        args.join(" "),
        o.code,
        o.stdout,
        o.stderr
    );
    assert!(
        !s.path(".torpor-stage").exists(),
        "stage folder left behind\n{ctx}"
    );
    match o.code {
        0 if args[0] == "undo" && undo_hint(&o).is_some() => {
            t.undo_done += 1;
            let k: usize = args[1].parse().unwrap();
            let pre = undo_hint(&o).unwrap();
            check_undo(s, rec, k, &before, &after, &ctx);
            s.verify_last_point();
            rec.insert(pre, before.clone());
            note(s, rec, s.max_point());
            // Undo of the undo.
            let b = s.cli(&["back", "--id", &pre.to_string()]);
            assert_eq!(
                b.code, 0,
                "undo of the undo failed: {}{}\nafter {ctx}",
                b.stdout, b.stderr
            );
            assert_eq!(
                oracle(s.proj()),
                before,
                "undo-of-undo did not bring back the pre-undo folder\n{ctx}"
            );
            s.verify_point(pre);
            note(s, rec, s.max_point());
            // Back to the state after the undo, so later steps run on it.
            let id = rec
                .iter()
                .rev()
                .find(|(_, t)| **t == after)
                .map(|(i, _)| *i)
                .expect("the post-undo point is recorded");
            let b = s.cli(&["back", "--id", &id.to_string()]);
            assert_eq!(b.code, 0, "{}{}", b.stdout, b.stderr);
            assert_eq!(oracle(s.proj()), after);
            note(s, rec, s.max_point());
        }
        0 if args[0] == "undo" => {
            t.undo_nothing += 1;
            assert!(
                o.stdout.contains("Nothing needed changing.")
                    || o.stdout.contains("so there is nothing to undo."),
                "{ctx}"
            );
            assert_eq!(after, before, "{ctx}");
            assert_eq!(
                s.max_point(),
                points,
                "no new save point when nothing changed\n{ctx}"
            );
        }
        0 => {
            let target =
                point_in(&o).unwrap_or_else(|| panic!("no save point in the report\n{ctx}"));
            s.verify_point(target);
            if let Some(want) = rec.get(&target) {
                assert_eq!(
                    &after, want,
                    "the folder is not the recorded state of save point {target}\n{ctx}"
                );
            }
            if o.stdout.contains("Nothing to do.") {
                t.back_same += 1;
                assert_eq!(after, before, "{ctx}");
                assert_eq!(s.max_point(), points, "{ctx}");
            } else {
                t.back_changed += 1;
                let pre = undo_hint(&o).unwrap_or_else(|| panic!("no undo hint\n{ctx}"));
                s.verify_last_point();
                rec.insert(pre, before.clone());
                note(s, rec, s.max_point());
                // `back 1` after each: it goes back to where we were.
                assert_ne!(
                    after, before,
                    "a restore that changed nothing should have said so\n{ctx}"
                );
                let b = s.cli(&["back", "1"]);
                assert_eq!(
                    b.code,
                    0,
                    "back 1 after `{}` failed: {}{}",
                    args.join(" "),
                    b.stdout,
                    b.stderr
                );
                assert_eq!(
                    oracle(s.proj()),
                    before,
                    "back 1 did not return to the folder before `{}`\n{ctx}",
                    args.join(" ")
                );
                s.verify_last_point();
                note(s, rec, s.max_point());
                // And forward again, to keep the history moving.
                let b = s.cli(&["back", "--id", &target.to_string()]);
                assert_eq!(b.code, 0, "{}{}", b.stdout, b.stderr);
                assert_eq!(oracle(s.proj()), after);
                note(s, rec, s.max_point());
            }
        }
        3 | 5 => {
            if args[0] == "undo" {
                t.undo_refused += 1
            } else {
                t.back_refused += 1
            }
            assert_eq!(after, before, "a refused restore changed the folder\n{ctx}");
            assert_eq!(
                s.max_point(),
                points,
                "a refused restore left a save point\n{ctx}"
            );
            assert!(
                args[0] == "undo" || o.code == 5,
                "unexpected refusal\n{ctx}"
            );
        }
        c => panic!("exit {c} is not expected here\n{ctx}"),
    }
    assert_eq!(
        s.q_i64("SELECT COUNT(*) FROM restores WHERE status IN ('staging','applying')"),
        0,
        "a restore row is still open\n{ctx}"
    );
}

/// What an undo of turn `k` must have done, from the recorded states alone.
fn check_undo(s: &Sim, rec: &Rec, k: usize, before: &Tree, after: &Tree, ctx: &str) {
    let (start, end) = (
        &rec[&turn_point(s, k, "start_point")],
        &rec[&turn_point(s, k, "end_point")],
    );
    let paths: BTreeSet<&String> = start
        .keys()
        .chain(end.keys())
        .chain(before.keys())
        .chain(after.keys())
        .collect();
    for p in paths {
        let (st, en, lv, af) = (start.get(p), end.get(p), before.get(p), after.get(p));
        if [st, en, lv, af]
            .iter()
            .any(|e| e.is_some_and(|e| e.kind == 'd'))
        {
            continue; // folders are judged by the undo-of-undo
        }
        if st == en {
            assert_eq!(
                af, lv,
                "{p}: turn {k} did not change it, so the undo must leave it\n{ctx}"
            );
        } else if lv == en {
            assert_eq!(
                af, st,
                "{p}: nothing changed it after turn {k}, so it must be as before turn {k}\n{ctx}"
            );
        }
    }
}

fn run(c: &Case, t: &mut Tally) {
    let mut s = Sim::new();
    let p = s.proj().to_path_buf();
    write_files(
        &p,
        &[
            ("a.txt", &body(0)),
            ("src/d.rs", &body(0)),
            ("docs/c.md", &body(1)),
        ],
    );
    std::fs::write(p.join("data.bin"), [0u8, 1, 2, 3]).unwrap();
    let mut rec: Rec = BTreeMap::new();
    s.session_start();
    for (i, ops) in c.turns.iter().enumerate() {
        s.prompt("go");
        let n = i + 1;
        note(&s, &mut rec, turn_point(&s, n, "start_point"));
        for o in ops {
            // A shell command that changed something leaves a point; a Write-tool edit waits for the end of the turn.
            let last = s.max_point();
            apply(&mut s, o);
            t.ops += 1;
            if s.max_point() != last {
                t.shell_points += 1;
                note(&s, &mut rec, s.max_point());
            }
        }
        s.stop();
        note(&s, &mut rec, turn_point(&s, n, "end_point"));
    }
    s.verify_last_point();
    let turns = c.turns.len();
    for a in &c.acts {
        match a {
            Act::Back(n) => restore_step(&mut s, &mut rec, t, &["back", &n.to_string()]),
            Act::BackTurn(k) => restore_step(
                &mut s,
                &mut rec,
                t,
                &["back", "--turn", &(1 + k % turns).to_string()],
            ),
            Act::BackId(i) => {
                let ids: Vec<i64> = rec.keys().copied().collect();
                restore_step(
                    &mut s,
                    &mut rec,
                    t,
                    &["back", "--id", &ids[i % ids.len()].to_string()],
                );
            }
            Act::Undo(k) => {
                restore_step(&mut s, &mut rec, t, &["undo", &(1 + k % turns).to_string()])
            }
        }
    }
    assert_store_sound(&s.env);
    assert!(s.faults().is_empty(), "faults: {:?}", s.faults());
    t.cases += 1;
}

fn run_cases(seed: u8) {
    let config = Config {
        cases: 64,
        failure_persistence: None,
        max_shrink_iters: 24,
        ..Config::default()
    };
    let mut runner = TestRunner::new_with_rng(
        config,
        TestRng::from_seed(RngAlgorithm::ChaCha, &[seed; 32]),
    );
    let t = std::cell::RefCell::new(Tally::default());
    if let Err(e) = runner.run(&case(), |c| {
        run(&c, &mut t.borrow_mut());
        Ok(())
    }) {
        panic!("{e}");
    }
    let t = t.into_inner();
    eprintln!("A2 seed {seed}: {t:?}");
    assert_eq!(t.cases, 64);
    // The histories must reach every branch, or the property proves nothing.
    assert!(
        t.shell_points > 20 && t.back_changed > 10 && t.undo_done > 5,
        "{t:?}"
    );
    assert!(
        t.back_same + t.back_refused + t.undo_nothing + t.undo_refused > 0,
        "{t:?}"
    );
}

#[test]
fn a2_histories_1() {
    run_cases(1);
}

#[test]
fn a2_histories_2() {
    run_cases(2);
}

#[test]
fn a2_histories_3() {
    run_cases(3);
}

#[test]
fn a2_histories_4() {
    run_cases(4);
}
