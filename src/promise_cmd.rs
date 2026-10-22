//! The promise commands (spec 14, 17, 24): promise add|list|show|dispute|ok|drop|block, keep, check, gate.
//! Every command that runs an app does so with no store lock and no transaction held (`promise::write` is for rows only).
use crate::capture::{self, PointKind, PointSpec, Req};
use crate::cli::{err, fail, open_here, out, stdin_is_tty, usage};
use crate::promise::{self, Call, Done, LIVE_MAX, Promise, Who};
use crate::spec::{self, Linted, SPEC_BYTES_MAX};
use crate::store::*;
use crate::util::*;
use crate::{restore, scratch, texts, vault};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::io::Read;
use std::time::Duration;

/// A command either goes on or has already said why it stopped and gives its exit code.
pub(crate) type Code<T> = std::result::Result<T, i32>;

const LINT_LINES_MAX: usize = 12;
const KEEP_WORDS_MAX: usize = 200;

/// `fail`, but a promise, save point or turn that does not exist is named.
pub(crate) fn fail_p(e: &Error) -> i32 {
    if let Error::NotFound(w) = e {
        if ["promise ", "save point", "turn ", "check "].iter().any(|p| w.starts_with(p)) {
            err(&format!("Torpor has no {w}.\n"));
            return 5;
        }
    }
    fail(e)
}

/// "3" or "#3".
pub(crate) fn num(s: &str) -> Option<i64> {
    s.trim_start_matches('#').parse::<i64>().ok().filter(|n| *n >= 1)
}

fn say(s: &str) {
    out(&format!("{s}\n"));
}

pub(crate) fn get_live(db: &Connection, id: i64) -> Code<Promise> {
    match promise::get(db, id) {
        Ok(Some(p)) if p.state != "dropped" => Ok(p),
        Ok(_) => Err(fail_p(&Error::NotFound(format!("promise {id}")))),
        Err(e) => Err(fail_p(&e)),
    }
}

pub fn promise_cmd(a: &[&str]) -> i32 {
    match a.first().copied() {
        Some("add") => add(&a[1..]),
        Some("list") => list(&a[1..]),
        Some("show") => show(&a[1..]),
        Some("dispute") => dispute(&a[1..]),
        Some("ok") => ok_or_drop(&a[1..], true),
        Some("drop") => ok_or_drop(&a[1..], false),
        Some("block") => block(&a[1..]),
        Some("qualify") => qualify_cmd(&a[1..]),
        _ => usage("promise add|list|show|dispute|ok|drop|block ... (see: torpor help)"),
    }
}

// ---------------------------------------------------------------- reading a check

/// The check on standard input, linted. Lint lines go to stderr and the exit code is 3 (spec 14).
pub(crate) fn read_check() -> Code<Linted> {
    if stdin_is_tty() {
        return Err(usage("... - (the check is read from standard input, e.g. with <<'EOF')"));
    }
    let mut buf = Vec::new();
    let _ = std::io::stdin().take(SPEC_BYTES_MAX as u64 + 1).read_to_end(&mut buf);
    let raw = String::from_utf8(buf).map_err(|_| {
        err("Rejected: the check is not valid UTF-8 text.\n");
        3
    })?;
    spec::lint(&raw).map_err(|lines| {
        for l in lines.iter().take(LINT_LINES_MAX) {
            err(&format!("{l}\n"));
        }
        if lines.len() > LINT_LINES_MAX {
            err(&format!("Rejected: and {} more problems.\n", lines.len() - LINT_LINES_MAX));
        }
        3
    })
}

// ---------------------------------------------------------------- making a promise

#[derive(Default)]
struct Opts {
    repro: bool,
    background: bool,
    good: Option<i64>,
    turn: Option<i64>,
    bad: Option<i64>,
    replace: Option<i64>,
}

struct Plan {
    source: String,
    want: Option<String>,
    /// None for `--repro`: find-break supplies the good point.
    good: Option<i64>,
    bad: Option<i64>,
    turn: Option<i64>,
    replaces: Option<i64>,
}

/// Everything decided before the row exists: the cap, a repeated check, the version called good, and the source (spec 14).
/// `good` is the version `keep` just saved; `add` leaves it to the options and the turn.
fn plan(db: &Connection, l: &Linted, who: &Who, want: &str, o: &Opts, good: Option<i64>) -> Code<Plan> {
    let old = match o.replace {
        Some(id) => Some(get_live(db, id)?),
        None => None,
    };
    if old.is_none() && promise::live_count(db).map_err(|e| fail_p(&e))? >= LIVE_MAX {
        err(&format!("Torpor keeps at most {LIVE_MAX} promises. Drop one first: torpor promise list\n"));
        return Err(3);
    }
    let same: Option<i64> = db
        .query_row("SELECT id FROM promises WHERE spec_id=?1 AND state<>'dropped' AND id IS NOT ?2 LIMIT 1", params![l.id, o.replace], |r| r.get(0))
        .optional()
        .map_err(|e| fail_p(&e.into()))?;
    if let Some(id) = same {
        err(&format!("Promise #{id} already has this check.\n"));
        return Err(3);
    }
    let inherited = old.as_ref().and_then(|p| p.good_point).filter(|g| promise::point_tree(db, *g).is_ok());
    let good = if o.repro {
        None
    } else {
        Some(
            match (o.good.or(good), o.turn, inherited) {
                (Some(p), _, _) => promise::pick_good(db, who, Some(p), None),
                (None, Some(k), _) => promise::pick_good(db, who, None, Some(k)),
                (None, None, Some(g)) => Ok(g),
                (None, None, None) => promise::pick_good(db, who, None, None),
            }
            .map_err(|e| fail_p(&e))?,
        )
    };
    if let Some(b) = o.bad {
        promise::point_tree(db, b).map_err(|e| fail_p(&e))?;
    }
    // A rewrite keeps the words it came from, once: a rewrite of a rewrite is the AI's own and stays advisory.
    let (source, want_source, turn) = match &old {
        Some(p) if p.replaces.is_none() => (p.source.clone(), p.want_source.clone(), p.created_turn),
        Some(p) => ("agent".to_string(), None, p.created_turn),
        None => {
            let (s, w) = promise::source_for(db, who, want).map_err(|e| fail_p(&e))?;
            (s, w, who.turn.as_ref().map(|t| t.id))
        }
    };
    Ok(Plan { source, want: want_source, good, bad: o.bad, turn, replaces: o.replace })
}

/// The vault entry and the `proposed` row (one short transaction).
fn insert(ws: &mut Ws, key: &Key, l: &Linted, p: &Plan, now: i64) -> Result<i64> {
    promise::write(ws, |w| {
        vault::put(w, key, l)?;
        w.conn.execute(
            "INSERT INTO promises(name,why,source,tier,state,spec_id,good_point,bad_point,created_turn,created_at,want_source,announced,replaces,assertions_n) \
             VALUES(?1,?2,?3,'proposed','live',?4,?5,?6,?7,?8,?9,0,?10,?11)",
            params![promise::clean(&l.spec.name, spec::NAME_MAX), promise::clean(&l.spec.why, spec::WHY_MAX), p.source, l.id, p.good, p.bad, p.turn, now, p.want, p.replaces, l.spec.assertions() as i64],
        )?;
        Ok(w.conn.last_insert_rowid())
    })
}

/// A promise that replaced another one: the old one goes once the new one is saved.
fn after_saved(ws: &mut Ws, p: &Promise) {
    if let Some(old) = p.replaces {
        let _ = promise::write(ws, |w| Ok(w.conn.execute("UPDATE promises SET state='dropped', announced=1 WHERE id=?1 AND state<>'dropped'", [old])?));
    }
}

/// Qualify in the foreground and say the result. 0 saved, 1 not saved.
fn finish_foreground(ws: &mut Ws, id: i64, not_saved_code: i32, repro: bool) -> i32 {
    let r = if repro { crate::findbreak::prove(ws, id, &say) } else { promise::qualify(ws, id, &say) };
    match r {
        Ok(Done::Saved(s)) => {
            let Ok(Some(p)) = promise::get(&ws.conn, id) else { return 1 };
            after_saved(ws, &p);
            say(&promise::saved_line(&ws.conn, &p, &s));
            0
        }
        Ok(Done::NotSaved(why)) => {
            // The AI is told here; Stop repeats only what a background run found.
            let _ = promise::write(ws, |w| Ok(w.conn.execute("UPDATE promises SET announced=1 WHERE id=?1", [id])?));
            err(&format!("Not saved: {why}\n"));
            not_saved_code
        }
        Err(e) => {
            let _ = promise::write(ws, |w| Ok(w.conn.execute("UPDATE promises SET state='dropped', announced=1 WHERE id=?1 AND tier='proposed'", [id])?));
            fail_p(&e)
        }
    }
}

/// `--background`: the same qualification in a detached process (its own process group, so the end of this command does not stop it).
fn spawn_qualify(id: i64, repro: bool) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let flag: &[&str] = if repro { &["--repro"] } else { &[] };
    Command::new(std::env::current_exe()?).args(["promise", "qualify", &id.to_string()]).args(flag).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn().map(|_| ())
}

fn add(args: &[&str]) -> i32 {
    const U: &str = "promise add [--repro] [--good POINT | --turn K] [--bad POINT] [--replace ID] [--background] -";
    let (mut o, mut dash) = (Opts::default(), false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().and_then(|v| num(v));
        match *a {
            "-" => dash = true,
            "--repro" => o.repro = true,
            "--background" => o.background = true,
            // A missing or bad number is stored as 0, which the check below turns into the usage line.
            "--good" => o.good = val().or(Some(0)),
            "--turn" => o.turn = val().or(Some(0)),
            "--bad" => o.bad = val().or(Some(0)),
            "--replace" => o.replace = val().or(Some(0)),
            _ => return usage(U),
        }
    }
    if !dash || [o.good, o.turn, o.bad, o.replace].contains(&Some(0)) || (o.good.is_some() && o.turn.is_some()) {
        return usage(U);
    }
    if o.repro && (o.good.is_some() || o.turn.is_some() || o.bad.is_some()) {
        return usage(U);
    }
    let l = match read_check() {
        Ok(l) => l,
        Err(c) => return c,
    };
    let mut ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    let id = match propose(&mut ws, &l, &o, if o.repro { "repro" } else { "praise" }, "promise add") {
        Ok(i) => i,
        Err(c) => return c,
    };
    if o.background {
        return match spawn_qualify(id, o.repro) {
            Ok(()) => {
                say(&format!("Promise #{id} \"{}\" is being checked in the background. The result shows in the next Torpor line.", promise::clean(&l.spec.name, spec::NAME_MAX)));
                0
            }
            Err(e) => {
                let _ = promise::write(&mut ws, |w| Ok(w.conn.execute("UPDATE promises SET state='dropped', announced=1 WHERE id=?1", [id])?));
                fail_p(&e.into())
            }
        };
    }
    finish_foreground(&mut ws, id, 1, o.repro)
}

/// The `proposed` row for a new check, after the cap, the repeated check and the source have been settled (spec 14). `sub` names the
/// command for the "who ran it" test.
fn propose(ws: &mut Ws, l: &Linted, o: &Opts, want: &str, sub: &str) -> Code<i64> {
    let now = now_ns();
    let who = promise::who(&ws.conn, sub, now).map_err(|e| fail_p(&e))?;
    let p = plan(&ws.conn, l, &who, want, o, None)?;
    let key = Key::load_or_create().map_err(|e| fail_p(&e))?;
    insert(ws, &key, l, &p, now).map_err(|e| fail_p(&e))
}

/// A repro check from `find-break --check -`: the same row `promise add --repro -` makes.
pub(crate) fn propose_repro(ws: &mut Ws, l: &Linted) -> Code<i64> {
    propose(ws, l, &Opts { repro: true, ..Opts::default() }, "repro", "find-break")
}

/// The detached half of `promise add --background` (not in the help text).
fn qualify_cmd(args: &[&str]) -> i32 {
    let (id, repro) = match args {
        [id] => (id, false),
        [id, "--repro"] => (id, true),
        _ => return usage("promise qualify ID [--repro]"),
    };
    let Some(id) = num(id) else { return usage("promise qualify ID [--repro]") };
    let mut ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    match promise::get(&ws.conn, id) {
        Ok(Some(p)) if p.tier == "proposed" && p.state == "live" => {}
        Ok(_) => return 0,
        Err(e) => return fail_p(&e),
    }
    let r = if repro { crate::findbreak::prove(&mut ws, id, &|_| {}) } else { promise::qualify(&mut ws, id, &|_| {}) };
    match r {
        Ok(Done::Saved(_)) => {
            if let Ok(Some(p)) = promise::get(&ws.conn, id) {
                after_saved(&mut ws, &p);
            }
            0
        }
        Ok(Done::NotSaved(_)) => 0,
        Err(e) => {
            let _ = promise::write(&mut ws, |w| Ok(w.conn.execute("UPDATE promises SET state='dropped', announced=0 WHERE id=?1 AND tier='proposed'", [id])?));
            fail_p(&e)
        }
    }
}

// ---------------------------------------------------------------- keep

/// `torpor keep "<words>" [--check -]` (spec 14): a save point the user wants to come back to, and what it was kept for.
pub fn keep(args: &[&str]) -> i32 {
    const U: &str = "keep \"<words>\" [--check -]";
    let (mut words, mut check) = (None::<&str>, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--check" if it.next() == Some(&"-") => check = true,
            w if words.is_none() && !w.starts_with("--") => words = Some(w),
            _ => return usage(U),
        }
    }
    let words = promise::clean(words.unwrap_or(""), KEEP_WORDS_MAX);
    if words.trim().is_empty() {
        return usage(U);
    }
    let l = if check {
        match read_check() {
            Ok(l) => Some(l),
            Err(c) => return c,
        }
    } else {
        None
    };
    let mut ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    let now = now_ns();
    let who = match promise::who(&ws.conn, "keep", now) {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    // An AI run sits inside its own turn; the user's run sits after the newest turn (its START is the version before that work).
    let turn_id = who.turn.as_ref().map(|t| t.id);
    let (session, req_turn) = match (&who.agent, &who.turn) {
        (true, Some(t)) => (t.session_id.clone(), Some(t.id)),
        _ => (String::new(), None),
    };
    let skip = texts::skip_history();
    let saved = (|| -> Result<capture::Captured> {
        ws.lock_shared(restore::LOCK_WAIT_MS)?;
        let r = (|| -> Result<capture::Captured> {
            let tx = Transaction::new_unchecked(&ws.conn, TransactionBehavior::Immediate)?;
            let req = Req {
                session: &session,
                points: vec![PointSpec { kind: PointKind::Keep, turn_id, note: (!skip).then(|| words.clone()) }],
                turn: req_turn,
                only_if_changed: false,
                budget: Some(Duration::from_secs(30)),
            };
            let c = capture::capture(&ws, &tx, &req)?;
            if !skip {
                tx.execute("INSERT INTO constraints(turn_id,text,polarity,source) VALUES(?1,?2,'do',?3)", params![turn_id, words, if who.agent { "keep_agent" } else { "keep_user" }])?;
                crate::constraints::supersede(&tx, tx.last_insert_rowid())?; // only the user's own keep replaces an older opposite one
            }
            tx.commit()?;
            Ok(c)
        })();
        ws.unlock();
        r
    })();
    let c = match saved {
        Ok(c) => c,
        Err(e) => return fail_p(&e),
    };
    let point = c.point_ids[0];
    let at = match promise::turn_n(&ws.conn, turn_id) {
        Some(n) => format!("save point {point}, turn {n}"),
        None => format!("save point {point}"),
    };
    say(&format!("Kept: \"{words}\" ({at})."));
    let Some(l) = l else { return 0 };
    // The promise made with it has the keep as its good version.
    let pl = match plan(&ws.conn, &l, &who, "keep", &Opts::default(), Some(point)) {
        Ok(p) => p,
        Err(c) => return if c == 3 { 4 } else { c },
    };
    let key = match Key::load_or_create() {
        Ok(k) => k,
        Err(e) => return fail_p(&e),
    };
    match insert(&mut ws, &key, &l, &pl, now) {
        Ok(id) => finish_foreground(&mut ws, id, 4, false),
        Err(e) => fail_p(&e),
    }
}

// ---------------------------------------------------------------- list, show

fn list(args: &[&str]) -> i32 {
    let all = match args {
        [] => false,
        ["--all"] => true,
        _ => return usage("promise list [--all]"),
    };
    let ws = match open_here() {
        Ok(w) => w,
        Err(Error::NotFound(_)) => {
            say("No promises yet.");
            return 0;
        }
        Err(e) => return fail_p(&e),
    };
    match promise::all(&ws.conn, all) {
        Ok(v) if v.is_empty() => {
            say("No promises yet.");
            0
        }
        Ok(v) => {
            let now = now_ns();
            for p in &v {
                say(&promise::list_line(&ws.conn, p, now));
            }
            0
        }
        Err(e) => fail_p(&e),
    }
}

/// "passed 5 of 5 on {good} and failed 2 of 2 on {bad}", from the qualification runs.
fn proof(db: &Connection, p: &Promise) -> Option<String> {
    let count = |point: Option<i64>, verdict: &str| -> Option<(i64, i64)> {
        let tree = promise::point_tree(db, point?).ok()?;
        let q = |extra: &str, v: &str| -> Option<i64> {
            let sql = format!("SELECT COUNT(*) FROM promise_runs WHERE promise_id=?1 AND kind='create' AND tree_id=?2{extra}");
            if extra.is_empty() { db.query_row(&sql, params![p.id, &tree[..]], |r| r.get(0)).ok() } else { db.query_row(&sql, params![p.id, &tree[..], v], |r| r.get(0)).ok() }
        };
        let total = q("", "")?;
        let hit = q(" AND verdict=?3", verdict)?;
        (total > 0).then_some((hit, total))
    };
    let (a, n) = count(p.good_point, "PASS")?;
    let good = promise::point_name(db, p.good_point?);
    Some(match count(p.bad_point, "FAIL") {
        Some((b, m)) => format!("Proof: passed {a} of {n} on {good} and failed {b} of {m} on {}.", promise::point_name(db, p.bad_point?)),
        None => format!("Proof: passed {a} of {n} on {good}. No earlier version fails it."),
    })
}

fn show(args: &[&str]) -> i32 {
    const U: &str = "promise show ID [--spec]";
    let (mut id, mut raw) = (None, false);
    for a in args {
        match *a {
            "--spec" => raw = true,
            s => match (num(s), id) {
                (Some(n), None) => id = Some(n),
                _ => return usage(U),
            },
        }
    }
    let Some(id) = id else { return usage(U) };
    let ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    let db = &ws.conn;
    let p = match get_live(db, id) {
        Ok(p) => p,
        Err(c) => return c,
    };
    if raw {
        return match vault::get(&ws, &p.spec_id) {
            Ok(b) => {
                say(&String::from_utf8_lossy(&b));
                0
            }
            Err(e) => fail_p(&e),
        };
    }
    let now = now_ns();
    say(&promise::list_line(db, &p, now));
    match promise::spec_of(&ws, &p) {
        Ok(l) => {
            say(&format!("Why: {}", promise::clean(&l.spec.why, 200)));
            for (i, s) in l.spec.steps.iter().enumerate() {
                let why = s.why.as_deref().map(|w| format!(": {}", promise::clean(w, 160))).unwrap_or_default();
                say(&format!("  step {} {}{why}", i + 1, s.act.verb()));
            }
        }
        Err(e) => say(&format!("The stored check could not be read ({e}). Check torpor status.")),
    }
    if let Some(line) = proof(db, &p) {
        say(&line);
    }
    let runs: Vec<(String, String, Option<i64>, Option<String>, i64)> = (|| -> rusqlite::Result<_> {
        let mut st = db.prepare("SELECT kind,verdict,failed_step,reason,at FROM promise_runs WHERE promise_id=?1 ORDER BY id DESC LIMIT 5")?;
        st.query_map([p.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect()
    })()
    .unwrap_or_default();
    for (kind, verdict, step, reason, at) in runs {
        let step = step.map(|s| format!(" at step {s}")).unwrap_or_default();
        let reason = reason.filter(|r| !r.is_empty()).map(|r| format!(": {r}")).unwrap_or_default();
        say(&format!("  {} {kind} {verdict}{step}{reason}", local_stamp(at, now)));
    }
    for d in promise::disputes(&p).iter().filter(|d| d["valid"].is_null()) {
        say(&format!("Open dispute: {}", d["why"].as_str().unwrap_or("")));
    }
    0
}

// ---------------------------------------------------------------- dispute, ok, drop, block

fn dispute(args: &[&str]) -> i32 {
    const U: &str = "promise dispute ID \"<why>\"";
    let Some((id, why)) = args.split_first() else { return usage(U) };
    let (Some(id), why) = (num(id), promise::clean(&why.join(" "), 400)) else { return usage(U) };
    if why.trim().is_empty() {
        return usage(U);
    }
    let mut ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    let now = now_ns();
    let r = promise::write(&mut ws, |w| {
        let p = promise::get(&w.conn, id)?.filter(|p| p.state != "dropped").ok_or_else(|| Error::NotFound(format!("promise {id}")))?;
        promise::dispute(&w.conn, &p, &why, now)
    });
    match r {
        Ok(()) => {
            say(&format!("Promise #{id} is advisory until the user decides."));
            0
        }
        Err(e) => fail_p(&e),
    }
}

/// `promise ok` (the user keeps it; the dispute is rejected) and `promise drop`. Both are user-only (cli.rs refuses them for an AI).
fn ok_or_drop(args: &[&str], ok: bool) -> i32 {
    let [id] = args else { return usage(if ok { "promise ok ID" } else { "promise drop ID" }) };
    let Some(id) = num(id) else { return usage(if ok { "promise ok ID" } else { "promise drop ID" }) };
    let mut ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    let now = now_ns();
    let r = promise::write(&mut ws, |w| {
        let p = promise::get(&w.conn, id)?.filter(|p| p.state != "dropped").ok_or_else(|| Error::NotFound(format!("promise {id}")))?;
        if ok {
            promise::keep_after_dispute(&w.conn, &p, now)
        } else {
            promise::drop_promise(&w.conn, &p, now).map(|_| true)
        }
    });
    match r {
        Ok(had) => {
            say(&match (ok, had) {
                (true, true) => format!("Promise #{id} stays. The dispute was rejected."),
                (true, false) => format!("Promise #{id} has no open dispute."),
                (false, _) => format!("Promise #{id} dropped. Its save points are no longer pinned."),
            });
            0
        }
        Err(e) => fail_p(&e),
    }
}

/// `promise block ID` (user only): promotion to blocking comes with the gate. shortcut: refuses for now, S2b implements it.
fn block(args: &[&str]) -> i32 {
    let [id] = args else { return usage("promise block ID") };
    if num(id).is_none() {
        return usage("promise block ID");
    }
    err("Torpor cannot make a promise blocking in this version.\n");
    1
}

// ---------------------------------------------------------------- check

/// `torpor check <ids>|--all` (spec 16): the live folder, as it is now, against each promise. No save point is written.
pub fn check(args: &[&str]) -> i32 {
    const U: &str = "check ID... | --all";
    let (mut ids, mut all) = (Vec::new(), false);
    for a in args {
        match (*a, num(a)) {
            ("--all", _) => all = true,
            (_, Some(n)) => ids.push(n),
            _ => return usage(U),
        }
    }
    if all == !ids.is_empty() {
        return usage(U);
    }
    let mut ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    let picked: Code<Vec<Promise>> = if all {
        promise::all(&ws.conn, false).map(|v| v.into_iter().filter(|p| p.tier != "proposed").collect()).map_err(|e| fail_p(&e))
    } else {
        ids.iter().map(|id| get_live(&ws.conn, *id)).collect()
    };
    let ps = match picked {
        Ok(v) => v,
        Err(c) => return c,
    };
    if ps.is_empty() {
        say("No promises to check.");
        return 0;
    }
    let tree = match scratch::snapshot(&mut ws, Duration::from_secs(30)) {
        Ok(t) => t,
        Err(e) => return fail_p(&e),
    };
    let mut worst = 0;
    for p in ps {
        let name = promise::clean(&p.name, spec::NAME_MAX);
        let l = match promise::spec_of(&ws, &p) {
            Ok(l) => l,
            Err(_) => {
                say(&format!("Promise #{} {name}: UNRUNNABLE: its stored check cannot be read (torpor status).", p.id));
                worst = 1;
                continue;
            }
        };
        let c = promise::check_tree(&ws, &l.spec, &tree);
        let at = now_ns();
        let seen = promise::point_of_tree(&ws.conn, &tree);
        let rows = promise::write(&mut ws, |w| {
            c.runs.iter().try_for_each(|r| promise::record(&w.conn, p.id, "check", Some(&tree), r, at))?;
            if let (Call::Pass, Some(pt)) = (c.call, seen) {
                w.conn.execute("UPDATE promises SET last_pass_point=?2 WHERE id=?1", params![p.id, pt])?;
            }
            Ok(())
        });
        if let Err(e) = rows {
            fail_p(&e);
            worst = 1;
        }
        say(&promise::check_line(&p, &l.spec, &c));
        if c.call != Call::Pass {
            worst = 1;
        }
    }
    worst
}

// ---------------------------------------------------------------- gate

/// `torpor gate [on|off]`: the switches only; `on` and `off` are the user's (cli.rs refuses them for an AI).
pub fn gate(args: &[&str]) -> i32 {
    let mut ws = match open_here() {
        Ok(w) => w,
        Err(e) => return fail_p(&e),
    };
    match args {
        [] => {
            let db = &ws.conn;
            say(&if meta_get(db, "gate_off").is_some() {
                "The gate is off. Promises show failures but do not stop the AI. Turn it on: torpor gate on".to_string()
            } else if meta_get(db, "gate_demoted").is_some() {
                "The gate is advisory: two disputes were upheld within 7 days. Turn it on again: torpor gate on".to_string()
            } else {
                "The gate is on.".to_string()
            });
            0
        }
        [s @ ("on" | "off")] => {
            let on = *s == "on";
            match promise::write(&mut ws, |w| promise::gate_set(&w.conn, on)) {
                Ok(()) => {
                    say(if on { "The gate is on." } else { "The gate is off. Promises show failures but do not stop the AI. Turn it on: torpor gate on" });
                    0
                }
                Err(e) => fail_p(&e),
            }
        }
        _ => usage("gate [on|off]"),
    }
}
