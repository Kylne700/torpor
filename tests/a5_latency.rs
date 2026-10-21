//! A5: latency (spec 10, row A5). Ignored by default: it builds a ~30,000-file project under `target/` and fires about
//! 5,000 hooks through the launcher, one after another, so nothing else should be using the machine.
//!
//!     cargo test --release --test a5_latency -- --ignored --nocapture
//!
//! Project: 8,000 source files in 400 folders (median about 2 KB, none over 200 KB), a node_modules of 20,000 files and a
//! .git of 2,000 files (both hard-excluded, as in a real project), a `.env` and a SQLite file. Warm page cache. Between
//! hooks 1-3 files change. Every event runs 200 times or more (A5_RUNS=n lowers it for a quick look). Budget per event: p50 <= 36 ms, p99 <= 150 ms; the
//! scan (walk + stat) p50 <= 25 ms.
mod common;
use common::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SPEC_RUNS: usize = 200;
const P50_MS: f64 = 36.0;
const P99_MS: f64 = 150.0;
const SCAN_P50_MS: f64 = 25.0;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const WORDS: [&str; 32] = [
    "const", "let", "function", "return", "import", "from", "export", "default", "async", "await", "if", "else", "for", "of", "class", "extends", "this",
    "props", "state", "value", "index", "count", "name", "path", "data", "result", "error", "config", "items", "user", "id", "type",
];

/// Source-looking text: compresses about like code does, and no two files are alike.
fn source_text(r: &mut Rng, n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n + 64);
    while v.len() < n {
        for _ in 0..4 + r.below(8) {
            v.extend_from_slice(WORDS[r.below(32) as usize].as_bytes());
            v.push(b' ');
        }
        v.extend_from_slice(format!("{};\n", r.below(1_000_000)).as_bytes());
    }
    v.truncate(n);
    v
}

/// 55% under 2 KB (median about 1.9 KB), 43% 2-20 KB, 2% 20-200 KB.
fn pick_size(r: &mut Rng) -> usize {
    match r.below(100) {
        0..=54 => 300 + r.below(1700) as usize,
        55..=97 => 2_000 + r.below(18_000) as usize,
        _ => 20_000 + r.below(180_000) as usize,
    }
}

fn put(root: &std::path::Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

/// Returns the relative paths of the 8,000 source files.
fn generate_repo(root: &std::path::Path, r: &mut Rng) -> Vec<String> {
    let exts = ["ts", "tsx", "js", "json", "md", "css"];
    let mut sources = Vec::with_capacity(8000);
    for a in 0..20 {
        for b in 0..20 {
            for k in 0..20 {
                let rel = format!("src/g{a:02}/d{b:02}/f{k:02}.{}", exts[(a + b + k) % exts.len()]);
                let n = pick_size(r);
                put(root, &rel, &source_text(r, n));
                sources.push(rel);
            }
        }
    }
    for p in 0..200 {
        for k in 0..100 {
            let n = 200 + r.below(1300) as usize;
            put(root, &format!("node_modules/pkg{p:03}/lib/f{k:03}.js"), &source_text(r, n));
        }
    }
    put(root, ".git/HEAD", b"ref: refs/heads/main\n");
    put(root, ".git/config", b"[core]\n\trepositoryformatversion = 0\n");
    for x in 0..20 {
        for _k in 0..100 {
            let name = format!("{:038x}", r.next() as u128 * r.next() as u128);
            let bytes: Vec<u8> = (0..200).map(|_| r.next() as u8).collect();
            put(root, &format!(".git/objects/{x:02x}/{name}"), &bytes);
        }
    }
    put(root, "package.json", b"{\"name\":\"a5-project\",\"version\":\"1.0.0\"}\n");
    put(root, ".gitignore", b"node_modules/\ndist/\n.env\n*.log\n");
    put(root, ".env", b"API_KEY=synthetic-a5-not-a-secret\n");
    put(root, "prisma/dev.db", &(0..8192).map(|_| r.next() as u8).collect::<Vec<u8>>());
    sources
}

fn pctl(sorted: &[Duration], p: f64) -> f64 {
    let idx = ((p / 100.0 * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[idx].as_secs_f64() * 1000.0
}

struct Bench {
    sim: Sim,
    rng: Rng,
    sources: Vec<String>,
}

impl Bench {
    fn pick(&mut self) -> String {
        let i = self.rng.below(self.sources.len() as u64) as usize;
        self.sources[i].clone()
    }

    /// 1-3 files change behind the hooks' back (another program, the user, git).
    fn touch_some(&mut self) {
        for _ in 0..1 + self.rng.below(3) {
            let rel = self.pick();
            let n = pick_size(&mut self.rng).min(30_000);
            let text = source_text(&mut self.rng, n);
            std::fs::write(self.sim.path(&rel), text).unwrap();
        }
    }

    fn edit(&mut self) {
        let rel = self.pick();
        let n = pick_size(&mut self.rng).min(30_000);
        let text = String::from_utf8(source_text(&mut self.rng, n)).unwrap();
        self.sim.edit(&rel, &text);
    }

    /// A shell command that changes 1-3 files.
    fn shell_cmd(&mut self, fail: bool) -> String {
        let mut cmd = String::new();
        for _ in 0..1 + self.rng.below(3) {
            let rel = self.pick();
            cmd.push_str(&format!("echo {} >> {rel}; ", self.rng.below(1_000_000)));
        }
        cmd.push_str(if fail { "exit 3" } else { "true" });
        cmd
    }

    /// One turn that ends with Stop and the session closing and reopening.
    fn turn_stop(&mut self) {
        self.touch_some();
        self.sim.session_start();
        self.touch_some();
        self.sim.prompt("work on the project");
        self.edit();
        let c = self.shell_cmd(false);
        assert_eq!(self.sim.bash(&c), 0);
        let c = self.shell_cmd(true);
        assert_eq!(self.sim.bash(&c), 3);
        self.sim.subagent_stop("agent-x");
        self.sim.hook("PreCompact", json!({"trigger": "auto", "custom_instructions": ""}));
        self.touch_some();
        self.sim.stop();
        self.sim.session_end("other");
    }

    /// One turn that ends with StopFailure.
    fn turn_stop_failure(&mut self) {
        self.touch_some();
        self.sim.prompt("work on the project");
        self.edit();
        self.sim.hook("StopFailure", json!({"error": "server_error"}));
    }
}

#[test]
#[ignore = "slow and needs a quiet machine: cargo test --release --test a5_latency -- --ignored --nocapture"]
fn a5_latency() {
    // A5_RUNS lowers the count for quick profiling; only the default is the spec's run.
    let runs: usize = std::env::var("A5_RUNS").ok().and_then(|s| s.parse().ok()).unwrap_or(SPEC_RUNS);
    let base = manifest_dir().join("target").join("a5-run");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let env = Arc::new(Env::in_dir(tempfile::Builder::new().prefix("a5-").tempdir_in(&base).unwrap()));
    let t0 = Instant::now();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let sources = generate_repo(&env.proj, &mut rng);
    println!("generated the project in {:.1} s ({} source files)", t0.elapsed().as_secs_f64(), sources.len());
    if std::env::var_os("A5_KEEP").is_some() {
        // For profiling by hand: the project and its store stay under target/a5-run.
        println!("kept: {}", env.proj.display());
        std::mem::forget(env.clone());
    }

    let root = make_plugin_root(env._tmp.path(), true);
    let mut sim = Sim::with_env(env.clone(), 1);
    sim.program = Some(root.join("bin/torpor"));
    let mut b = Bench { sim, rng, sources };

    // Cold start: the first capture hashes and stores every file. Not part of the budget; printed for the record.
    let t = Instant::now();
    b.sim.session_start();
    b.sim.prompt("first turn");
    let cold = t.elapsed();
    b.sim.stop();
    println!(
        "first capture: {:.0} ms; mode {:?}, file_count {:?}, scan_ms {:?}",
        cold.as_secs_f64() * 1000.0,
        b.sim.q_str("SELECT value FROM meta WHERE key='mode'"),
        b.sim.q_str("SELECT value FROM meta WHERE key='file_count'"),
        b.sim.q_str("SELECT value FROM meta WHERE key='scan_ms'"),
    );
    b.sim.timings.clear();

    // Launcher start-up alone, for reference: `torpor version` through the fast path.
    let mut startup = Vec::new();
    for _ in 0..runs {
        let mut c = cmd_with(&root.join("bin/torpor"), &env);
        c.arg("version");
        startup.push(run_cmd(c, b"").elapsed);
    }
    startup.sort();

    let t = Instant::now();
    for _ in 0..runs {
        b.turn_stop();
    }
    for _ in 0..runs {
        b.turn_stop_failure();
    }
    println!("{} turns in {:.0} s", 2 * runs, t.elapsed().as_secs_f64());

    let mut by_event: BTreeMap<String, Vec<Duration>> = BTreeMap::new();
    for (e, d) in b.sim.timings.drain(..) {
        by_event.entry(e).or_default().push(d);
    }
    let mut failures: Vec<String> = Vec::new();
    println!("\n{:<28} {:>5} {:>9} {:>9} {:>9}", "event", "runs", "p50 ms", "p99 ms", "max ms");
    println!("{:<28} {:>5} {:>9.1} {:>9.1} {:>9.1}", "(launcher: torpor version)", startup.len(), pctl(&startup, 50.0), pctl(&startup, 99.0), pctl(&startup, 100.0));
    let mut all: Vec<Duration> = Vec::new();
    for (e, v) in by_event.iter_mut() {
        v.sort();
        all.extend(v.iter().copied());
        let (p50, p99, max) = (pctl(v, 50.0), pctl(v, 99.0), pctl(v, 100.0));
        println!("{e:<28} {:>5} {p50:>9.1} {p99:>9.1} {max:>9.1}", v.len());
        if v.len() < runs {
            failures.push(format!("{e}: only {} runs", v.len()));
        }
        if p50 > P50_MS {
            failures.push(format!("{e}: p50 {p50:.1} ms is over {P50_MS} ms"));
        }
        if p99 > P99_MS {
            failures.push(format!("{e}: p99 {p99:.1} ms is over {P99_MS} ms"));
        }
    }
    all.sort();
    println!("{:<28} {:>5} {:>9.1} {:>9.1} {:>9.1}", "(all hooks together)", all.len(), pctl(&all, 50.0), pctl(&all, 99.0), pctl(&all, 100.0));

    // The scan alone (walk + stat + ignore rules), warm, with the thread count the product uses for this size.
    let files = b.sim.q_str("SELECT value FROM meta WHERE key='file_count'").and_then(|s| s.parse().ok()).unwrap_or(8000);
    let threads = torpor::scan::thread_count(files);
    let mut scans = Vec::new();
    let mut entries = 0;
    for _ in 0..runs {
        let t = Instant::now();
        let s = torpor::scan::scan(&env.proj, threads, torpor::scan::ENTRY_CAP).unwrap();
        scans.push(t.elapsed());
        entries = s.entries.len();
    }
    scans.sort();
    let scan_p50 = pctl(&scans, 50.0);
    println!("\nscan ({entries} entries, {threads} threads): p50 {scan_p50:.1} ms, p99 {:.1} ms, max {:.1} ms (budget p50 {SCAN_P50_MS} ms)", pctl(&scans, 99.0), pctl(&scans, 100.0));
    println!(
        "store: mode {:?}, points {}, faults {}",
        b.sim.q_str("SELECT value FROM meta WHERE key='mode'"),
        b.sim.q_i64("SELECT COUNT(*) FROM points"),
        b.sim.q_i64("SELECT COUNT(*) FROM faults"),
    );
    if scan_p50 > SCAN_P50_MS {
        failures.push(format!("scan p50 {scan_p50:.1} ms is over {SCAN_P50_MS} ms"));
    }
    assert_eq!(b.sim.q_i64("SELECT COUNT(*) FROM faults"), 0, "no hook faulted during the run");
    b.sim.verify_last_point();
    assert!(failures.is_empty(), "A5 budget missed:\n{}", failures.join("\n"));
}
