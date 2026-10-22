//! Promises (spec 14, 17, 24): the rows, who ran a command, runs with repeats, qualification, disputes and the lines Stop prints.
//! No store lock and no transaction is ever held while an app runs: runs come first, rows are written after (`write`).
use crate::runner::{self, Verdict};
use crate::scratch::{self, RunResult};
use crate::spec::{self, Linted, Spec};
use crate::store::*;
use crate::turn::{self, Turn};
use crate::util::*;
use crate::{hook, restore, texts, vault};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const LIVE_MAX: i64 = 50;
/// Raw runs on the good version (no repeats): all must PASS.
pub const GOOD_RUNS: usize = 5;
/// Runs on the bad version: all must FAIL at an assertion.
pub const BAD_RUNS: usize = 2;
pub const QUALIFY_SECS: u64 = 9 * 60;
const WHO_WINDOW_NS: i64 = 10 * NS_PER_SEC;
const STALE_NS: i64 = 15 * 60 * NS_PER_SEC;
const DAY_NS: i64 = 86_400 * NS_PER_SEC;
const NOTE_MAX: usize = 400;

// ---------------------------------------------------------------- rows

#[derive(Clone, Debug)]
pub struct Promise {
    pub id: i64,
    pub name: String,
    pub why: Option<String>,
    /// repro, keep, praise, agent.
    pub source: String,
    /// proposed, advisory, unproven, shadow, blocking.
    pub tier: String,
    /// live, disputed, flaky, quarantined, dropped.
    pub state: String,
    pub spec_id: String,
    pub good_point: Option<i64>,
    pub bad_point: Option<i64>,
    /// turns.id of the turn it was made in.
    pub created_turn: Option<i64>,
    pub created_at: i64,
    /// The source a provisional `agent` promise gets if Stop finds the turn's origin human.
    pub want_source: Option<String>,
    pub announced: bool,
    pub replaces: Option<i64>,
    pub assertions_n: i64,
    pub flake_hat: Option<f64>,
    pub mutants_killed: Option<i64>,
    pub mutants_tried: Option<i64>,
    pub deps_json: Option<String>,
    pub watched_json: Option<String>,
    pub last_pass_point: Option<i64>,
    pub shadow_days: i64,
    pub catches: i64,
    pub disputes_json: Option<String>,
}

const COLS: &str = "id,name,why,source,tier,state,spec_id,good_point,bad_point,created_turn,created_at,want_source,announced,replaces,assertions_n,flake_hat,\
    mutants_killed,mutants_tried,deps_json,watched_json,last_pass_point,shadow_days,catches,disputes_json";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Promise> {
    Ok(Promise {
        id: r.get(0)?,
        name: r.get(1)?,
        why: r.get(2)?,
        source: r.get(3)?,
        tier: r.get(4)?,
        state: r.get(5)?,
        spec_id: r.get(6)?,
        good_point: r.get(7)?,
        bad_point: r.get(8)?,
        created_turn: r.get(9)?,
        created_at: r.get(10)?,
        want_source: r.get(11)?,
        announced: r.get::<_, i64>(12)? != 0,
        replaces: r.get(13)?,
        assertions_n: r.get(14)?,
        flake_hat: r.get(15)?,
        mutants_killed: r.get(16)?,
        mutants_tried: r.get(17)?,
        deps_json: r.get(18)?,
        watched_json: r.get(19)?,
        last_pass_point: r.get(20)?,
        shadow_days: r.get(21)?,
        catches: r.get(22)?,
        disputes_json: r.get(23)?,
    })
}

pub fn get(db: &Connection, id: i64) -> Result<Option<Promise>> {
    Ok(db.query_row(&format!("SELECT {COLS} FROM promises WHERE id=?1"), [id], row).optional()?)
}

/// All promises in id order; dropped ones only when asked.
pub fn all(db: &Connection, with_dropped: bool) -> Result<Vec<Promise>> {
    let mut st = db.prepare(&format!("SELECT {COLS} FROM promises {} ORDER BY id", if with_dropped { "" } else { "WHERE state<>'dropped'" }))?;
    let v = st.query_map([], row)?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(v)
}

pub fn live_count(db: &Connection) -> Result<i64> {
    Ok(db.query_row("SELECT COUNT(*) FROM promises WHERE state<>'dropped'", [], |r| r.get(0))?)
}

/// The stored check of a promise, read back through the vault (its bytes are checked against their hash).
pub fn spec_of(ws: &Ws, p: &Promise) -> Result<Linted> {
    let b = vault::get(ws, &p.spec_id)?;
    spec::load(&b).map_err(|e| Error::msg(e.join(" ")))
}

/// Text from a check or a person, safe to print and to store: no control characters, no secrets, at most `max` chars.
pub fn clean(s: &str, max: usize) -> String {
    let s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    trunc_chars(&texts::redact(&s).0, max)
}

/// Write rows: shared store lock plus one IMMEDIATE transaction, retried a few times when busy. Never wrap an app run in this.
pub fn write<T>(ws: &mut Ws, f: impl Fn(&Ws) -> Result<T>) -> Result<T> {
    let mut last = Error::Busy;
    for i in 0..4u64 {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(300 * i));
        }
        if let Err(e) = ws.lock_shared(restore::LOCK_WAIT_MS) {
            last = e;
            continue;
        }
        let r = (|| -> Result<T> {
            let tx = Transaction::new_unchecked(&ws.conn, TransactionBehavior::Immediate)?;
            let v = f(ws)?;
            tx.commit()?;
            Ok(v)
        })();
        ws.unlock();
        match r {
            Err(Error::Busy) => last = Error::Busy,
            other => return other,
        }
    }
    Err(last)
}

// ---------------------------------------------------------------- who ran it

pub struct Who {
    /// An AI ran it: its Bash call was seen by PreToolUse (a `cli_calls` row), or CLAUDECODE is set.
    pub agent: bool,
    pub turn: Option<Turn>,
}

fn newest_open_turn(db: &Connection) -> Result<Option<Turn>> {
    let id: Option<i64> = db
        .query_row(
            "SELECT t.id FROM turns t WHERE t.end_kind='open' ORDER BY (SELECT MAX(last_at) FROM heartbeat h WHERE h.session_id=t.session_id) DESC, t.id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let id = match id {
        Some(i) => Some(i),
        None => db.query_row("SELECT MAX(id) FROM turns", [], |r| r.get::<_, Option<i64>>(0))?,
    };
    match id {
        Some(i) => turn::by_id(db, i),
        None => Ok(None),
    }
}

/// Spec 12: a CLI run is "agent" when CLAUDECODE is set or a `cli_calls` row at most 10 s old names its subcommand; the row gives its turn.
/// Otherwise it is "user" and the turn is the open one with the newest heartbeat.
pub fn who(db: &Connection, sub: &str, now: i64) -> Result<Who> {
    let call: Option<(Option<String>, Option<String>)> = db
        .query_row("SELECT session_id,prompt_id FROM cli_calls WHERE subcommand=?1 AND at>=?2 ORDER BY at DESC LIMIT 1", params![sub, now - WHO_WINDOW_NS], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    if let Some((session, prompt)) = call {
        let session = session.unwrap_or_default();
        let by_prompt = match prompt.as_deref() {
            Some(p) => turn::find(db, &session, p)?,
            None => None,
        };
        let t = match by_prompt {
            Some(t) => Some(t),
            None => turn::open_of(db, &session)?,
        };
        return Ok(Who { agent: true, turn: t });
    }
    let agent = std::env::var_os("CLAUDECODE").is_some_and(|v| !v.is_empty());
    Ok(Who { agent, turn: newest_open_turn(db)? })
}

/// The origin of `turn` as far as the transcript says now. Runs the section 4.4 reader mid-turn (the offset stays where Stop left it).
pub fn origin_now(db: &Connection, t: &Turn) -> Result<String> {
    hook::read_origin_for(db, &t.session_id, None, t, false)?;
    Ok(db.query_row("SELECT origin FROM turns WHERE id=?1", [t.id], |r| r.get(0))?)
}

/// Spec 14: praise, a complaint and a keep count as the user's words only when the turn's origin is human. Returns (source, want_source):
/// an origin that cannot be read yet gives a provisional `agent` that Stop upgrades.
pub fn source_for(db: &Connection, who: &Who, want: &str) -> Result<(String, Option<String>)> {
    if !who.agent {
        return Ok((want.to_string(), None));
    }
    let Some(t) = &who.turn else { return Ok(("agent".into(), None)) };
    Ok(match origin_now(db, t)?.as_str() {
        "human" => (want.to_string(), None),
        "unknown" | "none" => ("agent".into(), Some(want.to_string())),
        _ => ("agent".into(), None),
    })
}

/// Stop has read the origin: provisional sources become what was asked for when the turn is human, and are final either way.
pub fn upgrade_sources(db: &Connection, t: &Turn) -> Result<()> {
    let origin: String = db.query_row("SELECT origin FROM turns WHERE id=?1", [t.id], |r| r.get(0))?;
    if origin == "human" {
        db.execute("UPDATE promises SET source=want_source WHERE want_source IS NOT NULL AND created_turn=?1", [t.id])?;
    }
    db.execute("UPDATE promises SET want_source=NULL WHERE want_source IS NOT NULL AND created_turn=?1", [t.id])?;
    Ok(())
}

// ---------------------------------------------------------------- save points

pub fn point_tree(db: &Connection, point: i64) -> Result<[u8; 32]> {
    let b: Option<Vec<u8>> = db.query_row("SELECT tree_id FROM points WHERE id=?1 AND dropped=0", [point], |r| r.get(0)).optional()?;
    let b = b.ok_or_else(|| Error::NotFound(format!("save point {point}")))?;
    b.try_into().map_err(|_| Error::msg("a save point has a damaged tree id"))
}

/// The newest save point whose tree is `tree` (a `check` of the live folder has none when nothing was saved since).
pub fn point_of_tree(db: &Connection, tree: &[u8; 32]) -> Option<i64> {
    db.query_row("SELECT id FROM points WHERE tree_id=?1 AND dropped=0 ORDER BY id DESC LIMIT 1", [&tree[..]], |r| r.get(0)).optional().ok().flatten()
}

fn point_info(db: &Connection, id: i64) -> Option<(String, Option<i64>)> {
    db.query_row("SELECT p.kind,t.n FROM points p LEFT JOIN turns t ON t.id=p.turn_id WHERE p.id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok().flatten()
}

/// How a save point is named in a sentence: "the start of turn 12", "the kept version (save point 61)".
pub fn point_name(db: &Connection, id: i64) -> String {
    match point_info(db, id) {
        Some((k, Some(n))) if k == "start" => format!("the start of turn {n}"),
        Some((k, Some(n))) if k == "end" => format!("the end of turn {n}"),
        Some((k, _)) if k == "keep" => format!("the kept version (save point {id})"),
        _ => format!("save point {id}"),
    }
}

pub(crate) fn good_name(db: &Connection, id: i64) -> String {
    let n = point_name(db, id);
    if n.starts_with("the start") || n.starts_with("the end") { format!("the version from {n}") } else { n }
}

/// The version the user called good (spec 14): `--good`, `--turn K`, or the START of the newest human turn, which is what the user saw
/// before they spoke. A turn that is not the user's own (origin known and not human) gives way to the newest human one.
pub fn pick_good(db: &Connection, who: &Who, point: Option<i64>, turn_k: Option<i64>) -> Result<i64> {
    if let Some(p) = point {
        point_tree(db, p)?;
        return Ok(p);
    }
    if let Some(k) = turn_k {
        let sp: Option<i64> = db.query_row("SELECT start_point FROM turns WHERE n=?1", [k], |r| r.get(0)).optional()?.flatten();
        let sp = sp.ok_or_else(|| Error::NotFound(format!("turn {k}")))?;
        point_tree(db, sp)?;
        return Ok(sp);
    }
    let cur = who.turn.as_ref();
    if let Some(t) = cur {
        if matches!(origin_now(db, t)?.as_str(), "human" | "unknown" | "none") {
            if let Some(sp) = t.start_point {
                if point_tree(db, sp).is_ok() {
                    return Ok(sp);
                }
            }
        }
    }
    let cur_id = cur.map_or(i64::MAX, |t| t.id);
    for sql in [
        "SELECT start_point FROM turns WHERE origin='human' AND start_point IS NOT NULL AND id<?1 ORDER BY id DESC LIMIT 5",
        "SELECT start_point FROM turns WHERE start_point IS NOT NULL AND id<=?1 ORDER BY id DESC LIMIT 5",
    ] {
        let mut st = db.prepare(sql)?;
        let ids = st.query_map([cur_id], |r| r.get::<_, i64>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        if let Some(sp) = ids.into_iter().find(|sp| point_tree(db, *sp).is_ok()) {
            return Ok(sp);
        }
    }
    Err(Error::NotFound("save point to call good".into()))
}

/// Earlier versions that should fail the check, newest first: the START of the good point's own turn when the good point came after
/// its work (keep, `--good`), then the STARTs of the turns 1, 2, 4 and 8 before it. Versions with the good tree are left out.
pub fn bad_candidates(db: &Connection, good: i64) -> Result<Vec<i64>> {
    let gtree = point_tree(db, good)?;
    let Some((kind, Some(n))) = point_info(db, good) else { return Ok(Vec::new()) };
    let mut ns: Vec<i64> = Vec::new();
    if kind != "start" {
        ns.push(n);
    }
    ns.extend([1i64, 2, 4, 8].iter().map(|o| n - o));
    let mut out: Vec<i64> = Vec::new();
    for k in ns {
        let sp: Option<i64> = db.query_row("SELECT start_point FROM turns WHERE n=?1", [k], |r| r.get(0)).optional()?.flatten();
        if let Some(sp) = sp {
            if sp != good && !out.contains(&sp) && point_tree(db, sp).is_ok_and(|t| t != gtree) {
                out.push(sp);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- runs

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Pass,
    /// Failed in 3 of 3 fresh runs.
    Fail,
    /// The runs disagreed.
    Flaky,
    Unrunnable,
}

impl Call {
    pub fn as_str(self) -> &'static str {
        match self {
            Call::Pass => "PASS",
            Call::Fail => "FAIL",
            Call::Flaky => "FLAKY",
            Call::Unrunnable => "UNRUNNABLE",
        }
    }
}

pub struct Checked {
    pub call: Call,
    pub failed_step: Option<usize>,
    pub reason: String,
    pub assertions: usize,
    pub ms: u64,
    /// Every raw run, in order (all of them are recorded: the same tree giving two verdicts is a flake observation).
    pub runs: Vec<RunResult>,
}

/// Under a deadline another run starts only if the longest run so far would still fit.
fn fits_another(runs: &[RunResult]) -> bool {
    runner::time_left().is_none_or(|left| left.as_millis() as u64 >= runs.iter().map(|r| r.ms).max().unwrap_or(0) * 5 / 4 + 300)
}

/// One verdict for `tree` (spec 16): a PASS stands; a FAIL is re-run in up to 2 more fresh scratch copies and is a FAIL only in 3 of 3.
pub fn check_tree(ws: &Ws, spec: &Spec, tree: &[u8; 32]) -> Checked {
    let n = Some(spec.assertions());
    let mut runs: Vec<RunResult> = Vec::new();
    let mut call = Call::Fail;
    let mut cut = false;
    for i in 0..3 {
        if i > 0 && !fits_another(&runs) {
            // Under a deadline (the Stop gate) fewer than 3 runs cannot call a FAIL: unverified, not failed.
            (call, cut) = (Call::Unrunnable, true);
            break;
        }
        let r = scratch::run_once(ws, spec, tree, n);
        let v = r.judged.verdict;
        runs.push(r);
        match v {
            Verdict::Unrunnable => {
                call = if i == 0 { Call::Unrunnable } else { Call::Flaky };
                break;
            }
            Verdict::Pass => {
                call = if i == 0 { Call::Pass } else { Call::Flaky };
                break;
            }
            Verdict::Fail => {}
        }
    }
    let pick = match call {
        Call::Flaky => runs.iter().find(|r| r.judged.verdict == Verdict::Fail).unwrap_or(&runs[0]),
        _ => &runs[runs.len() - 1],
    };
    let (failed_step, mut reason, assertions) = (pick.judged.failed_step, pick.judged.reason.clone(), pick.judged.assertions);
    if cut {
        reason = format!("{}: only {} of 3 runs fit", runner::OUT_OF_TIME, runs.len());
    }
    let ms = runs.iter().map(|r| r.ms).sum();
    Checked { call, failed_step, reason, assertions, ms, runs }
}

/// One `promise_runs` row (`promise` None: a search with a check that was not saved). Kinds follow the spec: create, gate, control, check, bisect, undo, all.
pub fn record(db: &Connection, promise: impl Into<Option<i64>>, kind: &str, tree: Option<&[u8; 32]>, r: &RunResult, now: i64) -> Result<()> {
    let j = &r.judged;
    let reason = (!j.reason.is_empty()).then(|| clean(&j.reason, 300));
    db.execute(
        "INSERT INTO promise_runs(promise_id,tree_id,kind,verdict,failed_step,reason,assertions_n,token_ok,ms,at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![promise.into(), tree.map(|t| t.to_vec()), kind, j.verdict.as_str(), j.failed_step.map(|x| x as i64), reason, j.assertions as i64, j.token_ok as i64, r.ms as i64, now],
    )?;
    Ok(())
}

/// Raw runs on one version, stopping at an UNRUNNABLE or the deadline. `say` gets one line per run.
fn raw_runs(ws: &Ws, spec: &Spec, tree: &[u8; 32], n: usize, until: Instant, say: &dyn Fn(&str)) -> Vec<RunResult> {
    let mut v: Vec<RunResult> = Vec::new();
    for i in 1..=n {
        if Instant::now() >= until {
            break;
        }
        let r = scratch::run_once(ws, spec, tree, Some(spec.assertions()));
        say(&format!("  run {i} of {n}: {} ({})", r.judged.verdict.as_str(), secs(r.ms)));
        let stop = r.judged.verdict == Verdict::Unrunnable;
        v.push(r);
        if stop {
            break;
        }
    }
    v
}

pub fn secs(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

fn fails_at_assertion(r: &RunResult) -> bool {
    r.judged.verdict == Verdict::Fail && r.judged.failed_step.is_some()
}

// ---------------------------------------------------------------- qualification

pub struct Saved {
    pub tier: &'static str,
    pub state: &'static str,
    pub good_runs: usize,
    pub good_fails: usize,
    /// The version that fails the check, if one was found.
    pub bad_point: Option<i64>,
}

pub enum Done {
    Saved(Saved),
    NotSaved(String),
}

pub fn not_saved(ws: &mut Ws, id: i64, why: String) -> Result<Done> {
    let at = now_ns();
    write(ws, |w| {
        w.conn.execute("UPDATE promises SET state='dropped', announced=0 WHERE id=?1", [id])?;
        w.conn.execute(
            "INSERT INTO promise_runs(promise_id,tree_id,kind,verdict,failed_step,reason,assertions_n,token_ok,ms,at) VALUES(?1,NULL,'create','UNRUNNABLE',NULL,?2,0,0,0,?3)",
            params![id, clean(&why, 300), at],
        )?;
        Ok(())
    })?;
    Ok(Done::NotSaved(why))
}

/// Spec 17, F2P: 5 raw runs on the good version must all PASS; then an earlier version must FAIL at an assertion in 2 of 2 runs
/// (the first of `bad_candidates`, or the one given with `--bad`). Proof found = advisory; none = unproven (a guard only).
/// Failures in 1 to 4 of the 5 = flaky: kept, advisory, and never promoted. Nothing is held across the runs.
pub fn qualify(ws: &mut Ws, id: i64, say: &dyn Fn(&str)) -> Result<Done> {
    let until = Instant::now() + Duration::from_secs(QUALIFY_SECS);
    let p = get(&ws.conn, id)?.ok_or_else(|| Error::NotFound(format!("promise {id}")))?;
    let l = spec_of(ws, &p)?;
    let Some(good) = p.good_point else { return not_saved(ws, id, "there is no version to call good.".into()) };
    let gtree = match point_tree(&ws.conn, good) {
        Ok(t) => t,
        Err(_) => return not_saved(ws, id, format!("save point {good} is gone.")),
    };
    let gname = good_name(&ws.conn, good);
    say(&format!("Checking promise #{id} \"{}\" on {gname}: {GOOD_RUNS} runs.", clean(&p.name, 60)));
    let good_runs = raw_runs(ws, &l.spec, &gtree, GOOD_RUNS, until, say);
    let at = now_ns();
    write(ws, |w| good_runs.iter().try_for_each(|r| record(&w.conn, id, "create", Some(&gtree), r, at)))?;
    if let Some(u) = good_runs.iter().find(|r| r.judged.verdict == Verdict::Unrunnable) {
        return not_saved(ws, id, format!("the check could not run on {gname}: {}", u.judged.reason));
    }
    if good_runs.len() < GOOD_RUNS {
        return not_saved(ws, id, format!("the checks took longer than {} minutes.", QUALIFY_SECS / 60));
    }
    let fails = good_runs.iter().filter(|r| r.judged.verdict == Verdict::Fail).count();
    if fails == GOOD_RUNS {
        let last = &good_runs[GOOD_RUNS - 1].judged;
        return not_saved(ws, id, format!("it fails on {gname} in {GOOD_RUNS} of {GOOD_RUNS} runs ({}).", last.reason));
    }
    if fails > 0 {
        let flake = fails as f64 / GOOD_RUNS as f64;
        write(ws, |w| {
            w.conn.execute(
                "UPDATE promises SET tier='advisory', state='flaky', bad_point=NULL, flake_hat=?2, last_pass_point=?3 WHERE id=?1",
                params![id, flake, good],
            )?;
            Ok(())
        })?;
        return Ok(Done::Saved(Saved { tier: "advisory", state: "flaky", good_runs: GOOD_RUNS, good_fails: fails, bad_point: None }));
    }
    // Good holds 5 of 5. Does an earlier version fail it?
    let candidates = match p.bad_point {
        Some(b) => vec![b],
        None => bad_candidates(&ws.conn, good)?,
    };
    let mut bad: Option<i64> = None;
    for c in candidates {
        if Instant::now() >= until {
            break;
        }
        let Ok(btree) = point_tree(&ws.conn, c) else { continue };
        say(&format!("Looking for an earlier version that fails it: {}.", point_name(&ws.conn, c)));
        let rs = raw_runs(ws, &l.spec, &btree, BAD_RUNS, until, say);
        let at = now_ns();
        write(ws, |w| rs.iter().try_for_each(|r| record(&w.conn, id, "create", Some(&btree), r, at)))?;
        if rs.len() == BAD_RUNS && rs.iter().all(fails_at_assertion) {
            bad = Some(c);
            break;
        }
    }
    let tier = if bad.is_some() { "advisory" } else { "unproven" };
    // The files the bad-to-good change touched are what this promise watches (spec 18); docs and tests do not count, lock files and settings do.
    let watched: Option<String> = bad
        .and_then(|b| point_tree(&ws.conn, b).ok())
        .and_then(|bt| crate::findbreak::changed_files_opt(ws, &bt, &gtree))
        .map(|v| v.into_iter().filter(|p| crate::gate::is_global(p) || !crate::gate::doc_like(p)).collect::<Vec<_>>())
        .and_then(|v| serde_json::to_string(&v).ok());
    write(ws, |w| {
        w.conn.execute(
            "UPDATE promises SET tier=?2, state='live', bad_point=?3, flake_hat=0, last_pass_point=?4, watched_json=?5 WHERE id=?1",
            params![id, tier, bad, good, watched],
        )?;
        Ok(())
    })?;
    Ok(Done::Saved(Saved { tier, state: "live", good_runs: GOOD_RUNS, good_fails: 0, bad_point: bad }))
}

/// The foreground answer to `promise add` once qualification is done (spec 24).
pub fn saved_line(db: &Connection, p: &Promise, s: &Saved) -> String {
    let name = clean(&p.name, 60);
    let good = p.good_point.map(|g| good_name(db, g)).unwrap_or_default();
    match (s.state, s.tier) {
        ("flaky", _) => format!(
            "Promise #{} \"{name}\" saved but flaky: it failed {} of {} runs on {good}. It stays advisory and can never stop the AI.",
            p.id, s.good_fails, s.good_runs
        ),
        (_, "unproven") => format!("Promise #{} \"{name}\" saved as a guard only: no earlier version fails it, so Torpor can't prove it tests the new work.", p.id),
        _ => {
            let bad = s.bad_point.map(|b| point_name(db, b)).unwrap_or_default();
            format!(
                "Promise #{} \"{name}\" saved. Proof: passed {} of {} on {good} and failed {BAD_RUNS} of {BAD_RUNS} on {bad}. Advisory for now: Torpor shows failures but does not stop the AI.",
                p.id, s.good_runs, s.good_runs
            )
        }
    }
}

// ---------------------------------------------------------------- lines for people

/// "reason (hint)": the hint goes inside a closing bracket when there is one.
pub fn with_hint(reason: &str, hint: &str) -> String {
    match reason.strip_suffix(')') {
        Some(head) => format!("{head}; {hint})"),
        None => format!("{reason} ({hint})"),
    }
}

pub fn check_line(p: &Promise, spec: &Spec, c: &Checked) -> String {
    let name = clean(&p.name, 60);
    match c.call {
        Call::Pass => format!("Promise #{} {name}: PASS ({} checks, {}).", p.id, c.assertions, secs(c.ms)),
        Call::Fail => {
            let step = c.failed_step.unwrap_or(0);
            let why = step.checked_sub(1).and_then(|i| spec.steps.get(i)).and_then(|s| s.why.clone()).unwrap_or_else(|| "an assertion failed".into());
            format!("Promise #{} {name}: FAIL at step {step}: {} ({}).", p.id, clean(&why, 160), clean(c.reason.trim_end_matches('.'), 200))
        }
        Call::Flaky => format!("Promise #{} {name}: FLAKY: it failed once and passed on a re-run, so the result can't be trusted ({}).", p.id, secs(c.ms)),
        Call::Unrunnable => format!("Promise #{} {name}: UNRUNNABLE: {}", p.id, with_hint(&clean(&c.reason, 250), &format!("torpor promise show {}", p.id))),
    }
}

pub fn tier_label(p: &Promise) -> &str {
    if p.tier == "unproven" { "guard only" } else { p.tier.as_str() }
}

pub fn turn_n(db: &Connection, turn_id: Option<i64>) -> Option<i64> {
    db.query_row("SELECT n FROM turns WHERE id=?1", [turn_id?], |r| r.get(0)).optional().ok().flatten()
}

/// "your words, turn 12" / "the AI, turn 12".
pub fn source_phrase(db: &Connection, p: &Promise) -> String {
    let who = if p.source == "agent" { "the AI" } else { "your words" };
    match turn_n(db, p.created_turn) {
        Some(n) => format!("{who}, turn {n}"),
        None => who.to_string(),
    }
}

fn when(ns: i64, now: i64) -> String {
    local_stamp(ns, now).trim_start_matches("today ").to_string()
}

/// The latest run of the kinds a person cares about: (verdict, at).
pub fn last_run(db: &Connection, id: i64) -> Option<(String, i64)> {
    // The runs on the earlier version are the proof, not the promise's state: of the creation runs only the good version's count.
    db.query_row(
        "SELECT verdict,at FROM promise_runs r WHERE promise_id=?1 AND kind IN ('create','check','gate','control') \
         AND (kind<>'create' OR tree_id IS (SELECT p.tree_id FROM promises m JOIN points p ON p.id=m.good_point WHERE m.id=r.promise_id)) ORDER BY id DESC LIMIT 1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .ok()
    .flatten()
}

pub fn list_line(db: &Connection, p: &Promise, now: i64) -> String {
    let last = match last_run(db, p.id) {
        Some((v, at)) if v == "PASS" => format!("passed {}", when(at, now)),
        Some((v, at)) if v == "FAIL" => format!("failed {}", when(at, now)),
        Some((_, at)) => format!("could not run {}", when(at, now)),
        None => "not run yet".to_string(),
    };
    let suffix = match p.state.as_str() {
        "live" => String::new(),
        s => format!(" ({s})"),
    };
    format!("#{} {} · {}{suffix} · {last} · {}", p.id, clean(&p.name, 60), tier_label(p), source_phrase(db, p))
}

// ---------------------------------------------------------------- disputes

pub fn disputes(p: &Promise) -> Vec<Value> {
    p.disputes_json.as_deref().and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok()).unwrap_or_default()
}

fn store_disputes(db: &Connection, id: i64, d: &[Value]) -> Result<()> {
    db.execute("UPDATE promises SET disputes_json=?2 WHERE id=?1", params![id, serde_json::to_string(d)?])?;
    Ok(())
}

/// The AI says a promise is wrong: it turns advisory at once and the user is asked at the next Stop.
pub fn dispute(db: &Connection, p: &Promise, why: &str, now: i64) -> Result<()> {
    let mut d = disputes(p);
    let prev = if p.state == "disputed" { d.iter().rev().find_map(|x| x["prev"].as_str()).unwrap_or("live").to_string() } else { p.state.clone() };
    d.push(json!({"at": now, "why": clean(why, 400), "prev": prev, "valid": null, "shown": false}));
    store_disputes(db, p.id, &d)?;
    if matches!(p.state.as_str(), "live" | "flaky") {
        db.execute("UPDATE promises SET state='disputed' WHERE id=?1", [p.id])?;
    }
    Ok(())
}

/// How many disputes the user has upheld in the last 7 days, over all promises.
pub fn valid_in_window(db: &Connection, now: i64) -> Result<usize> {
    let mut n = 0;
    for p in all(db, true)? {
        n += disputes(&p).iter().filter(|d| d["valid"] == json!(true) && d["at"].as_i64().is_some_and(|a| now - a < 7 * DAY_NS)).count();
    }
    Ok(n)
}

/// Close the open disputes of a promise: valid (the user dropped it, or its control run failed) or not (the user kept it).
/// Two valid disputes within 7 days make the whole gate advisory until `torpor gate on` (meta `gate_demoted`).
pub fn settle_disputes(db: &Connection, id: i64, valid: bool, now: i64) -> Result<()> {
    let Some(p) = get(db, id)? else { return Ok(()) };
    let mut d = disputes(&p);
    for x in d.iter_mut().filter(|x| x["valid"].is_null()) {
        x["valid"] = json!(valid);
        x["shown"] = json!(true);
    }
    store_disputes(db, id, &d)?;
    if valid && valid_in_window(db, now)? >= 2 {
        meta_set(db, "gate_demoted", "1")?;
    }
    Ok(())
}

/// `promise ok`: the user keeps the promise and rejects the dispute.
pub fn keep_after_dispute(db: &Connection, p: &Promise, now: i64) -> Result<bool> {
    let d = disputes(p);
    let open = d.iter().any(|x| x["valid"].is_null());
    let prev = d.iter().rev().find_map(|x| x["prev"].as_str().map(str::to_string)).unwrap_or_else(|| "live".into());
    settle_disputes(db, p.id, false, now)?;
    if p.state == "disputed" {
        db.execute("UPDATE promises SET state=?2 WHERE id=?1", params![p.id, prev])?;
    }
    Ok(open)
}

/// `promise drop`: gone for good (the row stays; its save points are no longer pinned).
pub fn drop_promise(db: &Connection, p: &Promise, now: i64) -> Result<()> {
    let open = disputes(p).iter().any(|x| x["valid"].is_null());
    if open {
        settle_disputes(db, p.id, true, now)?;
    }
    db.execute("UPDATE promises SET state='dropped', announced=1 WHERE id=?1", [p.id])?;
    Ok(())
}

// ---------------------------------------------------------------- what Stop says

fn cut(s: String) -> String {
    trunc_chars(&s, NOTE_MAX)
}

/// The lines Stop adds for promises (spec 14, 24): new ones once, a promise that was not saved, a dispute the user has not seen.
/// Each line starts with "Torpor". Promises left `proposed` for a long time (a qualification that died) are dropped first.
pub fn stop_notes(db: &Connection, now: i64) -> Result<Vec<String>> {
    let stale: Vec<i64> = {
        let mut st = db.prepare("SELECT id FROM promises WHERE tier='proposed' AND state='live' AND created_at<?1")?;
        st.query_map([now - STALE_NS], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
    };
    for id in stale {
        db.execute("UPDATE promises SET state='dropped', announced=0 WHERE id=?1", [id])?;
        db.execute(
            "INSERT INTO promise_runs(promise_id,tree_id,kind,verdict,failed_step,reason,assertions_n,token_ok,ms,at) VALUES(?1,NULL,'create','UNRUNNABLE',NULL,'the check did not finish',0,0,0,?2)",
            params![id, now],
        )?;
    }
    let mut notes: Vec<String> = Vec::new();
    for p in all(db, true)? {
        let name = clean(&p.name, 60);
        if p.state == "dropped" {
            if !p.announced {
                let why: String = db
                    .query_row("SELECT COALESCE(reason,'') FROM promise_runs WHERE promise_id=?1 ORDER BY id DESC LIMIT 1", [p.id], |r| r.get(0))
                    .optional()?
                    .unwrap_or_default();
                notes.push(cut(format!("Torpor: promise #{} \"{name}\" was not saved: {}", p.id, if why.is_empty() { "the check did not finish." } else { &why })));
                db.execute("UPDATE promises SET announced=1 WHERE id=?1", [p.id])?;
            }
            continue;
        }
        if !p.announced && p.tier != "proposed" {
            let label = if p.state == "flaky" { "flaky, advisory only" } else { tier_label(&p) };
            let from = match (p.source == "agent", turn_n(db, p.created_turn)) {
                (true, Some(n)) => format!("proposed by the AI at turn {n}"),
                (true, None) => "proposed by the AI".to_string(),
                (false, Some(n)) => format!("from your words at turn {n}"),
                (false, None) => "from your words".to_string(),
            };
            notes.push(cut(format!("Torpor: new promise #{} \"{name}\" ({label}), {from}. Drop it: torpor promise drop {}", p.id, p.id)));
            db.execute("UPDATE promises SET announced=1 WHERE id=?1", [p.id])?;
        }
        let mut d = disputes(&p);
        let mut changed = false;
        for x in d.iter_mut() {
            if x["shown"] == json!(false) && x["valid"].is_null() {
                let why = x["why"].as_str().unwrap_or("");
                notes.push(cut(format!(
                    "Torpor: the AI disputes promise #{} ({name}): {why}. Keep it: torpor promise ok {} · Drop it: torpor promise drop {}",
                    p.id, p.id, p.id
                )));
                x["shown"] = json!(true);
                changed = true;
            }
        }
        if changed {
            store_disputes(db, p.id, &d)?;
        }
    }
    Ok(notes)
}

// ---------------------------------------------------------------- the gate switches

/// `torpor gate on|off` (user only). `gate_demoted` is set by two valid disputes in 7 days; the gate (item 6) must check both flags.
pub fn gate_set(db: &Connection, on: bool) -> Result<()> {
    if on {
        db.execute("DELETE FROM meta WHERE key IN ('gate_off','gate_demoted')", [])?;
    } else {
        meta_set(db, "gate_off", "1")?;
    }
    Ok(())
}

/// True when the gate may block: neither switched off nor demoted by disputes.
pub fn gate_enabled(db: &Connection) -> bool {
    meta_get(db, "gate_off").is_none() && meta_get(db, "gate_demoted").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_goes_inside_a_closing_bracket() {
        assert_eq!(with_hint("the app did not start (npm run dev exit 1)", "torpor promise show 2"), "the app did not start (npm run dev exit 1; torpor promise show 2)");
        assert_eq!(with_hint("no free port", "h"), "no free port (h)");
    }

    #[test]
    fn clean_strips_control_characters_and_secrets() {
        let s = clean("line one\nline two ghp_abcdefghijklmnopqrstuvwxyz0123456789", 200);
        assert!(!s.contains('\n') && !s.contains("ghp_"), "{s}");
        assert_eq!(clean(&"x".repeat(500), 40).chars().count(), 40);
    }
}
