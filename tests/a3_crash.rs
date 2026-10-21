//! A3 crash (spec 10): SIGKILL a restore at every step, run `torpor recover`, and the folder must be exactly the state
//! before the restore or exactly the state after it, never a mix. Also kills during a save-point capture, and a second
//! kill inside `recover` itself.
mod common;
use common::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

const RUNS: usize = 100;

type Tree = BTreeMap<String, OEnt>;

struct Fixture {
    sim: Sim,
    pristine_proj: PathBuf,
    pristine_home: PathBuf,
    /// The folder before `back 1` (the live state) and the end of turn 1 (where `back 1` goes).
    pre: Tree,
    target: Tree,
}

fn blob(seed: u32, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761).wrapping_add(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 8) as u8
        })
        .collect()
}

/// A project with two turns; turn 2 touches every kind of thing a restore has to undo.
fn fixture() -> Fixture {
    let mut sim = Sim::new();
    let p = sim.proj().to_path_buf();
    write_files(
        &p,
        &[
            ("a.txt", "A1\n"),
            ("README.md", "# readme\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("src/util/mod.rs", "pub mod deep;\n"),
            ("src/util/deep/x.rs", "pub fn x() {}\n"),
            ("src/util/deep/y.rs", "pub fn y() {}\n"),
            ("docs/guide.md", "guide\n"),
            (".env", "API_TOKEN=synthetic-one\n"),
            ("config/.env.local", "LOCAL=1\n"),
            ("bin/run.sh", "echo run\n"),
            ("bin/tool.sh", "echo tool\n"),
            ("same.txt", "same\n"),
        ],
    );
    std::fs::write(p.join("data.bin"), blob(1, 3000)).unwrap();
    std::fs::set_permissions(p.join("bin/run.sh"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("a.txt", p.join("link1")).unwrap();
    std::os::unix::fs::symlink("README.md", p.join("link2")).unwrap();
    std::fs::create_dir_all(p.join("scratch")).unwrap();
    std::fs::create_dir_all(p.join("docs/img/empty")).unwrap();
    sim.session_start();
    sim.prompt("turn one");
    sim.edit("a.txt", "A2\n");
    sim.stop();
    let target = oracle(&p);
    sim.prompt("turn two");
    sim.edit("src/main.rs", "fn main() { println!(\"hi\"); }\n");
    assert_eq!(sim.bash("rm -rf src/util docs/guide.md scratch docs/img"), 0);
    assert_eq!(sim.bash("rm .env && echo 'LOCAL=2' > config/.env.local"), 0);
    assert_eq!(sim.bash("chmod +x bin/tool.sh && chmod -x bin/run.sh"), 0);
    assert_eq!(sim.bash("ln -sfn README.md link1 && rm link2"), 0);
    assert_eq!(sim.bash("mkdir -p new/a/b && echo one > new/f1.txt && echo two > new/a/b/c.txt"), 0);
    sim.write_tool("n1.txt", "n1\n");
    sim.write_tool("n2.txt", "n2\n");
    sim.write_tool("n3/n3.txt", "n3\n");
    std::fs::write(p.join("data.bin"), blob(2, 4100)).unwrap();
    sim.stop();
    let pre = oracle(&p);
    assert_ne!(pre, target);
    // Everything in the store file, so a copy of the folder is a copy of the store.
    {
        let c = rusqlite::Connection::open(sim.ws_dir().join("torpor.db")).unwrap();
        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
    }
    let base = sim.env.proj.parent().unwrap().to_path_buf();
    let (pristine_proj, pristine_home) = (base.join("pristine-proj"), base.join("pristine-home"));
    copy_tree(&p, &pristine_proj);
    copy_tree(&sim.env.thome, &pristine_home);
    for f in ["torpor.db-wal", "torpor.db-shm"] {
        for d in std::fs::read_dir(&pristine_home).unwrap() {
            let _ = std::fs::remove_file(d.unwrap().path().join(f));
        }
    }
    Fixture { sim, pristine_proj, pristine_home, pre, target }
}

fn reset(f: &Fixture) {
    clear_dir(&f.sim.env.thome);
    copy_tree(&f.pristine_home, &f.sim.env.thome);
    clear_dir(&f.sim.env.proj);
    copy_tree(&f.pristine_proj, &f.sim.env.proj);
}

fn steps(o: &Out, domain: &str) -> usize {
    o.stderr.lines().filter(|l| l.starts_with(&format!("fault-step {domain} "))).count()
}

#[derive(Default, Debug)]
struct Tally {
    runs: usize,
    killed: usize,
    mixed_before_recover: usize,
    pre_before: usize,
    post_before: usize,
    ended_pre: usize,
    ended_post: usize,
}

/// One crash: reset, run `back 1` with the fault, optionally crash `recover` once, run `recover`, check everything.
fn crash_run(f: &Fixture, fault: &str, recover_fault: Option<String>, tally: &mut Tally) {
    let sim = &f.sim;
    reset(f);
    let o = sim.cli_env(&["back", "1"], &[("TORPOR_FAULT", fault)]);
    tally.runs += 1;
    if o.code == -1 {
        tally.killed += 1;
    } else {
        assert_eq!(o.code, 0, "{fault}: neither killed nor done: {}{}", o.stdout, o.stderr);
    }
    let mid = oracle(sim.proj());
    if mid == f.pre {
        tally.pre_before += 1;
    } else if mid == f.target {
        tally.post_before += 1;
    } else {
        tally.mixed_before_recover += 1;
    }
    if let Some(rf) = recover_fault {
        let r = sim.cli_env(&["recover"], &[("TORPOR_FAULT", &rf)]);
        assert!(r.code == -1 || r.code == 0, "{fault} then {rf}: recover exited {} {}{}", r.code, r.stdout, r.stderr);
    }
    let r = sim.cli(&["recover"]);
    assert_eq!(r.code, 0, "{fault}: recover exit {}: {}{}", r.code, r.stdout, r.stderr);
    let after = oracle(sim.proj());
    if after == f.target {
        tally.ended_post += 1;
    } else if after == f.pre {
        tally.ended_pre += 1;
    } else {
        let diff: Vec<_> = after.iter().filter(|(k, v)| f.pre.get(*k) != Some(v) && f.target.get(*k) != Some(v)).map(|(k, _)| k.clone()).collect();
        panic!("{fault}: after recover the folder is neither before nor after; odd paths: {diff:?}");
    }
    assert_store_sound(&sim.env);
    sim.verify_last_point();
    assert!(!sim.proj().join(".torpor-stage").exists(), "{fault}: the stage folder is still there");
    let left: Vec<_> = std::fs::read_dir(sim.ws_dir().join("journal")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "{fault}: journal left behind: {left:?}");
    assert_eq!(sim.q_i64("SELECT COUNT(*) FROM restores WHERE status IN ('staging','applying')"), 0, "{fault}: restore row still open");
    let again = sim.cli(&["recover"]);
    assert_eq!((again.code, again.stdout.as_str()), (0, "Nothing to recover.\n"), "{fault}");
}

#[test]
fn a3_killed_restores_end_before_or_after_never_mixed() {
    let f = fixture();
    // Count the steps of one clean run.
    reset(&f);
    let o = f.sim.cli_env(&["back", "1"], &[("TORPOR_FAULT", "restore:0")]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let s = steps(&o, "restore");
    let post_clean = oracle(f.sim.proj());
    assert_eq!(post_clean, f.target, "a clean restore reaches the end of turn 1 exactly");
    assert!(s >= 30, "a restore of this size has many steps ({s})");
    let changed = f.pre.iter().filter(|(k, v)| f.target.get(*k) != Some(v)).count() + f.target.keys().filter(|k| !f.pre.contains_key(*k)).count();
    assert!((5..=60).contains(&changed), "5-30 file restore ({changed} paths differ)");

    let mut t = Tally::default();
    for i in 0..RUNS {
        let k = 1 + (i * 7) % s;
        let rf = (i % 5 == 0).then(|| format!("restore:{}", 1 + (i / 5) % 9));
        crash_run(&f, &format!("restore:{k}"), rf, &mut t);
    }
    // Every step at least once, in order.
    for k in 1..=s {
        crash_run(&f, &format!("restore:{k}"), None, &mut t);
    }
    eprintln!("A3 restore: {} runs ({s} steps in a clean run, {changed} paths differ): {t:?}", t.runs);
    assert_eq!(t.runs, RUNS + s);
    assert_eq!(t.killed, t.runs, "every run was really killed");
    assert!(t.mixed_before_recover > 0, "the kills must land in the middle of the restore for recover to have work: {t:?}");
    assert!(t.ended_pre > 0 && t.ended_post > 0, "both outcomes happen: {t:?}");
    assert_eq!(t.ended_pre + t.ended_post, t.runs);
}

#[test]
fn a3_kills_during_capture() {
    let f = fixture();
    reset(&f);
    let o = f.sim.cli_env(&["back", "1"], &[("TORPOR_FAULT", "capture:0")]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let c = steps(&o, "capture");
    assert!(c >= 6, "two captures with three steps each ({c})");
    let mut t = Tally::default();
    for k in 1..=c {
        crash_run(&f, &format!("capture:{k}"), None, &mut t);
    }
    eprintln!("A3 capture: {} runs: {t:?}", t.runs);
    assert_eq!(t.killed, c);
}
