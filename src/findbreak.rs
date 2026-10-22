//! Find-break (spec 19): which turn broke it. `bayes` and `confirm` are the search and are tested with a synthetic oracle (B3);
//! everything below them runs real checks on saved versions and says the result in words.
use crate::capture;
use crate::cli::{err, open_here, out, usage};
use crate::promise::{self, Done, LIVE_MAX};
use crate::promise_cmd::{self, fail_p, get_live, num, read_check};
use crate::runner::Verdict;
use crate::scratch::{self, RunResult};
use crate::spec::Spec;
use crate::store::*;
use crate::util::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- the search (pure)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    Pass,
    Fail,
    /// The version could not be checked (the app would not start): no news.
    Skip,
}

pub trait Prober {
    /// One run on state `state`.
    fn probe(&mut self, state: usize) -> Probe;
    fn out_of_time(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Certain,
    RunCap,
    Time,
    TooManySkips,
    NoProbes,
}

pub struct Searched {
    /// `post[i]` = the chance that state `good + 1 + i` is the first broken one.
    pub post: Vec<f64>,
    /// Probes taken by this search (not the earlier ones it was given).
    pub runs: usize,
    pub stop: Stop,
    pub skipped: Vec<usize>,
}

pub const CERTAIN: f64 = 0.99;
pub const RUN_CAP: usize = 60;
/// A check that fails at random more often than this cannot be searched with.
pub const FLAKE_MAX: f64 = 0.30;
/// A cell with less chance than this is not part of the answer.
const IN_ANSWER: f64 = 0.01;

/// The noise a search assumes: a good version fails with this chance (spec 19: the stored estimate, at least 5%; 10% when unknown).
pub fn noise(flake_hat: Option<f64>) -> f64 {
    flake_hat.map_or(0.10, |h| h.max(0.05))
}

pub fn too_flaky(flake: f64) -> Option<String> {
    (flake > FLAKE_MAX).then(|| format!("This check fails at random too often ({}%) to search with. Fix the check first.", (flake * 100.0).round() as i64))
}

/// One run's news. A PASS at `s` means the first broken state is after `s`; a FAIL at `s` is likely to mean it is at or before `s`.
fn update(post: &mut [f64], good: usize, s: usize, r: Probe, f: f64) {
    if r == Probe::Skip {
        return;
    }
    for (i, w) in post.iter_mut().enumerate() {
        let c = good + 1 + i;
        match r {
            Probe::Pass if c <= s => *w = 0.0,
            Probe::Fail if c > s => *w *= f,
            _ => {}
        }
    }
    let sum: f64 = post.iter().sum();
    if sum > 0.0 {
        post.iter_mut().for_each(|w| *w /= sum);
    }
}

fn top(post: &[f64]) -> f64 {
    post.iter().cloned().fold(0.0, f64::max)
}

/// The state to run next: of those that can still tell cells apart and were not skipped, the one whose "broken by now" chance is closest to 1/2.
/// None = nothing left that is worth a run (for example every state in between cannot be started).
fn pick(post: &[f64], good: usize, skipped: &BTreeSet<usize>) -> Option<usize> {
    let live = |i: &usize| post[*i] > 1e-12;
    let lo = good + 1 + (0..post.len()).find(live)?;
    let hi = good + 1 + (0..post.len()).rev().find(live)?;
    let (mut cum, mut best): (f64, Option<(f64, usize)>) = (0.0, None);
    for (i, w) in post.iter().enumerate() {
        let s = good + 1 + i;
        cum += w;
        // A state at either end of the answer tells nothing new, and one that splits off less than 1% of the chance is not worth a run.
        if s < lo || s >= hi || skipped.contains(&s) || cum < IN_ANSWER || cum > 1.0 - IN_ANSWER {
            continue;
        }
        let d = (cum - 0.5).abs();
        if best.is_none_or(|(bd, _)| d < bd - 1e-12) {
            best = Some((d, s));
        }
    }
    best.map(|(_, s)| s)
}

/// Spec 19 steps 3-4. States `good` (known to pass) to `tip` (known to fail); cell `c` = "state c is the first broken one".
/// Uniform start, one-sided noise `f`; `seen` are runs already taken in between. Stops at one cell >= 0.99, 60 runs, no time,
/// over half the probes skipped, or when no state is left that could tell the cells apart.
pub fn bayes(good: usize, tip: usize, f: f64, seen: &[(usize, Probe)], p: &mut dyn Prober) -> Searched {
    assert!(good < tip, "a search needs a state between the working one and the broken one");
    let n = tip - good;
    let mut post = vec![1.0 / n as f64; n];
    let mut skipped: BTreeSet<usize> = BTreeSet::new();
    let (mut runs, mut total, mut skips) = (0, seen.len(), seen.iter().filter(|x| x.1 == Probe::Skip).count());
    for &(s, r) in seen {
        update(&mut post, good, s, r, f);
        if r == Probe::Skip {
            skipped.insert(s);
        }
    }
    let stop = loop {
        if top(&post) >= CERTAIN {
            break Stop::Certain;
        }
        if total >= RUN_CAP {
            break Stop::RunCap;
        }
        if p.out_of_time() {
            break Stop::Time;
        }
        if total >= 4 && skips * 2 > total {
            break Stop::TooManySkips;
        }
        let Some(s) = pick(&post, good, &skipped) else { break Stop::NoProbes };
        let r = p.probe(s);
        (runs, total) = (runs + 1, total + 1);
        update(&mut post, good, s, r, f);
        if r == Probe::Skip {
            skipped.insert(s);
            skips += 1;
        }
    };
    Searched { post, runs, stop, skipped: skipped.into_iter().collect() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    Yes,
    No,
    /// No time left to run what is missing.
    Late,
}

fn tally(h: &[(usize, Probe)], s: usize) -> (usize, usize) {
    let n = |r: Probe| h.iter().filter(|x| x.0 == s && x.1 == r).count();
    (n(Probe::Pass), n(Probe::Fail))
}

/// Spec 19 step 5: `parent` (the state before cell `c`, or before the first cell of a turn) passes and `c` fails twice, reusing the runs in `hist`. A PASS on the culprit, or a parent that
/// never passes, means there is no culprit: the check was noise.
pub fn confirm(parent: usize, c: usize, mut hist: Vec<(usize, Probe)>, p: &mut dyn Prober) -> Confirm {
    let mut tries = 0;
    while tally(&hist, parent).0 == 0 {
        if p.out_of_time() {
            return Confirm::Late;
        }
        if tries == 2 {
            return Confirm::No;
        }
        hist.push((parent, p.probe(parent)));
        tries += 1;
    }
    tries = 0;
    loop {
        let (pass, fail) = tally(&hist, c);
        if pass > 0 {
            return Confirm::No;
        }
        if fail >= 2 {
            return Confirm::Yes;
        }
        if p.out_of_time() {
            return Confirm::Late;
        }
        // Runs that could not be judged do not count; a few of them mean the culprit cannot be checked twice.
        if tries == 4 {
            return Confirm::No;
        }
        hist.push((c, p.probe(c)));
        tries += 1;
    }
}

/// The cells that are part of the answer: (state numbers) first to last with at least 1% each, and the chance of the best one.
fn answer(post: &[f64], good: usize) -> (usize, usize, f64) {
    let at = |i: usize| good + 1 + i;
    let lo = post.iter().position(|w| *w >= IN_ANSWER).unwrap_or(0);
    let hi = post.iter().rposition(|w| *w >= IN_ANSWER).unwrap_or(post.len() - 1);
    (at(lo), at(hi), top(post))
}

// ---------------------------------------------------------------- the versions (states) and the steps between them (cells)

/// One distinct saved version of the folder. Neighbouring save points with the same tree are one state.
#[derive(Clone, Debug)]
struct State {
    tree: [u8; 32],
    /// First and last visible save point with this tree; 0 for the folder as it is now when no save point has it.
    first: i64,
    last: i64,
    /// The turn numbers of the first and of the last point that belong to a turn.
    n_first: Option<i64>,
    n_last: Option<i64>,
}

/// The step from the state before to this one: a turn, or a change nobody's turn made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cell {
    Turn(i64),
    Gap(Option<i64>, Option<i64>),
}

/// Cell `i` (i >= 1) = the step from state `i - 1` to state `i`. `open` = the turn that has not ended.
fn cell(states: &[State], i: usize, open: Option<i64>) -> Cell {
    let (a, b) = (&states[i - 1], &states[i]);
    match (a.n_last, b.n_first) {
        (Some(x), Some(y)) if x == y => Cell::Turn(x),
        // The folder as it is now, while the turn that saved the version before it is still running.
        (Some(x), None) if b.first == 0 && open == Some(x) => Cell::Turn(x),
        (x, y) => Cell::Gap(x, y),
    }
}

fn gap_text(a: Option<i64>, b: Option<i64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) => format!("between turns {a} and {b}"),
        (Some(a), None) => format!("after turn {a}"),
        (None, Some(b)) => format!("before turn {b}"),
        (None, None) => "between two save points".into(),
    }
}

/// The lowest and highest turn number named by cells `lo..=hi`.
fn turn_span(states: &[State], open: Option<i64>, lo: usize, hi: usize) -> Option<(i64, i64)> {
    let mut ns: Vec<i64> = Vec::new();
    for i in lo..=hi {
        match cell(states, i, open) {
            Cell::Turn(n) => ns.push(n),
            Cell::Gap(a, b) => ns.extend(a.into_iter().chain(b)),
        }
    }
    Some((*ns.iter().min()?, *ns.iter().max()?))
}

/// All cells `lo..=hi` are steps of one and the same turn.
fn one_turn(states: &[State], open: Option<i64>, lo: usize, hi: usize) -> bool {
    let first = cell(states, lo, open);
    matches!(first, Cell::Turn(_)) && (lo..=hi).all(|i| cell(states, i, open) == first)
}

fn state_name(db: &Connection, s: &State) -> String {
    if s.last == 0 { "the folder as it is now".into() } else { promise::point_name(db, s.last) }
}

/// The distinct visible trees in time order (spec 19), then the folder as it is now when no save point has that tree. The live tree is
/// stored as blobs and a tree only, with no save point.
fn load_states(ws: &mut Ws) -> Result<Vec<State>> {
    let live = scratch::snapshot(ws, Duration::from_secs(30))?;
    let mut v: Vec<State> = Vec::new();
    {
        let mut st = ws.conn.prepare("SELECT p.id,p.tree_id,t.n FROM points p LEFT JOIN turns t ON t.id=p.turn_id WHERE p.visible=1 AND p.dropped=0 ORDER BY p.id")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Option<i64>>(2)?)))?;
        for row in rows {
            let (id, tree, n) = row?;
            let tree: [u8; 32] = tree.try_into().map_err(|_| Error::msg("a save point has a damaged tree id"))?;
            match v.last_mut() {
                Some(s) if s.tree == tree => {
                    s.last = id;
                    s.n_last = n.or(s.n_last);
                    s.n_first = s.n_first.or(n);
                }
                _ => v.push(State { tree, first: id, last: id, n_first: n, n_last: n }),
            }
        }
    }
    if v.last().is_none_or(|s| s.tree != live) {
        v.push(State { tree: live, first: 0, last: 0, n_first: None, n_last: None });
    }
    Ok(v)
}

/// The turn that is still running, when it is the newest one.
fn open_turn(db: &Connection) -> Option<i64> {
    db.query_row("SELECT n FROM turns WHERE end_kind='open' AND n=(SELECT MAX(n) FROM turns)", [], |r| r.get(0)).optional().ok().flatten()
}

/// The state a save point belongs to.
fn state_of(db: &Connection, states: &[State], point: i64) -> Option<usize> {
    let tree = promise::point_tree(db, point).ok()?;
    states.iter().position(|s| s.tree == tree && s.first > 0 && s.first <= point && point <= s.last).or_else(|| states.iter().rposition(|s| s.tree == tree))
}

/// Files (not folders, links' targets or secrets) that differ between two trees, in name order.
fn changed_files(ws: &Ws, a: &[u8; 32], b: &[u8; 32]) -> Vec<String> {
    changed_files_opt(ws, a, b).unwrap_or_default()
}

/// Same, but None when a tree cannot be read (the gate treats that as "unknown", not as "nothing changed").
pub fn changed_files_opt(ws: &Ws, a: &[u8; 32], b: &[u8; 32]) -> Option<Vec<String>> {
    let (Ok(x), Ok(y)) = (capture::flatten_tree(ws, &ws.conn, a), capture::flatten_tree(ws, &ws.conn, b)) else { return None };
    let plain = |v: &[(String, TreeEntry)]| -> BTreeMap<String, TreeEntry> { v.iter().filter(|(_, e)| e.kind == 'f' || e.kind == 'l').map(|(p, e)| (p.clone(), e.clone())).collect() };
    let (x, y) = (plain(&x), plain(&y));
    let names: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
    Some(
        names
            .into_iter()
            .filter(|n| match (x.get(*n), y.get(*n)) {
                (Some(p), Some(q)) => p.kind != q.kind || p.id != q.id || p.mode != q.mode,
                _ => true,
            })
            .cloned()
            .collect(),
    )
}

// ---------------------------------------------------------------- the search on real versions

fn probe_of(r: &RunResult) -> Probe {
    match (r.judged.verdict, r.judged.failed_step) {
        (Verdict::Pass, _) => Probe::Pass,
        // A FAIL is always an assertion failing; anything else could not be judged.
        (Verdict::Fail, Some(_)) => Probe::Fail,
        _ => Probe::Skip,
    }
}

/// Runs the check on saved versions, one run per probe, in fresh scratch copies.
struct Env<'a> {
    ws: &'a Ws,
    spec: &'a Spec,
    states: &'a [State],
    say: &'a dyn Fn(&str),
    until: Instant,
    /// Every run so far: (state, result).
    runs: Vec<(usize, RunResult)>,
}

impl Prober for Env<'_> {
    fn probe(&mut self, s: usize) -> Probe {
        let r = scratch::run_once(self.ws, self.spec, &self.states[s].tree, Some(self.spec.assertions()));
        let p = probe_of(&r);
        (self.say)(&format!("  run {}: {}: {} ({})", self.runs.len() + 1, state_name(&self.ws.conn, &self.states[s]), r.judged.verdict.as_str(), promise::secs(r.ms)));
        self.runs.push((s, r));
        p
    }
    fn out_of_time(&self) -> bool {
        Instant::now() >= self.until
    }
}

impl Env<'_> {
    fn hist(&self) -> Vec<(usize, Probe)> {
        self.runs.iter().map(|(s, r)| (*s, probe_of(r))).collect()
    }
    /// Runs on state `s` that could be judged: (all, failed).
    fn judged_on(&self, s: usize) -> (usize, usize) {
        let on = || self.runs.iter().filter(|(x, r)| *x == s && probe_of(r) != Probe::Skip);
        (on().count(), on().filter(|(_, r)| probe_of(r) == Probe::Fail).count())
    }
}

#[derive(Debug)]
enum Outcome {
    Works,
    /// Failed this many of the 3 runs on the folder as it is now.
    Mixed(usize),
    CannotRun(String),
    NoGood,
    Late,
    /// `parent` passes and `culprit` fails; `p` = the chance it is in that step; `sure` = confirmed (not out of time).
    One { parent: usize, culprit: usize, p: f64, sure: bool },
    /// The break is somewhere in cells `lo..=hi`; `why` says what stopped the search from narrowing it.
    Several { lo: usize, hi: usize, p: f64, why: String },
    NoCulprit,
}

struct Found {
    states: Vec<State>,
    runs: Vec<(usize, RunResult)>,
    out: Outcome,
    open: Option<i64>,
}

/// Spec 19 steps 1-5. `hint` = a save point believed to work (tried first); `flake` = the stored flake estimate; `proof` = top up the
/// confirmed parent to the 5 runs a promise needs.
// shortcut: assumes that once broken it stays broken; a revert in between hides the culprit, so the answer may be wrong. Upgrade: probe
// each side of the answer and say "it was broken, then fixed" when a revert shows.
fn search(ws: &mut Ws, spec: &Spec, hint: Option<i64>, flake: Option<f64>, proof: bool, say: &dyn Fn(&str)) -> Result<Found> {
    let states = load_states(ws)?;
    let open = open_turn(&ws.conn);
    let hint = hint.and_then(|p| state_of(&ws.conn, &states, p));
    let mut env = Env { ws: &*ws, spec, states: &states, say, until: Instant::now() + Duration::from_secs(promise::QUALIFY_SECS), runs: Vec::new() };
    let out = decide(&mut env, open, hint, flake, proof);
    let runs = env.runs;
    Ok(Found { states, runs, out, open })
}

fn decide(env: &mut Env, open: Option<i64>, hint: Option<usize>, flake: Option<f64>, proof: bool) -> Outcome {
    let tip = env.states.len() - 1;
    // 1. The folder as it is now: 3 runs.
    let mut fails = 0;
    for _ in 0..3 {
        match env.probe(tip) {
            Probe::Fail => fails += 1,
            Probe::Pass => {}
            Probe::Skip => return Outcome::CannotRun(env.runs.last().map(|r| r.1.judged.reason.clone()).unwrap_or_default()),
        }
    }
    match fails {
        0 => return Outcome::Works,
        3 => {}
        k => return Outcome::Mixed(k),
    }
    if tip == 0 {
        return Outcome::NoGood;
    }
    // 2. A version that passes: the hinted one, then 1, 2, 4, 8 ... states back from it (or from the tip).
    let base = hint.filter(|h| *h < tip);
    let mut order: Vec<usize> = base.into_iter().collect();
    let (from, mut off) = (base.unwrap_or(tip), 1usize);
    loop {
        let s = from.saturating_sub(off);
        if order.last() != Some(&s) {
            order.push(s);
        }
        if s == 0 {
            break;
        }
        off *= 2;
    }
    let (mut good, mut seen) = (None, Vec::new());
    for s in order {
        if env.out_of_time() {
            return Outcome::Late;
        }
        match env.probe(s) {
            Probe::Pass => {
                good = Some(s);
                break;
            }
            r => seen.push((s, r)),
        }
    }
    let Some(good) = good else { return Outcome::NoGood };
    // 3-4. Bayes between the working version and the folder as it is now.
    let s = bayes(good, tip, noise(flake), &seen, &mut *env);
    let (lo, hi, _) = answer(&s.post, good);
    let p: f64 = s.post[lo - good - 1..=hi - good - 1].iter().sum();
    if lo < hi && !one_turn(env.states, open, lo, hi) {
        let why = match s.skipped.iter().find(|x| **x >= lo && **x < hi) {
            Some(&x) => {
                let what = promise::good_name(&env.ws.conn, env.states[x].last);
                let reason = env.runs.iter().rev().find(|r| r.0 == x).map(|r| r.1.judged.reason.as_str()).unwrap_or("");
                if reason.starts_with("the app did not start") { format!("{what} would not start") } else { format!("{what} could not be checked") }
            }
            None => match s.stop {
                Stop::Time => "the time ran out".into(),
                Stop::RunCap => "the run limit was reached".into(),
                _ => String::new(),
            },
        };
        return Outcome::Several { lo, hi, p, why };
    }
    // 5. Confirm: the parent passes, the culprit fails twice. Cells of one turn are one culprit (the turn).
    let parent = lo - 1;
    let sure = match confirm(parent, hi, env.hist(), &mut *env) {
        Confirm::Yes => true,
        Confirm::Late => false,
        Confirm::No => return Outcome::NoCulprit,
    };
    if proof && sure {
        while env.judged_on(parent).0 < promise::GOOD_RUNS && !env.out_of_time() {
            if env.probe(parent) == Probe::Skip {
                break;
            }
        }
    }
    Outcome::One { parent, culprit: hi, p, sure }
}

// ---------------------------------------------------------------- saying it

fn pct(p: f64) -> i64 {
    ((p * 100.0).floor() as i64).min(99)
}

/// The report line (spec 19) and the exit code: 0 an answer or "works now", 1 cannot run, 4 no answer.
fn report(ws: &Ws, f: &Found, flake: Option<f64>) -> (String, i32) {
    let n = f.runs.len();
    let runs = if n == 1 { "1 run".to_string() } else { format!("{n} runs") };
    match &f.out {
        Outcome::Works => ("It works now.".into(), 0),
        Outcome::Mixed(k) => (
            match flake {
                Some(h) if h > 0.0 => format!("Flaky, no regression: it failed {k} of 3 runs now and was already flaky at creation."),
                Some(_) => format!("It fails sometimes, possibly new ({k} of 3 runs failed; 0 of 5 at creation)."),
                None => format!("It fails sometimes ({k} of 3 runs failed), so Torpor cannot search with it. Make the check steadier first."),
            },
            4,
        ),
        Outcome::CannotRun(why) => (format!("The check could not run on the folder as it is now: {why}"), 1),
        Outcome::NoGood => ("No known working version in the kept history (30 days).".into(), 4),
        Outcome::Late => (format!("Ran out of time ({} minutes) before the search was done.", promise::QUALIFY_SECS / 60), 4),
        Outcome::NoCulprit => ("No culprit found, probably flaky.".into(), 4),
        Outcome::Several { lo, hi, why, .. } => {
            let head = match turn_span(&f.states, f.open, *lo, *hi) {
                Some((a, b)) if a != b => format!("One of turns {a}-{b} broke it"),
                Some((a, _)) => format!("One of the changes around turn {a} broke it"),
                None => "One of the changes between two save points broke it".into(),
            };
            (if why.is_empty() { head } else { format!("{head} ({why})") }, 4)
        }
        Outcome::One { parent, culprit, p, sure } => {
            let mut s = match cell(&f.states, *culprit, f.open) {
                Cell::Turn(t) => format!("Turn {t} broke it ({}%, {runs}). Undo only turn {t}: torpor undo {t}", pct(*p)),
                Cell::Gap(a, b) => {
                    let files = changed_files(ws, &f.states[*parent].tree, &f.states[*culprit].tree);
                    let shown = match files.len() {
                        0 => String::new(),
                        1..=3 => format!(" ({})", files.join(", ")),
                        k => format!(" ({} and {} more)", files[..3].join(", "), k - 3),
                    };
                    format!("It broke {}, outside the chat{shown}. To go back before it: torpor back --id {}", gap_text(a, b), f.states[*parent].last)
                }
            };
            if !sure {
                s.push_str(" (not confirmed: the time ran out)");
            }
            (s, 0)
        }
    }
}

/// The `bisects` row's result word, culprit turn and chance.
fn summary(f: &Found) -> (&'static str, Option<i64>, Option<f64>) {
    match &f.out {
        Outcome::Works => ("works", None, None),
        Outcome::Mixed(_) => ("mixed", None, None),
        Outcome::CannotRun(_) => ("cannot_run", None, None),
        Outcome::NoGood => ("no_good", None, None),
        Outcome::Late => ("late", None, None),
        Outcome::NoCulprit => ("no_culprit", None, None),
        Outcome::Several { p, .. } => ("interval", None, Some(*p)),
        Outcome::One { culprit, p, sure, .. } => {
            let turn = match cell(&f.states, *culprit, f.open) {
                Cell::Turn(n) => Some(n),
                Cell::Gap(..) => None,
            };
            (if *sure { "culprit" } else { "culprit_unconfirmed" }, turn, Some(*p))
        }
    }
}

/// Keep the runs and the search for `promise promise show` and the tests: one `promise_runs` row per run, one `bisects` row. `proof`
/// = the parent and culprit states whose runs count as the promise's creation runs.
fn store(ws: &mut Ws, f: &Found, promise_id: Option<i64>, spec_id: &str, proof: Option<(usize, usize)>) -> Result<()> {
    let at = now_ns();
    let (result, turn, prob) = summary(f);
    let states_json = serde_json::to_string(&f.states.iter().map(|s| [s.first, s.last]).collect::<Vec<_>>())?;
    let probes_json = serde_json::to_string(&f.runs.iter().map(|(s, r)| serde_json::json!([s, r.judged.verdict.as_str(), r.ms])).collect::<Vec<_>>())?;
    promise::write(ws, |w| {
        for (s, r) in &f.runs {
            let kind = if proof.is_some_and(|(a, b)| *s == a || *s == b) { "create" } else { "bisect" };
            promise::record(&w.conn, promise_id, kind, Some(&f.states[*s].tree), r, at)?;
        }
        w.conn.execute(
            "INSERT INTO bisects(promise_id,spec_id,states_json,probes_json,result,culprit,prob,runs,at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![promise_id, spec_id, states_json, probes_json, result, turn, prob, f.runs.len() as i64, at],
        )?;
        Ok(())
    })
}

// ---------------------------------------------------------------- a repro check becomes a promise (spec 19, REV 5)

/// Why a search did not give the single confirmed turn a repro promise needs.
fn why_not(db: &Connection, f: &Found) -> String {
    match &f.out {
        Outcome::Works => "the check passes on the folder as it is now, so there is nothing to reproduce.".into(),
        Outcome::Mixed(k) => format!("it fails only {k} of 3 runs on the folder as it is now."),
        Outcome::CannotRun(r) => format!("the check could not run on the folder as it is now: {r}"),
        Outcome::NoGood => "no earlier version in the kept history passes it.".into(),
        Outcome::Late => "the search ran out of time before it found a version that passes.".into(),
        Outcome::NoCulprit => "no single turn could be shown to break it, probably flaky.".into(),
        Outcome::Several { lo, hi, .. } => match turn_span(&f.states, f.open, *lo, *hi) {
            Some((a, b)) if a != b => format!("Torpor could not tell which of turns {a}-{b} broke it."),
            Some((a, _)) => format!("Torpor could not tell which change around turn {a} broke it."),
            None => "Torpor could not tell which change broke it.".into(),
        },
        Outcome::One { culprit, .. } => format!("the search ran out of time before it could confirm {}.", state_name(db, &f.states[*culprit])),
    }
}

/// Spec 19: "A repro check used here becomes a promise (source repro) whose good point is the confirmed parent." Needs a confirmed single
/// culprit with a save point, and 5 judged runs on the parent; otherwise it is not saved and says why.
fn complete(ws: &mut Ws, id: i64, f: &Found, say: &dyn Fn(&str)) -> Result<Done> {
    let Outcome::One { parent, culprit, sure: true, .. } = f.out else {
        let why = why_not(&ws.conn, f);
        return promise::not_saved(ws, id, why);
    };
    let (c, g) = (&f.states[culprit], &f.states[parent]);
    if c.first == 0 {
        return promise::not_saved(ws, id, "the break came in work that has no save point yet. Run this again after the turn ends.".into());
    }
    let judged = f.runs.iter().filter(|(s, r)| *s == parent && probe_of(r) != Probe::Skip).count();
    if judged < promise::GOOD_RUNS {
        let why = format!("the checks on {} did not finish.", state_name(&ws.conn, g));
        return promise::not_saved(ws, id, why);
    }
    let fails = f.runs.iter().filter(|(s, r)| *s == parent && probe_of(r) == Probe::Fail).count();
    let state = if fails > 0 { "flaky" } else { "live" };
    let hat = fails as f64 / judged as f64;
    let cold_ms = f.runs.iter().find(|(s, r)| *s == parent && probe_of(r) != Probe::Skip).map_or(0, |(_, r)| r.ms);
    let pr = promise::Proof { state, hat, good: g.last, bad: Some(c.first), runs: judged, fails, cold_ms };
    Ok(Done::Saved(promise::finish(ws, id, &pr, Instant::now() + Duration::from_secs(promise::QUALIFY_SECS), say)?))
}

struct Proved {
    line: String,
    code: i32,
    done: Done,
}

/// The whole of `promise add --repro`: look for the culprit with promise `id`'s check, say what was found, and complete the promise.
fn prove_full(ws: &mut Ws, id: i64, say: &dyn Fn(&str)) -> Result<Proved> {
    let p = promise::get(&ws.conn, id)?.ok_or_else(|| Error::NotFound(format!("promise {id}")))?;
    let l = promise::spec_of(ws, &p)?;
    say(&format!("Looking for the turn that broke promise #{id} \"{}\".", promise::clean(&p.name, 60)));
    let hint = lite_good(&ws.conn).map(|g| g.point);
    let f = search(ws, &l.spec, hint, None, true, say)?;
    let (line, code) = report(ws, &f, None);
    let proof = match f.out {
        Outcome::One { parent, culprit, sure: true, .. } => Some((parent, culprit)),
        _ => None,
    };
    store(ws, &f, Some(id), &p.spec_id, proof)?;
    let done = complete(ws, id, &f, say)?;
    Ok(Proved { line, code, done })
}

/// `promise add --repro` (foreground and `--background`): the same as `find-break --check -`, ending in a saved promise or a reason.
pub fn prove(ws: &mut Ws, id: i64, say: &dyn Fn(&str)) -> Result<Done> {
    let r = prove_full(ws, id, say)?;
    say(&r.line);
    Ok(r.done)
}

// ---------------------------------------------------------------- lite: no check

const WINDOW_TURNS: i64 = 50;

/// G = the newest good point within 50 turns: a keep point, a promise's good point, or a promise's last PASS point (spec 19).
struct Good {
    point: i64,
    what: String,
    turn: i64,
}

fn lite_good(db: &Connection) -> Option<Good> {
    let newest: i64 = db.query_row("SELECT COALESCE(MAX(n),0) FROM turns", [], |r| r.get(0)).ok()?;
    let mut cands: Vec<(i64, String)> = Vec::new();
    {
        let mut st = db.prepare("SELECT id FROM points WHERE kind='keep' AND dropped=0 ORDER BY id DESC LIMIT 20").ok()?;
        let ids = st.query_map([], |r| r.get::<_, i64>(0)).ok()?;
        cands.extend(ids.flatten().map(|id| (id, "your last 'works'".to_string())));
    }
    for p in promise::all(db, false).ok()? {
        // "last passed" first: on equal ids the first candidate wins, and it is the more exact description.
        cands.extend(p.last_pass_point.map(|g| (g, format!("promise #{} last passed", p.id))));
        cands.extend(p.good_point.map(|g| (g, format!("promise #{}'s working version", p.id))));
    }
    let mut best: Option<Good> = None;
    for (point, what) in cands {
        if best.as_ref().is_some_and(|b| b.point >= point) {
            continue;
        }
        let row = db.query_row(
            "SELECT p.kind, COALESCE(t.n,(SELECT MAX(x.n) FROM turns x WHERE x.start_point<=p.id),0) FROM points p LEFT JOIN turns t ON t.id=p.turn_id WHERE p.id=?1 AND p.dropped=0",
            [point],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        );
        let Ok((kind, n)) = row else { continue };
        // The start of turn n is the folder as turn n - 1 left it.
        let turn = if kind == "start" { (n - 1).max(0) } else { n };
        if newest - turn <= WINDOW_TURNS && promise::point_tree(db, point).is_ok() {
            best = Some(Good { point, what, turn });
        }
    }
    best
}

/// The turn of the newest recorded change to `path` after save point `after`.
fn file_turn(db: &Connection, path: &str, after: i64) -> Option<i64> {
    db.query_row(
        "SELECT t.n FROM changes c JOIN points p ON p.id=c.point_id LEFT JOIN turns t ON t.id=COALESCE(c.turn_id,p.turn_id) WHERE c.path=?1 AND c.point_id>?2 AND t.n IS NOT NULL ORDER BY c.point_id DESC LIMIT 1",
        params![path, after],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

fn say(s: &str) {
    out(&format!("{s}\n"));
}

const LITE_MAX_NAMES: usize = 5;

fn lite() -> i32 {
    let mut ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    let Some(g) = lite_good(&ws.conn) else {
        say("Torpor has no confirmed working point in the last 50 turns. `torpor log` lists them.");
        return 0;
    };
    let tip = match scratch::snapshot(&mut ws, Duration::from_secs(30)) {
        Ok(t) => t,
        Err(e) => return fail_p(&e),
    };
    let files = match promise::point_tree(&ws.conn, g.point) {
        Ok(gt) => changed_files(&ws, &gt, &tip),
        Err(e) => return fail_p(&e),
    };
    if files.is_empty() {
        say(&format!("Torpor: the files are the same as at {} (turn {}), so the cause is outside the project folder.", g.what, g.turn));
        return 0;
    }
    let mut named: Vec<(Option<i64>, String)> = files.iter().map(|f| (file_turn(&ws.conn, f, g.point), f.clone())).collect();
    // Oldest change first; files whose turn is unknown last.
    named.sort_by_key(|(t, f)| (t.unwrap_or(i64::MAX), f.clone()));
    let list: Vec<String> = named.iter().take(LITE_MAX_NAMES).map(|(t, f)| t.map_or(f.clone(), |t| format!("{f} ({t})"))).collect();
    let mut text = list.join(", ");
    if named.len() > LITE_MAX_NAMES {
        text = format!("{text} and {} more", named.len() - LITE_MAX_NAMES);
    }
    say(&format!("Torpor: since {} (turn {}) these files changed: {text}. `torpor find-break --check -` can test which turn broke it", g.what, g.turn));
    0
}

// ---------------------------------------------------------------- the command

const USAGE: &str = "find-break [--check - | --promise ID]";

pub fn find_break(args: &[&str]) -> i32 {
    match args {
        [] => lite(),
        ["--check", "-"] => with_new_check(),
        ["--promise", id] => match num(id) {
            Some(id) => with_promise(id),
            None => usage(USAGE),
        },
        _ => usage(USAGE),
    }
}

/// A search with a promise's own check. Its stored flake estimate decides whether it can be searched with at all.
fn search_with(ws: &mut Ws, p: &promise::Promise) -> i32 {
    if let Some(msg) = p.flake_hat.and_then(too_flaky) {
        err(&format!("{msg}\n"));
        return 3;
    }
    let l = match promise::spec_of(ws, p) {
        Ok(l) => l,
        Err(e) => return fail_p(&e),
    };
    say(&format!("Looking for the turn that broke promise #{} \"{}\".", p.id, promise::clean(&p.name, 60)));
    // The version it last passed on is the likeliest working one.
    let hint = p.last_pass_point.or(p.good_point);
    let f = match search(ws, &l.spec, hint, p.flake_hat, false, &say) {
        Ok(f) => f,
        Err(e) => return fail_p(&e),
    };
    let (line, code) = report(ws, &f, p.flake_hat);
    if let Err(e) = store(ws, &f, Some(p.id), &p.spec_id, None) {
        return fail_p(&e);
    }
    say(&line);
    code
}

fn with_promise(id: i64) -> i32 {
    let mut ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    match get_live(&ws.conn, id) {
        Ok(p) => search_with(&mut ws, &p),
        Err(c) => c,
    }
}

fn with_new_check() -> i32 {
    let l = match read_check() {
        Ok(l) => l,
        Err(c) => return c,
    };
    let mut ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    // The same check is already a promise: use that one.
    let same: Option<i64> = ws.conn.query_row("SELECT id FROM promises WHERE spec_id=?1 AND state<>'dropped' LIMIT 1", [&l.id], |r| r.get(0)).optional().unwrap_or(None);
    if let Some(id) = same {
        say(&format!("This check is promise #{id} already."));
        return match get_live(&ws.conn, id) {
            Ok(p) => search_with(&mut ws, &p),
            Err(c) => c,
        };
    }
    let full = match promise::live_count(&ws.conn) {
        Ok(n) => n >= LIVE_MAX,
        Err(e) => return fail_p(&e),
    };
    if full {
        // Search anyway; the check just cannot be kept.
        let hint = lite_good(&ws.conn).map(|g| g.point);
        let f = match search(&mut ws, &l.spec, hint, None, false, &say) {
            Ok(f) => f,
            Err(e) => return fail_p(&e),
        };
        let (line, code) = report(&ws, &f, None);
        if let Err(e) = store(&mut ws, &f, None, &l.id, None) {
            return fail_p(&e);
        }
        say(&line);
        say(&format!("Not saved as a promise: Torpor keeps at most {LIVE_MAX} promises. Drop one first: torpor promise list"));
        return code;
    }
    let id = match promise_cmd::propose_repro(&mut ws, &l) {
        Ok(id) => id,
        Err(c) => return c,
    };
    match prove_full(&mut ws, id, &say) {
        Ok(r) => {
            say(&r.line);
            match r.done {
                Done::Saved(s) => {
                    let Ok(Some(p)) = promise::get(&ws.conn, id) else { return 1 };
                    say(&promise::saved_line(&ws.conn, &p, &s));
                }
                Done::NotSaved(why) => {
                    // The AI is told here; Stop repeats only what a background run found.
                    let _ = promise::write(&mut ws, |w| Ok(w.conn.execute("UPDATE promises SET announced=1 WHERE id=?1", [id])?));
                    say(&format!("Not saved as a promise: {why}"));
                }
            }
            r.code
        }
        Err(e) => {
            let _ = promise::write(&mut ws, |w| Ok(w.conn.execute("UPDATE promises SET state='dropped', announced=1 WHERE id=?1 AND tier='proposed'", [id])?));
            fail_p(&e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny fixed-seed generator: the tests must give the same numbers every time.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// States `>= culprit` are broken and always fail; the others fail at random with chance `f`. `skip` states cannot be run.
    struct Oracle {
        culprit: usize,
        f: f64,
        rng: Rng,
        skip: Vec<usize>,
        probes: Vec<usize>,
    }
    impl Prober for Oracle {
        fn probe(&mut self, s: usize) -> Probe {
            self.probes.push(s);
            if self.skip.contains(&s) {
                Probe::Skip
            } else if s >= self.culprit || self.rng.next() < self.f {
                Probe::Fail
            } else {
                Probe::Pass
            }
        }
    }

    fn oracle(culprit: usize, f: f64, rng: Rng) -> Oracle {
        Oracle { culprit, f, rng, skip: Vec::new(), probes: Vec::new() }
    }

    fn best(s: &Searched, good: usize) -> usize {
        good + 1 + s.post.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0
    }

    /// B3: 64 cells, one culprit, 10% of the good versions fail at random, 1,000 trials.
    #[test]
    fn b3_the_search_finds_the_culprit_in_about_ten_runs() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut right, mut runs) = (0, 0);
        for _ in 0..1000 {
            let culprit = 1 + (rng.next() * 64.0) as usize;
            let mut o = oracle(culprit, 0.10, Rng(rng.next().to_bits() | 1));
            let s = bayes(0, 64, noise(None), &[], &mut o);
            right += (best(&s, 0) == culprit) as usize;
            runs += s.runs;
            assert_eq!(s.runs, o.probes.len());
        }
        let (acc, mean) = (right as f64 / 10.0, runs as f64 / 1000.0);
        println!("B3: accuracy {acc:.1}% over 1000 trials, mean {mean:.2} runs (64 cells, f=10%)");
        assert!(acc >= 95.0, "accuracy {acc}%");
        assert!(mean <= 12.0, "mean runs {mean}");
    }

    #[test]
    fn a_check_that_fails_at_random_too_often_is_refused() {
        assert_eq!(too_flaky(0.35).as_deref(), Some("This check fails at random too often (35%) to search with. Fix the check first."));
        assert_eq!(too_flaky(0.30), None);
        assert_eq!(too_flaky(0.0), None);
        assert_eq!((noise(None), noise(Some(0.0)), noise(Some(0.2))), (0.10, 0.05, 0.2));
    }

    #[test]
    fn neighbouring_states_are_certain_without_a_run() {
        let s = bayes(7, 8, 0.1, &[], &mut oracle(8, 0.1, Rng(1)));
        assert_eq!((s.runs, s.stop, best(&s, 7)), (0, Stop::Certain, 8));
    }

    #[test]
    fn states_that_cannot_run_widen_the_answer_and_never_decide_it() {
        // States 4 and 5 will not start and the first broken one is 5: the answer is cell 4 or cell 5, found without ever running them.
        let mut o = oracle(5, 0.0, Rng(3));
        o.skip = vec![4, 5];
        let s = bayes(0, 8, 0.1, &[], &mut o);
        assert!(matches!(s.stop, Stop::NoProbes | Stop::TooManySkips), "{:?}", s.stop);
        assert!(s.skipped.contains(&4));
        let (lo, hi, _) = answer(&s.post, 0);
        assert!(lo <= 4 && hi >= 5, "{lo}..{hi}");
        assert!(lo >= 1 && hi <= 6, "{lo}..{hi}");
    }

    #[test]
    fn runs_taken_before_the_search_count() {
        // A FAIL at 6 and a PASS at 2 were seen while looking for a working version: the search starts inside 3..=6.
        let seen = [(6, Probe::Fail), (6, Probe::Fail), (6, Probe::Fail)];
        let mut o = oracle(4, 0.0, Rng(5));
        let s = bayes(2, 9, 0.1, &seen, &mut o);
        assert_eq!(best(&s, 2), 4);
        assert!(o.probes.iter().all(|p| *p <= 6), "{:?}", o.probes);
    }

    /// Out of time: the search stops at once and the confirmation is late, not wrong.
    struct Late;
    impl Prober for Late {
        fn probe(&mut self, _: usize) -> Probe {
            panic!("no run may start after the time is up")
        }
        fn out_of_time(&self) -> bool {
            true
        }
    }

    #[test]
    fn nothing_starts_after_the_time_is_up() {
        assert_eq!(bayes(0, 8, 0.1, &[], &mut Late).stop, Stop::Time);
        assert_eq!(confirm(4, 5, vec![(5, Probe::Fail)], &mut Late), Confirm::Late);
        assert_eq!(confirm(4, 5, vec![(4, Probe::Pass), (5, Probe::Fail)], &mut Late), Confirm::Late);
    }

    /// Step 5: no culprit when the parent never passes or the culprit passes.
    #[test]
    fn confirm_says_no_culprit_for_noise() {
        // The "culprit" passes once: it is not broken.
        let mut o = oracle(99, 0.0, Rng(7));
        assert_eq!(confirm(4, 5, vec![(4, Probe::Pass), (5, Probe::Fail), (5, Probe::Pass)], &mut o), Confirm::No);
        assert!(o.probes.is_empty());
        // The parent was never seen passing and fails on both tries.
        let mut o = oracle(2, 0.0, Rng(7));
        assert_eq!(confirm(4, 5, vec![(5, Probe::Fail), (5, Probe::Fail)], &mut o), Confirm::No);
        assert_eq!(o.probes, vec![4, 4]);
        // Everything is as it should be, and the runs already taken are reused.
        let mut o = oracle(5, 0.0, Rng(7));
        assert_eq!(confirm(4, 5, vec![(4, Probe::Pass), (5, Probe::Fail)], &mut o), Confirm::Yes);
        assert_eq!(o.probes, vec![5]);
        // Only one run on the culprit so far, and the oracle really has no culprit there.
        let mut o = oracle(99, 0.0, Rng(7));
        assert_eq!(confirm(4, 5, vec![(4, Probe::Pass), (5, Probe::Fail)], &mut o), Confirm::No);
    }
}
