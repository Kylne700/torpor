//! A6 (spec section 10): during a session with no restores, Torpor writes nothing outside TORPOR_HOME except the key
//! file (once, with its parent folders). Nothing in the project, nothing inside `.git`, nothing else in HOME.
//! One inotify watch covers the whole project (the real `git init` folder included) and the fake HOME. The harness's
//! own writes are thrown away just before each Torpor process starts; whatever arrives while it runs is the evidence.
#![cfg(target_os = "linux")] // inotify
mod common;
use common::*;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

struct Watch {
    ino: Inotify,
    dirs: HashMap<WatchDescriptor, PathBuf>,
}

fn mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MODIFY
        | WatchMask::ATTRIB
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::CLOSE_WRITE
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
}

impl Watch {
    fn new() -> Watch {
        Watch { ino: Inotify::init().unwrap(), dirs: HashMap::new() }
    }

    /// Watch `dir` and every folder below it (links are not followed).
    fn add_tree(&mut self, dir: &Path) {
        match self.ino.watches().add(dir, mask()) {
            Ok(wd) => {
                self.dirs.insert(wd, dir.to_path_buf());
            }
            Err(_) => return, // already gone again
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                self.add_tree(&e.path());
            }
        }
    }

    /// Everything that arrived since the last call. Folders that appeared get watched too.
    fn drain(&mut self) -> Vec<(PathBuf, EventMask)> {
        let mut buf = vec![0u8; 1 << 16];
        let mut got = Vec::new();
        let mut fresh = Vec::new();
        loop {
            let events = match self.ino.read_events(&mut buf) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("inotify: {e}"),
            };
            for ev in events {
                assert!(!ev.mask.contains(EventMask::Q_OVERFLOW), "the inotify queue overflowed: events were lost");
                if ev.mask.contains(EventMask::IGNORED) {
                    continue;
                }
                let Some(dir) = self.dirs.get(&ev.wd) else { continue };
                let path = match ev.name {
                    Some(n) => dir.join(n),
                    None => dir.clone(),
                };
                if ev.mask.contains(EventMask::ISDIR) && (ev.mask.contains(EventMask::CREATE) || ev.mask.contains(EventMask::MOVED_TO)) {
                    fresh.push(path.clone());
                }
                got.push((path, ev.mask));
            }
        }
        for d in fresh {
            self.add_tree(&d);
        }
        got
    }
}

type Seen = Rc<RefCell<Vec<(usize, String, PathBuf, EventMask)>>>;

/// Make every Torpor process of `sim` an observed step: events before it are thrown away, events during it are kept.
fn attach(sim: &Sim, w: &Rc<RefCell<Watch>>, seen: &Seen, thrown: &Rc<Cell<usize>>, step: &Rc<Cell<usize>>) {
    let (w, seen, thrown, step) = (w.clone(), seen.clone(), thrown.clone(), step.clone());
    *sim.observer.borrow_mut() = Some(Box::new(move |after, label| {
        let events = w.borrow_mut().drain();
        if after {
            step.set(step.get() + 1);
            for (p, m) in events {
                seen.borrow_mut().push((step.get(), label.to_string(), p, m));
            }
        } else {
            thrown.set(thrown.get() + events.len());
        }
    }));
}

fn git(env: &Env, args: &[&str]) {
    let o = Command::new("git")
        .args(args)
        .current_dir(&env.proj)
        .env("HOME", &env.home)
        .env("XDG_CONFIG_HOME", env.home.join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn a6_torpor_writes_only_inside_its_store_and_the_key_file() {
    let mut sim = Sim::new();
    let env = sim.env.clone();
    write_files(
        &env.proj,
        &[
            ("src/main.rs", "fn main() {}\n"),
            ("src/old.rs", "// old\n"),
            ("README.md", "Hello\n"),
            (".env", "TOKEN=abc123\n"),
            (".gitignore", "ignored.log\n"),
            ("ignored.log", "noise\n"),
        ],
    );
    git(&env, &["init", "-q", "-b", "main"]);
    git(&env, &["config", "maintenance.auto", "false"]); // git runs auto-maintenance in the background after a commit; its lock file would land inside a Torpor step
    git(&env, &["add", "-A"]);
    git(&env, &["commit", "-q", "-m", "first"]);

    let w = Rc::new(RefCell::new(Watch::new()));
    w.borrow_mut().add_tree(&env.proj);
    w.borrow_mut().add_tree(&env.home);
    assert!(w.borrow().dirs.values().any(|p| p.starts_with(env.proj.join(".git").join("objects"))), ".git must be watched too");
    let (seen, thrown, step): (Seen, Rc<Cell<usize>>, Rc<Cell<usize>>) = Default::default();
    attach(&sim, &w, &seen, &thrown, &step);

    // One long session that uses every kind of event.
    let home = env.home.to_string_lossy().into_owned();
    let git_env = format!("export HOME='{home}' GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com");
    sim.session_start();
    sim.prompt("turn one");
    sim.edit("src/main.rs", "fn main() { println!(\"hi\"); }\n");
    sim.write_tool("src/new/deep/file.rs", "pub fn f() {}\n");
    sim.bash("sed -i 's/Hello/Howdy/' README.md");
    sim.bash(&format!("{git_env}; git add -A && git commit -qm second"));
    sim.bash("rm src/old.rs");
    sim.bash("exit 3");
    sim.subagent_edit("agent-1", "lib/x.rs", "pub fn x() {}\n");
    sim.subagent_bash("agent-1", "echo y >> lib/x.rs");
    sim.subagent_stop("agent-1");
    sim.stop();
    sim.prompt("turn two");
    sim.outside_edit("README.md", "Hello again\n");
    sim.write_tool(".env", "TOKEN=zzz999\n");
    sim.bash(&format!("{git_env}; git add -A && git commit -qm third"));
    sim.stop();
    sim.hook("PreCompact", serde_json::json!({"trigger": "auto"}));
    sim.hook("StopFailure", serde_json::json!({"error": "rate_limit"}));
    sim.session_end("other");

    // A second session, and the command line.
    let mut t = sim.second_session();
    attach(&t, &w, &seen, &thrown, &step);
    t.session_start();
    t.prompt("another session");
    t.edit("src/main.rs", "fn main() {}\n");
    t.stop();
    t.status();
    t.cli(&["quiet", "on"]);
    t.cli(&["quiet", "off"]);
    t.cli(&["gc", "--if-quiet"]);
    t.cli(&["version"]);
    t.session_end("other");

    // The evidence: only the key file and its two parent folders may have been touched, in one single step.
    let seen = seen.borrow();
    let allowed = env.home.join(".config");
    let (key, bad): (Vec<_>, Vec<_>) = seen.iter().partition(|(_, _, p, _)| p.starts_with(&allowed));
    assert!(bad.is_empty(), "Torpor wrote outside its store:\n{}", bad.iter().map(|(s, l, p, m)| format!("  step {s} ({l}): {} {m:?}", p.display())).collect::<Vec<_>>().join("\n"));
    let key_steps: BTreeSet<usize> = key.iter().map(|(s, ..)| *s).collect();
    assert_eq!(key_steps.len(), 1, "the key file is made by exactly one hook, got steps {key_steps:?}: {key:?}");
    assert!(env.key.exists(), "the key file exists afterwards");
    for (_, _, p, _) in &key {
        assert!(p.starts_with(env.key.parent().unwrap()) || *p == allowed || p == env.key.parent().unwrap(), "unexpected path under .config: {}", p.display());
    }
    assert!(step.get() >= 35, "only {} observed steps: the scenario did not run", step.get());
    assert!(thrown.get() > 20, "the watch saw only {} harness events: it is not watching", thrown.get());

    // The machinery itself: a write inside .git, a new folder and a file inside it, a file in HOME must all show up.
    let mut wb = w.borrow_mut();
    wb.drain();
    std::fs::write(env.proj.join(".git").join("probe"), "x").unwrap();
    let got: Vec<PathBuf> = wb.drain().into_iter().map(|(p, _)| p).collect();
    assert!(got.contains(&env.proj.join(".git").join("probe")), "the watch missed a write inside .git: {got:?}");
    std::fs::create_dir_all(env.proj.join("newdir/inner")).unwrap();
    wb.drain();
    std::fs::write(env.proj.join("newdir/inner/f.txt"), "x").unwrap();
    std::fs::write(env.home.join("somefile"), "x").unwrap();
    let got: Vec<PathBuf> = wb.drain().into_iter().map(|(p, _)| p).collect();
    for want in [env.proj.join("newdir/inner/f.txt"), env.home.join("somefile")] {
        assert!(got.contains(&want), "the watch missed a write to {}: {got:?}", want.display());
    }
}
