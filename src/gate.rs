//! The promise gate at Stop (spec 18): which promises to check after a turn, the runs, the receipt, and the one case that blocks.
//! Fail-open: whatever goes wrong here, the Stop is allowed (the caller prints what phase 1 already had).
//! A block needs all of: tier blocking, 3 of 3 fresh runs failing at an assertion, a control run on the last good version that passes,
//! a watched file in the diff since then, a check that can be verified, a gate that is on, a chain that has not blocked twice,
//! and no sign of flakiness (a version that both passed and failed).
use crate::findbreak;
use crate::promise::{self, Call, Checked, Promise};
use crate::push;
use crate::runner::{self, Verdict};
use crate::scratch::{self, RunResult};
use crate::store::*;
use crate::turn;
use crate::util::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::Instant;

/// How long the gate may wait for the run lock of another check.
const LOCK_WAIT_MS: u64 = 3000;
const BLOCK_WORDS: usize = 100;
const KEEP_RECEIPTS: i64 = 1000;
/// Run time assumed for a promise that has not run yet.
const GUESS_MS: u64 = 5000;
/// At most two blocks per Stop chain: the first, and one more if files changed since.
const CHAIN_BLOCKS: i64 = 2;

pub struct Input {
    pub session: String,
    pub turn_id: i64,
    pub turn_n: i64,
    pub prompt_id: String,
    pub end_pt: i64,
    pub tree: [u8; 32],
    pub now: i64,
    /// Nothing may still be running after this moment.
    pub until: Instant,
    /// The notices phase 1 already marked as told; a block cannot print them, so the next Stop does.
    pub notices: Vec<String>,
}

pub enum Out {
    Nothing,
    /// More lines for the Stop message (after phase 1's own).
    Lines(Vec<String>),
    /// The whole answer: `{"decision":"block","reason":..}`.
    Block(String),
}

// ---------------------------------------------------------------- what counts as a change that matters

const LOCKFILES: [&str; 14] = [
    "package-lock.json", "npm-shrinkwrap.json", "pnpm-lock.yaml", "yarn.lock", "bun.lock", "bun.lockb", "poetry.lock", "uv.lock", "Pipfile.lock", "Cargo.lock",
    "Gemfile.lock", "composer.lock", "go.sum", "package.json",
];

fn base(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Files that can change what any check sees: dependencies, settings, middleware, database shape.
pub fn is_global(p: &str) -> bool {
    let b = base(p);
    LOCKFILES.contains(&b)
        || b.starts_with(".env")
        || (!p.contains('/') && b.contains("config."))
        || b.starts_with("middleware.")
        || p == "prisma/schema.prisma"
        || p.starts_with("migrations/")
        || p.contains("/migrations/")
        || b.ends_with(".sql")
        || (b.starts_with("requirements") && b.ends_with(".txt"))
}

/// Docs, tests and licence files: a turn that changes only these selects nothing.
pub fn doc_like(p: &str) -> bool {
    let low = p.to_lowercase();
    let b = base(&low);
    let ext = b.rsplit_once('.').map(|x| x.1).unwrap_or("");
    matches!(ext, "md" | "mdx" | "txt" | "rst" | "adoc")
        || low.split('/').rev().skip(1).any(|d| matches!(d, "docs" | "doc" | "test" | "tests" | "__tests__" | "spec" | "specs" | "e2e"))
        || b.contains(".test.")
        || b.contains(".spec.")
        || b.contains("_test.")
        || b.starts_with("test_")
        || b.starts_with("license")
        || b.starts_with("changelog")
        || b.starts_with("readme")
}

/// A change that may matter to a promise: globals always, other files unless they are docs or tests (unless the promise watches them).
fn relevant(p: &str, watched: Option<&BTreeSet<String>>) -> bool {
    is_global(p) || !doc_like(p) || watched.is_some_and(|w| w.contains(p))
}

fn paths_of(j: &Option<String>) -> Option<Vec<String>> {
    j.as_deref().and_then(|s| serde_json::from_str(s).ok())
}

/// The files a promise watches: the ones the bad-to-good change touched plus the ones its runs read. None = not known yet.
pub fn watched_set(p: &Promise) -> Option<BTreeSet<String>> {
    let (a, b) = (paths_of(&p.watched_json), paths_of(&p.deps_json));
    if a.is_none() && b.is_none() {
        return None;
    }
    Some(a.into_iter().flatten().chain(b.into_iter().flatten()).collect())
}

fn overlap_of(w: &BTreeSet<String>, diff: &[String]) -> usize {
    diff.iter().filter(|x| w.contains(*x) || is_global(x)).count()
}

// ---------------------------------------------------------------- small readers

pub fn eligible(p: &Promise) -> bool {
    p.state == "live" && matches!(p.tier.as_str(), "advisory" | "unproven" | "shadow" | "blocking")
}

pub fn eligible_count(db: &Connection) -> i64 {
    db.query_row("SELECT COUNT(*) FROM promises WHERE state='live' AND tier IN ('advisory','unproven','shadow','blocking')", [], |r| r.get(0)).unwrap_or(0)
}

fn id_list(db: &Connection, key: &str) -> Vec<i64> {
    meta_get(db, key).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn latest_verdict(db: &Connection, id: i64) -> Option<String> {
    db.query_row("SELECT verdict FROM promise_runs WHERE promise_id=?1 AND kind IN ('check','gate') ORDER BY id DESC LIMIT 1", [id], |r| r.get(0)).optional().ok().flatten()
}

/// Stops since this promise last ran (the current Stop has no receipt yet).
fn stops_since_run(db: &Connection, id: i64) -> i64 {
    db.query_row("SELECT COUNT(*) FROM receipts WHERE at>COALESCE((SELECT MAX(at) FROM promise_runs WHERE promise_id=?1),0)", [id], |r| r.get(0)).unwrap_or(0)
}

fn est_ms(db: &Connection, id: i64) -> u64 {
    db.query_row(
        "SELECT AVG(ms) FROM (SELECT ms FROM promise_runs WHERE promise_id=?1 AND kind IN ('create','check','gate') AND verdict<>'UNRUNNABLE' AND ms>0 ORDER BY id DESC LIMIT 3)",
        [id],
        |r| r.get::<_, Option<f64>>(0),
    )
    .ok()
    .flatten()
    .map(|f| f as u64)
    .unwrap_or(GUESS_MS)
}

fn fits(est: u64) -> bool {
    runner::time_left().is_none_or(|l| l.as_millis() as u64 >= est * 5 / 4 + 300)
}

/// A tree on which this promise has both passed and failed: the check is not stable (any run kind counts).
fn mixed_tree(db: &Connection, id: i64) -> bool {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM promise_runs a JOIN promise_runs b ON b.promise_id=a.promise_id AND b.tree_id=a.tree_id \
         WHERE a.promise_id=?1 AND a.verdict='PASS' AND b.verdict='FAIL')",
        [id],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

/// FAIL share of the runs on trees that passed at least once.
fn flake_hat(db: &Connection, id: i64) -> f64 {
    let (f, n): (i64, i64) = db
        .query_row(
            "SELECT COALESCE(SUM(verdict='FAIL'),0), COUNT(*) FROM promise_runs WHERE promise_id=?1 AND verdict IN ('PASS','FAIL') \
             AND tree_id IN (SELECT tree_id FROM promise_runs WHERE promise_id=?1 AND verdict='PASS')",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    if n == 0 { 0.0 } else { f as f64 / n as f64 }
}

// ---------------------------------------------------------------- the Stop chain

/// Blocks so far in this chain (session, prompt) and the tree of the newest one.
/// An error is returned, never read as "no blocks yet": that would lift the cap. The caller fails open (the Stop is allowed).
fn chain_state(db: &Connection, session: &str, prompt: &str) -> Result<(i64, Option<Vec<u8>>)> {
    let sum: i64 = db.query_row("SELECT COALESCE(SUM(blocks),0) FROM gate_chain WHERE session_id=?1 AND prompt_id=?2", params![session, prompt], |r| r.get(0))?;
    let tree: Option<Vec<u8>> = db
        .query_row("SELECT tree_id FROM gate_chain WHERE session_id=?1 AND prompt_id=?2 AND blocks>0 ORDER BY rowid DESC LIMIT 1", params![session, prompt], |r| r.get(0))
        .optional()?
        .flatten();
    Ok((sum, tree))
}

fn chain_open(sum: i64, last: &Option<Vec<u8>>, tree: &[u8; 32]) -> bool {
    sum == 0 || (sum < CHAIN_BLOCKS && last.as_deref() != Some(&tree[..]))
}

// ---------------------------------------------------------------- the targeted line

/// Longest list of promise numbers in the targeted line (more are counted).
const TOUCH_IDS: usize = 5;

fn touch_line(ids: &[i64]) -> String {
    let shown = &ids[..ids.len().min(TOUCH_IDS)];
    let nums: Vec<String> = shown.iter().map(|i| format!("#{i}")).collect();
    let more = if ids.len() > shown.len() { format!(" and {} more", ids.len() - shown.len()) } else { String::new() };
    let run = if ids.len() > shown.len() { "--all".to_string() } else { shown.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" ") };
    format!("Torpor: edit touches promise{} {}{more}; `torpor check {run}` before finishing", if ids.len() == 1 { "" } else { "s" }, nums.join(", "))
}

/// Spec 18 (B8): an edit whose files meet the watched set of a promise says so, once per promise per turn and within the push budgets (spec 23).
/// None = nothing to say. Only the numbers of promises are in the line, never their names.
pub fn touched(db: &Connection, session: &str, turn_id: i64, turn_n: i64, rels: &[String]) -> Option<String> {
    let ps = promise::all(db, false).ok()?;
    let told = |id: i64| -> bool {
        db.query_row("SELECT EXISTS(SELECT 1 FROM push_log WHERE turn_id=?1 AND kind=?2 AND delivered=1)", params![turn_id, format!("touch:{id}")], |r| r.get(0)).unwrap_or(true)
    };
    let ids: Vec<i64> = ps
        .iter()
        .filter(|p| eligible(p) && watched_set(p).is_some_and(|w| rels.iter().any(|r| w.contains(r))))
        .map(|p| p.id)
        .filter(|id| !told(*id))
        .collect();
    let first = *ids.first()?;
    let line = touch_line(&ids);
    if !push::admit(db, &push::Push { session, turn_id, turn_n, kind: &format!("touch:{first}"), text: &line, window: 1, must: false }).ok()? {
        return None;
    }
    // The other promises of the line are told too (no tokens of their own: the line was counted once).
    let cycle = push::compact_n(db, session);
    for id in &ids[1..] {
        let _ = db.execute(
            "INSERT INTO push_log(session_id,turn_id,compact_n,kind,chars,tokens,delivered) VALUES(?1,?2,?3,?4,0,0,1)",
            params![session, turn_id, cycle, format!("touch:{id}")],
        );
    }
    Some(line)
}

// ---------------------------------------------------------------- selection

struct Cand {
    p: Promise,
    watched: Option<BTreeSet<String>>,
    lp: Option<[u8; 32]>,
    diff: Option<Vec<String>>,
    forced: bool,
    failed_last: bool,
    overlap: usize,
    est: u64,
}

/// Spec 18: a turn that changed nothing but docs and tests (or nothing) selects nothing. Of the rest, a promise is run when any of: left unverified
/// last time; the watched set meets the files changed since the last PASS; the last verdict was FAIL or UNRUNNABLE; not run in the last 3 Stops; no deps.
/// (Spec 18 also says "unverified gets top priority": that is the order among the selected, not a way past the relevance filter, so a README-only
/// turn runs nothing; the promise stays unverified and goes first at the next turn that changes something relevant.)
fn select(ws: &Ws, inp: &Input, ps: Vec<Promise>, paths: &[String]) -> Vec<Cand> {
    let db = &ws.conn;
    let unrun = id_list(db, "gate_unrun");
    let mut diffs: HashMap<[u8; 32], Option<Vec<String>>> = HashMap::new();
    let mut out: Vec<Cand> = Vec::new();
    for p in ps {
        let watched = watched_set(&p);
        let forced = unrun.contains(&p.id);
        if !paths.iter().any(|x| relevant(x, watched.as_ref())) {
            continue;
        }
        let lp = p.last_pass_point.and_then(|pt| promise::point_tree(db, pt).ok());
        let diff = lp.and_then(|t| diffs.entry(t).or_insert_with(|| findbreak::changed_files_opt(ws, &t, &inp.tree)).clone());
        let overlap = match (&watched, &diff) {
            (Some(w), Some(d)) => overlap_of(w, d),
            _ => 0,
        };
        let failed_last = matches!(latest_verdict(db, p.id).as_deref(), Some("FAIL" | "UNRUNNABLE"));
        let picked = forced || failed_last || overlap > 0 || p.deps_json.is_none() || watched.is_none() || diff.is_none() || stops_since_run(db, p.id) >= 2;
        if picked {
            let est = est_ms(db, p.id);
            out.push(Cand { p, watched, lp, diff, forced, failed_last, overlap, est });
        }
    }
    // Unverified first, then failed last time, then the bigger overlap, then the shorter run.
    out.sort_by_key(|c| (!c.forced, !c.failed_last, std::cmp::Reverse(c.overlap), c.est));
    out
}

// ---------------------------------------------------------------- the runs

enum Control {
    /// Not a block or would-block candidate: no control run.
    No,
    /// The last good version cannot be read any more.
    NoPoint,
    Pass(RunResult),
    Fail(RunResult),
    /// Not finished in time.
    Cut,
}

struct FailInfo {
    checked: Checked,
    step_why: Option<String>,
    control: Control,
}

enum Res {
    /// No verdict: time ran out, the run lock stayed busy, the stored check could not be read, or an earlier block ended the runs.
    /// Carries the raw runs that did finish.
    Cut(Vec<RunResult>),
    Unrunnable(Checked),
    Pass(Checked),
    Flaky(Checked),
    Fail(Box<FailInfo>),
}

fn cut_reason(c: &Checked) -> bool {
    c.reason.contains(runner::OUT_OF_TIME)
}

/// Would this FAIL be a block (or, in shadow, a would-block) apart from the control run and the chain?
fn candidate(c: &Cand, f: &Checked, unverifiable: bool, gate_on: bool, open: bool) -> bool {
    let tier_ok = match c.p.tier.as_str() {
        "blocking" => gate_on && open,
        "shadow" => true,
        _ => false,
    };
    tier_ok
        && !unverifiable
        && f.runs.len() >= 3
        && f.runs.iter().all(|r| r.judged.verdict == Verdict::Fail && r.judged.failed_step.is_some())
        && c.watched.as_ref().zip(c.diff.as_ref()).is_some_and(|(w, d)| overlap_of(w, d) > 0)
}

fn run_all(ws: &Ws, inp: &Input, cands: &[Cand], gate_on: bool, open: bool) -> Vec<Res> {
    let _dl = runner::Deadline::set(inp.until);
    let held = scratch::hold_run_lock(ws, LOCK_WAIT_MS);
    let mut out: Vec<Res> = Vec::new();
    let mut ended = held.is_err();
    for c in cands {
        if ended || runner::out_of_time() || !fits(c.est) {
            out.push(Res::Cut(Vec::new()));
            continue;
        }
        let Ok(l) = promise::spec_of(ws, &c.p) else {
            out.push(Res::Cut(Vec::new()));
            continue;
        };
        let mut checked = promise::check_tree(ws, &l.spec, &inp.tree);
        let res = match checked.call {
            Call::Pass => Res::Pass(checked),
            Call::Flaky => Res::Flaky(checked),
            Call::Unrunnable if cut_reason(&checked) => Res::Cut(std::mem::take(&mut checked.runs)),
            Call::Unrunnable => Res::Unrunnable(checked),
            Call::Fail => {
                let step_why = checked.failed_step.and_then(|n| l.spec.steps.get(n.wrapping_sub(1))).and_then(|s| s.why.clone());
                let control = if !candidate(c, &checked, l.spec.unverifiable, gate_on, open) {
                    Control::No
                } else if let Some(lp) = c.lp {
                    // Right after the 3rd FAIL, under the same lock: the last good version must still pass.
                    if fits(c.est) {
                        let r = scratch::run_once(ws, &l.spec, &lp, Some(l.spec.assertions()));
                        match r.judged.verdict {
                            Verdict::Pass => Control::Pass(r),
                            Verdict::Fail => Control::Fail(r),
                            Verdict::Unrunnable => Control::Cut,
                        }
                    } else {
                        Control::Cut
                    }
                } else {
                    Control::NoPoint
                };
                if matches!(control, Control::Pass(_)) && c.p.tier == "blocking" {
                    ended = true; // the block ends this Stop; the rest run at the next one
                }
                Res::Fail(Box::new(FailInfo { checked, step_why, control }))
            }
        };
        out.push(res);
    }
    out
}

// ---------------------------------------------------------------- writing the answer

#[derive(Default)]
struct Applied {
    lines: Vec<String>,
    block: Option<String>,
    outcomes: BTreeSet<&'static str>,
    ran: i64,
    unrun: Vec<i64>,
    /// The one failure worth a model-visible line at the next prompt: (kind, text).
    ups: Option<(String, String)>,
}

fn record_runs(db: &Connection, id: i64, kind: &str, tree: &[u8; 32], runs: &[RunResult], now: i64) -> Result<()> {
    for r in runs {
        // A run cut by the clock says nothing about the check.
        if r.judged.verdict == Verdict::Unrunnable && r.judged.reason.contains(runner::OUT_OF_TIME) {
            continue;
        }
        promise::record(db, id, kind, Some(tree), r, now)?;
    }
    Ok(())
}

fn become_flaky(db: &Connection, id: i64) -> Result<()> {
    db.execute("UPDATE promises SET state='flaky', tier=CASE WHEN tier IN ('shadow','blocking') THEN 'advisory' ELSE tier END WHERE id=?1", [id])?;
    promise::end_shadow_wait(db, id)
}

/// Spec 17: a shadow promise passed. A would-block failure on another version has turned into this PASS (a dispute or a drop since would have
/// cleared the note): a true catch, and from now on it blocks. Otherwise the first PASS of a day is a clean day, and 3 clean days do the same.
fn shadow_pass(db: &Connection, p: &Promise, inp: &Input) -> Result<Option<String>> {
    let id = p.id;
    if let Some(failed) = meta_get(db, &format!("would:{id}")) {
        if failed != hex(&inp.tree) && promise::promote(db, id, true)? {
            return Ok(Some(format!("Torpor: promise {} would have stopped the AI and it was fixed since, so it stops the AI from now on. torpor promise show {id}", short_name(p))));
        }
        return Ok(None);
    }
    let (day, key) = (utc_day(inp.now), format!("shadow_day:{id}"));
    if meta_get_i64(db, &key) == Some(day) {
        return Ok(None);
    }
    meta_set(db, &key, &day.to_string())?;
    db.execute("UPDATE promises SET shadow_days=shadow_days+1 WHERE id=?1", [id])?;
    let n: i64 = db.query_row("SELECT shadow_days FROM promises WHERE id=?1", [id], |r| r.get(0))?;
    if n >= 3 && promise::promote(db, id, false)? {
        return Ok(Some(format!("Torpor: promise {} passed on {n} different days without a false alarm, so it stops the AI from now on. torpor promise show {id}", short_name(p))));
    }
    Ok(None)
}

fn set_hat(db: &Connection, id: i64) -> Result<()> {
    db.execute("UPDATE promises SET flake_hat=?2 WHERE id=?1", params![id, flake_hat(db, id)])?;
    Ok(())
}

fn flaky_line(p: &Promise) -> String {
    format!("Torpor: promise {} passed and failed on the same version, so it is flaky and advisory only. torpor promise show {}", short_name(p), p.id)
}

fn short_name(p: &Promise) -> String {
    format!("#{} ({})", p.id, promise::clean(&p.name, 60))
}

fn passed_on(db: &Connection, p: &Promise) -> String {
    match p.last_pass_point {
        Some(pt) => format!("it passed on {}", promise::good_name(db, pt)),
        None => "it passed before".to_string(),
    }
}

/// The last turn that changed any of `files`.
fn latest_turn_of(db: &Connection, files: &[String]) -> Option<i64> {
    files
        .iter()
        .filter_map(|f| db.query_row("SELECT t.n FROM changes c JOIN turns t ON t.id=c.turn_id WHERE c.path=?1 ORDER BY c.point_id DESC LIMIT 1", [f], |r| r.get::<_, i64>(0)).optional().ok().flatten())
        .max()
}

fn source_words(db: &Connection, p: &Promise) -> String {
    let who = if p.source == "agent" { "the AI's own" } else { "the user's words" };
    match promise::turn_n(db, p.created_turn) {
        Some(n) => format!("{who}, turn {n}"),
        None => who.to_string(),
    }
}

fn words_n(s: &str) -> usize {
    s.split_whitespace().count()
}

/// The block message (spec 18): one line, at most 100 words, at most 3 file names, the failing step in at most 25 words.
/// Everything that came from a person or a check goes through the class B filters first.
fn block_reason(db: &Connection, c: &Cand, f: &FailInfo) -> String {
    let p = &c.p;
    let id = p.id;
    let name = match push::line_ok(&p.name, 60) {
        Some(n) => format!("promise #{id} \"{n}\""),
        None => format!("promise #{id}"),
    };
    let head = |step: &str| format!("Torpor: {name} ({}) fails now. Failing step: {step}. It failed 3 of 3 fresh runs and still passes on the saved working version.", source_words(db, p));
    let mut files: Vec<String> = c.diff.clone().unwrap_or_default();
    if let Some(w) = &c.watched {
        files.sort_by_key(|x| !(w.contains(x) || is_global(x)));
    }
    let changed = if files.is_empty() {
        String::new()
    } else {
        let turn = latest_turn_of(db, &files[..files.len().min(3)]).map(|n| format!(" (turn {n})")).unwrap_or_default();
        format!(" Changed since it last passed: {}{turn}.", push::names_phrase(&files))
    };
    let tail = format!(
        " Fix the app, not the check, and do not special-case test values. If this promise is wrong or outdated run `torpor promise dispute {id} \"<why>\"`; it asks the user instead of you."
    );
    let step_n = f.checked.failed_step.unwrap_or(0);
    let generic = if step_n > 0 { format!("step {step_n} of the check") } else { "a step of the check".to_string() };
    let step = f.step_why.as_deref().and_then(|w| push::line_ok(w, 200)).unwrap_or(generic.clone());
    let fixed = words_n(&head("")) + words_n(&changed) + words_n(&tail);
    let allow = BLOCK_WORDS.saturating_sub(fixed).min(25);
    let step = if allow >= 3 { push::words(&step, allow) } else { generic };
    format!("{}{changed}{tail}", head(&step))
}

fn fail_line(db: &Connection, c: &Cand, would: bool) -> String {
    let p = &c.p;
    let tail = if would { "It would have stopped the AI, but it is in shadow mode." } else { "The AI was not stopped." };
    format!("Torpor: promise {} failed after this turn (3 of 3 runs; {}). {tail} torpor promise show {}", short_name(p), passed_on(db, p), p.id)
}

fn ups_text(turn_n: i64, c: &Cand, f: &FailInfo) -> String {
    let id = c.p.id;
    let with = f
        .step_why
        .as_deref()
        .and_then(|w| push::line_ok(w, 200))
        .map(|w| format!("Torpor: promise #{id} failed after turn {turn_n}: {}. Run torpor check {id}.", push::words(&w, 14)))
        .filter(|t| t.chars().count() <= 120);
    with.unwrap_or_else(|| format!("Torpor: promise #{id} failed after turn {turn_n}. Run torpor check {id}."))
}

/// Everything the gate learned, in one transaction: runs, flags, tiers, chain, receipt. Safe to run twice (the store retries on busy).
fn apply(w: &Ws, inp: &Input, cands: &[Cand], res: &[Res], gate_on: bool, skipped: &[String]) -> Result<Applied> {
    let db = &w.conn;
    let mut a = Applied::default();
    a.lines.extend(skipped.iter().cloned());
    if !skipped.is_empty() {
        a.outcomes.insert("unresolved");
    }
    let (sum, last) = chain_state(db, &inp.session, &inp.prompt_id)?;
    let mut open = chain_open(sum, &last, &inp.tree);
    for (c, r) in cands.iter().zip(res) {
        let id = c.p.id;
        match r {
            Res::Cut(runs) => {
                record_runs(db, id, "gate", &inp.tree, runs, inp.now)?;
                a.outcomes.insert("unverified");
                a.unrun.push(id);
            }
            Res::Unrunnable(ch) => {
                record_runs(db, id, "gate", &inp.tree, &ch.runs, inp.now)?;
                a.outcomes.insert("unrunnable");
                a.ran += 1;
            }
            Res::Pass(ch) => {
                record_runs(db, id, "gate", &inp.tree, &ch.runs, inp.now)?;
                a.ran += 1;
                set_hat(db, id)?;
                if mixed_tree(db, id) {
                    // It failed on this very version before and passes now.
                    become_flaky(db, id)?;
                    a.lines.push(flaky_line(&c.p));
                    a.outcomes.insert("flaky");
                } else {
                    db.execute("UPDATE promises SET last_pass_point=?2 WHERE id=?1", params![id, inp.end_pt])?;
                    a.outcomes.insert("ok");
                    if c.p.tier == "shadow" {
                        a.lines.extend(shadow_pass(db, &c.p, inp)?);
                    }
                }
            }
            Res::Flaky(ch) => {
                record_runs(db, id, "gate", &inp.tree, &ch.runs, inp.now)?;
                become_flaky(db, id)?;
                set_hat(db, id)?;
                a.lines.push(flaky_line(&c.p));
                a.outcomes.insert("flaky");
                a.ran += 1;
            }
            Res::Fail(f) => {
                record_runs(db, id, "gate", &inp.tree, &f.checked.runs, inp.now)?;
                if !matches!(f.control, Control::Cut) {
                    a.ran += 1;
                }
                let mut block_ok = false;
                match &f.control {
                    Control::Fail(rr) => {
                        if let Some(lp) = c.lp {
                            record_runs(db, id, "control", &lp, std::slice::from_ref(rr), inp.now)?;
                        }
                        db.execute("UPDATE promises SET state='quarantined' WHERE id=?1", [id])?;
                        a.lines.push(format!("Torpor: promise {} fails on the version that passed before, so its check cannot be trusted. It is set aside. torpor promise show {id}", short_name(&c.p)));
                        a.outcomes.insert("quarantined");
                        set_hat(db, id)?;
                        continue;
                    }
                    Control::Cut => {
                        a.outcomes.insert("unverified");
                        a.unrun.push(id);
                        continue;
                    }
                    Control::Pass(rr) => {
                        if let Some(lp) = c.lp {
                            record_runs(db, id, "control", &lp, std::slice::from_ref(rr), inp.now)?;
                        }
                        block_ok = true;
                    }
                    Control::No | Control::NoPoint => {}
                }
                set_hat(db, id)?;
                if mixed_tree(db, id) {
                    become_flaky(db, id)?;
                    a.lines.push(flaky_line(&c.p));
                    a.outcomes.insert("flaky");
                    continue;
                }
                if block_ok && c.p.tier == "blocking" && gate_on && open && a.block.is_none() {
                    let prev: i64 = db.query_row("SELECT COALESCE((SELECT blocks FROM gate_chain WHERE session_id=?1 AND prompt_id=?2 AND promise_id=?3),0)", params![inp.session, inp.prompt_id, id], |r| r.get(0))?;
                    db.execute("DELETE FROM gate_chain WHERE session_id=?1 AND prompt_id=?2 AND promise_id=?3", params![inp.session, inp.prompt_id, id])?;
                    db.execute("INSERT INTO gate_chain(session_id,prompt_id,promise_id,tree_id,blocks) VALUES(?1,?2,?3,?4,?5)", params![inp.session, inp.prompt_id, id, &inp.tree[..], prev + 1])?;
                    let reason = block_reason(db, c, f);
                    a.block = Some(reason);
                    a.outcomes.insert("block");
                    open = false;
                    continue;
                }
                let would = block_ok && c.p.tier == "shadow";
                if would && meta_get(db, &format!("would:{id}")).is_none() {
                    meta_set(db, &format!("would:{id}"), &hex(&inp.tree))?;
                }
                a.lines.push(fail_line(db, c, would));
                a.outcomes.insert(if c.p.tier == "blocking" && !open { "unresolved" } else { "fail" });
                if a.ups.is_none() {
                    a.ups = Some((format!("fail:{id}"), ups_text(inp.turn_n, c, f)));
                }
            }
        }
    }
    Ok(a)
}

pub fn last_receipts_all_unverified(db: &Connection) -> bool {
    let (n, k): (i64, i64) = db
        .query_row("SELECT COUNT(*), COALESCE(SUM(outcome LIKE '%unverified%'),0) FROM (SELECT outcome FROM receipts ORDER BY id DESC LIMIT 3)", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap_or((0, 0));
    n == 3 && k == 3
}

/// One row per Stop, even when nothing was selected. Only the newest KEEP_RECEIPTS stay.
pub fn write_receipt(db: &Connection, inp_session: &str, turn_id: i64, now: i64, selected: i64, ran: i64, outcome: &str) -> Result<()> {
    db.execute("INSERT INTO receipts(session_id,turn_id,at,selected,ran,outcome) VALUES(?1,?2,?3,?4,?5,?6)", params![inp_session, turn_id, now, selected, ran, outcome])?;
    db.execute("DELETE FROM receipts WHERE id<=(SELECT MAX(id) FROM receipts)-?1", [KEEP_RECEIPTS])?;
    Ok(())
}

/// Phase 1 calls this when no promise can be checked: the receipt says the gate looked and had nothing to do.
pub fn receipt_none(db: &Connection, session: &str, turn_id: i64, now: i64) -> Result<()> {
    write_receipt(db, session, turn_id, now, 0, 0, "none")
}

/// True when promises exist and the last 3 Stops left no receipt: the checks did not run.
pub fn silent(db: &Connection) -> bool {
    if eligible_count(db) == 0 {
        return false;
    }
    let (n, k): (i64, i64) = db
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(EXISTS(SELECT 1 FROM receipts r WHERE r.turn_id=t.id)),0) FROM (SELECT id FROM turns WHERE end_kind='stop' ORDER BY id DESC LIMIT 3) t",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    n == 3 && k == 0
}

pub const SILENT_NOTICE: &str = "Torpor's promise checks did not run at the last 3 turn ends.";

/// Run the gate for one Stop. Never blocks unless every condition at the top of this file holds.
pub fn run(root: &Path, inp: &Input) -> Result<Out> {
    let mut ws = Ws::open(root, false)?;
    let ps: Vec<Promise> = promise::all(&ws.conn, false)?.into_iter().filter(eligible).collect();
    if ps.is_empty() {
        return Ok(Out::Nothing);
    }
    let paths: Vec<String> = turn::net_changes(&ws.conn, inp.turn_id, "'edit','shell','subagent','outside'")?.into_iter().map(|n| n.path).collect();
    let (sum, last) = chain_state(&ws.conn, &inp.session, &inp.prompt_id)?;
    let open = chain_open(sum, &last, &inp.tree);
    let gate_on = promise::gate_enabled(&ws.conn);
    let mut cands = select(&ws, inp, ps, &paths);
    // Nothing changed since the AI was last stopped: the same runs would give the same answer, and no second block is allowed.
    let mut skipped: Vec<String> = Vec::new();
    if sum > 0 && last.as_deref() == Some(&inp.tree[..]) {
        let blocked: Vec<i64> = id_list_from_chain(&ws.conn, &inp.session, &inp.prompt_id);
        for c in cands.iter().filter(|c| blocked.contains(&c.p.id)) {
            skipped.push(format!("Torpor: promise {} still fails. The AI was stopped for it already and nothing changed since, so the turn is allowed to end. torpor promise show {}", short_name(&c.p), c.p.id));
        }
        cands.retain(|c| !blocked.contains(&c.p.id));
    }
    let selected = cands.len() as i64;
    if cands.is_empty() {
        let outcome = if skipped.is_empty() { "none" } else { "unresolved" };
        promise::write(&mut ws, |w| write_receipt(&w.conn, &inp.session, inp.turn_id, inp.now, 0, 0, outcome))?;
        return Ok(if skipped.is_empty() { Out::Nothing } else { Out::Lines(skipped) });
    }
    // If this process is killed in the middle, these stay first in line for the next Stop.
    let ids = serde_json::to_string(&cands.iter().map(|c| c.p.id).collect::<Vec<_>>())?;
    promise::write(&mut ws, |w| meta_set(&w.conn, "gate_unrun", &ids))?;
    let res = run_all(&ws, inp, &cands, gate_on, open);
    // (the run lock ended with run_all; the store lock is only taken here)
    let a = promise::write(&mut ws, |w| {
        let a = apply(w, inp, &cands, &res, gate_on, &skipped)?;
        let db = &w.conn;
        if a.unrun.is_empty() {
            db.execute("DELETE FROM meta WHERE key='gate_unrun'", [])?;
        } else {
            meta_set(db, "gate_unrun", &serde_json::to_string(&a.unrun)?)?;
        }
        let outcome = if a.outcomes.is_empty() { "none".to_string() } else { a.outcomes.iter().copied().collect::<Vec<_>>().join(",") };
        write_receipt(db, &inp.session, inp.turn_id, inp.now, selected, a.ran, &outcome)?;
        let mut a = a;
        if a.outcomes.contains("unverified") && last_receipts_all_unverified(db) {
            a.lines.push("Torpor: promise checks were cut short at the last 3 turn ends. torpor status".to_string());
        }
        match &a.ups {
            Some((k, t)) => meta_set(db, "ups_line", &json!({"kind": k, "text": t}).to_string())?,
            None => {
                db.execute("DELETE FROM meta WHERE key='ups_line'", [])?;
            }
        }
        if let Some(reason) = &a.block {
            let carry = [meta_get(db, "stop_carry").and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or_default(), inp.notices.clone()].concat();
            if !carry.is_empty() {
                meta_set(db, "stop_carry", &serde_json::to_string(&carry)?)?;
            }
            // A block is told whatever the budget (spec 23).
            push::admit(db, &push::Push { session: &inp.session, turn_id: inp.turn_id, turn_n: inp.turn_n, kind: "block", text: reason, window: 0, must: true })?;
        }
        for l in &a.lines {
            let chars = l.chars().count() as i64;
            db.execute("INSERT INTO push_log(session_id,turn_id,compact_n,kind,chars,tokens,delivered) VALUES(?1,?2,?3,'gate',?4,?5,1)", params![inp.session, inp.turn_id, push::compact_n(db, &inp.session), chars, push::tokens(l)])?;
        }
        Ok(a)
    })?;
    Ok(match a.block {
        Some(r) => Out::Block(r),
        None if a.lines.is_empty() => Out::Nothing,
        None => Out::Lines(a.lines),
    })
}

fn id_list_from_chain(db: &Connection, session: &str, prompt: &str) -> Vec<i64> {
    let Ok(mut st) = db.prepare("SELECT promise_id FROM gate_chain WHERE session_id=?1 AND prompt_id=?2 AND blocks>0") else { return Vec::new() };
    st.query_map(params![session, prompt], |r| r.get(0)).map(|rows| rows.filter_map(|x| x.ok()).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_and_tests_do_not_matter_but_globals_always_do() {
        for p in ["README.md", "docs/guide.rst", "tests/app.test.js", "src/a.spec.ts", "src/x_test.go", "LICENSE", "notes.txt", "e2e/login.js", "CHANGELOG.md"] {
            assert!(doc_like(p), "{p}");
        }
        for p in ["src/auth.ts", "server.js", "lib/testing.js", "src/contest.js"] {
            assert!(!doc_like(p), "{p}");
        }
        for p in ["package.json", "web/package-lock.json", ".env.local", "next.config.js", "middleware.ts", "prisma/schema.prisma", "db/migrations/001.sql", "requirements.txt", "a.sql"] {
            assert!(is_global(p), "{p}");
        }
        assert!(!is_global("src/config.rs"), "only root-level config files are global");
        assert!(!is_global("src/auth.ts"));
        assert!(relevant("requirements.txt", None), "a global beats the .txt rule");
        assert!(!relevant("README.md", None));
        let w: BTreeSet<String> = ["docs/api.md".to_string()].into();
        assert!(relevant("docs/api.md", Some(&w)), "a doc the promise watches counts");
    }

    #[test]
    fn the_watched_set_and_overlap() {
        let w: BTreeSet<String> = ["src/a.ts".to_string()].into();
        assert_eq!(overlap_of(&w, &["src/a.ts".into(), "src/b.ts".into(), "package.json".into()]), 2);
        assert_eq!(overlap_of(&w, &["src/b.ts".into()]), 0);
    }

    #[test]
    fn the_chain_allows_a_second_block_only_after_a_change() {
        let t = [1u8; 32];
        let other = [2u8; 32];
        assert!(chain_open(0, &None, &t));
        assert!(!chain_open(1, &Some(t.to_vec()), &t));
        assert!(chain_open(1, &Some(t.to_vec()), &other));
        assert!(!chain_open(2, &Some(t.to_vec()), &other));
    }

    #[test]
    fn a_chain_that_cannot_be_read_is_an_error_not_zero_blocks() {
        let db = Connection::open_in_memory().unwrap(); // no gate_chain table
        assert!(chain_state(&db, "s", "p").is_err());
    }
}
