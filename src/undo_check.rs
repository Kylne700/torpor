//! Undo with checks (spec 20): before `torpor undo K` writes anything, the result is built in a scratch copy of the project and the promises
//! that watch the undone files run on it. A promise that passes now and fails there stops the undo (exit 3; `--force` goes on anyway).
//! Unlike the Stop gate this runs inside the undo's store lock and open transaction: the merge was built there and must not change meanwhile.
//! A deadline bounds it. A check that cannot run, is flaky, ran out of time or already fails now never stops the undo; it is reported.
use crate::gate;
use crate::promise::{self, Call, Promise};
use crate::restore::{Act, Plan};
use crate::runner::{self, Verdict};
use crate::scratch::{self, Opts, RunResult};
use crate::store::Ws;
use crate::util::now_ns;
use std::time::{Duration, Instant};

/// All the checks of one undo together. The Bash tool of Claude Code gives up on a command after 2 minutes unless told otherwise.
const CHECK_SECS: u64 = 90;
/// How long to wait for another check in this project before the result goes unchecked (the store lock is held meanwhile).
const RUN_WAIT_MS: u64 = 5000;
/// Names listed in a dry run; the rest are counted.
const NAMES_MAX: usize = 5;
/// A file bigger than this is left out of the changed copy.
const FILE_MAX: u64 = 16 << 20;

#[derive(Default)]
pub struct Checks {
    /// Lines for the report, after the file list.
    pub notes: Vec<String>,
    /// Set when the undo must not run: a promise that passes now fails on the result.
    pub refuse: Option<String>,
    /// Every raw run by promise id, for `promise_runs` (kind undo) once the undo is over.
    pub runs: Vec<(i64, Vec<RunResult>)>,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "#1 (login works), #3 (cart works) and 2 more"
fn names(ps: &[Promise]) -> String {
    let mut s = ps.iter().take(NAMES_MAX).map(gate::short_name).collect::<Vec<_>>().join(", ");
    if ps.len() > NAMES_MAX {
        s.push_str(&format!(" and {} more", ps.len() - NAMES_MAX));
    }
    s
}

/// The undo's writes as a changed copy of the live project, and how many it could not take along: links, permission changes, secret files, `.env*`
/// files, dependency folders and files over `FILE_MAX`. (A scratch copy holds none of the first four, and the last would cost too much memory.)
/// shortcut: a permission change on a file that stays is not applied, so a check that runs it directly cannot see the undo take its exec bit away; upgrade when a project needs it.
fn changed_copy(ws: &Ws, plan: &Plan) -> (Opts, usize) {
    let mut o = Opts::default();
    let mut left_out = 0;
    for e in &plan.entries {
        if matches!(e.act, Act::Mkdir | Act::RmdirIfEmpty) {
            continue; // folders follow their files
        }
        let bytes = match e.act {
            _ if !scratch::overlay_ok(&e.path) => None,
            Act::Delete => Some(None),
            Act::Create | Act::Overwrite if e.kind == 'f' && e.size <= FILE_MAX => e.blob.and_then(|b| ws.read_blob(&ws.conn, &b).ok()).map(Some),
            _ => None,
        };
        match bytes {
            Some(b) => {
                if e.perm & 0o111 != 0 && b.is_some() {
                    o.exec.push(e.path.clone());
                }
                o.overlay.push((e.path.clone(), b));
            }
            None => left_out += 1,
        }
    }
    (o, left_out)
}

/// Run the promises this undo reaches on its result. `live` is the project as it is now. A dry run only says which promises it would run.
pub fn run(ws: &Ws, live: &[u8; 32], plan: &Plan, k: i64, dry_run: bool) -> Checks {
    let mut out = Checks::default();
    let files: Vec<String> = plan.entries.iter().filter(|e| !matches!(e.act, Act::Mkdir | Act::RmdirIfEmpty)).map(|e| e.path.clone()).collect();
    let picked: Vec<Promise> = match promise::all(&ws.conn, false) {
        Ok(v) => v.into_iter().filter(|p| gate::eligible(p) && gate::meets(p, &files)).collect(),
        Err(_) => {
            out.notes.push("The promise checks could not be read, so the result is untested.".to_string());
            return out;
        }
    };
    if picked.is_empty() {
        if !dry_run {
            out.notes.push("No checks cover these files, so the result is untested.".to_string());
        }
        return out;
    }
    if dry_run {
        out.notes.push(format!("Would check the result first: {} (a dry run does not run the checks).", names(&picked)));
        return out;
    }
    let (opts, left_out) = changed_copy(ws, plan);
    let _deadline = runner::Deadline::set(Instant::now() + Duration::from_secs(CHECK_SECS));
    let Ok(_held) = scratch::hold_run_lock(ws, RUN_WAIT_MS) else {
        out.notes.push("Another check is running in this project, so the result is untested.".to_string());
        return out;
    };
    let (mut passed, mut late) = (Vec::<&Promise>::new(), 0);
    for p in &picked {
        if runner::out_of_time() {
            late += 1;
            continue;
        }
        let name = gate::short_name(p);
        let Ok(l) = promise::spec_of(ws, p) else {
            out.notes.push(format!("Promise {name} did not count: its check cannot be read."));
            continue;
        };
        let mut c = promise::check_tree_with(ws, &l.spec, live, &opts);
        out.runs.push((p.id, std::mem::take(&mut c.runs)));
        match c.call {
            Call::Pass => passed.push(p),
            Call::Flaky => out.notes.push(format!("Promise {name} did not count: it is flaky.")),
            Call::Unrunnable if c.reason.contains(runner::OUT_OF_TIME) => late += 1,
            Call::Unrunnable => out.notes.push(format!("Promise {name} did not count: it could not run ({}).", promise::clean(c.reason.trim_end_matches('.'), 160))),
            Call::Fail => {
                // Only a check that passes on the project as it is now is broken by this undo (spec 20).
                let mut now = promise::check_tree(ws, &l.spec, live);
                out.runs.push((p.id, std::mem::take(&mut now.runs)));
                if now.call != Call::Pass {
                    out.notes.push(format!("Promise {name} did not count: it does not pass now either."));
                    continue;
                }
                let step = c.failed_step.and_then(|n| l.spec.steps.get(n.wrapping_sub(1))).and_then(|s| s.why.clone());
                let mut text = format!("Undoing turn {k} breaks promise {name}, so nothing was changed. To do it anyway: torpor undo {k} --force");
                if let Some(why) = step {
                    text.push_str(&format!("\nFailing step: {}.", promise::clean(why.trim_end_matches('.'), 160)));
                }
                out.refuse = Some(text);
                return out;
            }
        }
    }
    if late > 0 {
        out.notes.push(format!("{} did not run: the {CHECK_SECS} seconds for checks were up.", plural(late, "check", "checks")));
    }
    if !passed.is_empty() {
        let list = passed.iter().map(|p| gate::short_name(p)).collect::<Vec<_>>().join(", ");
        out.notes.insert(0, format!("Checked first in a scratch copy, still passing: {list}."));
    }
    if left_out > 0 {
        out.notes.push(format!("{} (links, permissions, secret files or very big ones) were not part of the check.", plural(left_out, "change", "changes")));
    }
    out
}

/// The raw runs into `promise_runs`, kind undo, once the undo has ended: a refusal rolls its transaction back, and the runs happened all the same.
/// No tree id: the result is no saved version, and a run without one never counts toward a promise's flake estimate.
pub fn record(ws: &Ws, runs: &[(i64, Vec<RunResult>)]) {
    let now = now_ns();
    for (id, rs) in runs {
        for r in rs {
            // A run cut by the clock says nothing about the check.
            if r.judged.verdict == Verdict::Unrunnable && r.judged.reason.contains(runner::OUT_OF_TIME) {
                continue;
            }
            let _ = promise::record(&ws.conn, *id, "undo", None, r, now);
        }
    }
}
