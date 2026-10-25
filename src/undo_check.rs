//! Undo with checks (spec 20): before `torpor undo K` writes anything, the result is built in a scratch copy of the project and the promises
//! that watch the undone files run on it. A promise that passes now and fails there stops the undo (exit 3; the user can go on anyway with `--force`).
//! The checks start the app and take a while, so they run without the store lock (merge.rs plans under the lock, lets go, and plans again after).
//! What they need from the plan is made while the undo still holds its transaction: merged bytes exist only there (`ready`).
//! A deadline bounds them. A check that is flaky, ran out of time or already fails now never stops the undo; it is reported. Neither does one that cannot run,
//! except when the app does not start on the result and the same check starts it on the project as it is now: then the undo is the cause.
use crate::gate;
use crate::promise::{self, Call, Promise};
use crate::restore::{Act, Plan};
use crate::runner::{self, Verdict};
use crate::scratch::{self, Opts, RunResult};
use crate::spec::Spec;
use crate::store::Ws;
use crate::util::now_ns;
use std::time::{Duration, Instant};

/// All the checks of one undo together. The Bash tool of Claude Code gives up on a command after 2 minutes unless told otherwise.
const CHECK_SECS: u64 = 90;
/// How long to wait for another check in this project before the result goes unchecked.
const RUN_WAIT_MS: u64 = 5000;
/// Saving the project as it is now, for the checks; the 90 seconds above include it.
const SAVE_SECS: u64 = 30;
/// How the reason of a check starts when the app did not come up (scratch.rs): the one reason that can stop an undo.
/// The two limits, with test overrides (fault-inject builds only, like the other TORPOR_TEST_* variables).
fn check_secs() -> u64 {
    #[cfg(feature = "fault-inject")]
    if let Some(v) = std::env::var("TORPOR_TEST_CHECK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return v;
    }
    CHECK_SECS
}

fn run_wait_ms() -> u64 {
    #[cfg(feature = "fault-inject")]
    if let Some(v) = std::env::var("TORPOR_TEST_RUN_WAIT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return v;
    }
    RUN_WAIT_MS
}

/// One build command, on the result and again on the project as it is.
const BUILD_SECS: u64 = 40;
const NO_START: &str = "the app did not start";
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
    let mut s = ps
        .iter()
        .take(NAMES_MAX)
        .map(gate::short_name)
        .collect::<Vec<_>>()
        .join(", ");
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
            Act::Create | Act::Overwrite if e.kind == 'f' && e.size <= FILE_MAX => e
                .blob
                .and_then(|b| ws.read_blob(&ws.conn, &b).ok())
                .map(Some),
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

/// The same check on the project as it is now. Only a check that passes there is broken by this undo (spec 20); a control that runs late or
/// cannot run is no pass. Its runs are kept too.
fn passes_now(ws: &Ws, spec: &Spec, live: &[u8; 32], id: i64, out: &mut Checks) -> bool {
    let mut now = promise::check_tree(ws, spec, live);
    out.runs.push((id, std::mem::take(&mut now.runs)));
    now.call == Call::Pass
}

/// What the checks of one undo will run: the promises its files reach, and its result as a changed copy of the project.
pub struct Ready {
    picked: Vec<Promise>,
    opts: Opts,
    left_out: usize,
}

/// Look at what this undo reaches, while the undo still holds its transaction: merged bytes exist only there, so the changed copy is made now.
/// `Err` is an undo with nothing to run, and the notes for its report. A dry run only says which promises it would run.
pub fn ready(ws: &Ws, plan: &Plan, dry_run: bool) -> Result<Ready, Checks> {
    let mut out = Checks::default();
    let files: Vec<String> = plan
        .entries
        .iter()
        .filter(|e| !matches!(e.act, Act::Mkdir | Act::RmdirIfEmpty))
        .map(|e| e.path.clone())
        .collect();
    let picked: Vec<Promise> = match promise::all(&ws.conn, false) {
        Ok(v) => v
            .into_iter()
            .filter(|p| gate::eligible(p) && gate::meets(p, &files))
            .collect(),
        Err(_) => {
            out.notes.push(
                "The promise checks could not be read, so the result is untested.".to_string(),
            );
            return Err(out);
        }
    };
    if picked.is_empty() && (dry_run || build_command(ws).is_none()) {
        if !dry_run {
            out.notes
                .push("No checks cover these files, so the result is untested.".to_string());
        }
        return Err(out);
    }
    if dry_run {
        out.notes.push(format!(
            "Would check the result first: {} (a dry run does not run the checks).",
            names(&picked)
        ));
        return Err(out);
    }
    let (opts, left_out) = changed_copy(ws, plan);
    Ok(Ready {
        picked,
        opts,
        left_out,
    })
}

/// Run the promises on the result of undoing turn `k`. No store lock is held: the project as it is now is saved first (shared lock, committed
/// at once) and the checks work on that tree. The 90 seconds start before the saving.
pub fn run(ws: &mut Ws, ready: Ready, k: i64) -> Checks {
    let mut out = Checks::default();
    let Ready {
        picked,
        opts,
        left_out,
    } = ready;
    let _deadline = runner::Deadline::set(Instant::now() + Duration::from_secs(check_secs()));
    let live = match scratch::snapshot(ws, Duration::from_secs(SAVE_SECS)) {
        Ok(t) => t,
        Err(e) => {
            out.notes.push(format!(
                "The project could not be saved for the checks ({}), so the result is untested.",
                promise::clean(&e.to_string(), 120)
            ));
            return out;
        }
    };
    let ws = &*ws;
    let Ok(_held) = scratch::hold_run_lock(ws, run_wait_ms()) else {
        out.notes.push(
            "Another check is running in this project, so the result is untested.".to_string(),
        );
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
            out.notes.push(format!(
                "Promise {name} did not count: its check cannot be read."
            ));
            continue;
        };
        let mut c = promise::check_tree_with(ws, &l.spec, &live, &opts);
        out.runs.push((p.id, std::mem::take(&mut c.runs)));
        // A start that fails once may be a hiccup: only a second failed start on the result can refuse the undo.
        if c.call == Call::Unrunnable && c.reason.starts_with(NO_START) && !runner::out_of_time() {
            c = promise::check_tree_with(ws, &l.spec, &live, &opts);
            out.runs.push((p.id, std::mem::take(&mut c.runs)));
        }
        match c.call {
            Call::Pass => passed.push(p),
            Call::Flaky => out
                .notes
                .push(format!("Promise {name} did not count: it is flaky.")),
            Call::Unrunnable if c.reason.contains(runner::OUT_OF_TIME) => late += 1,
            Call::Unrunnable => {
                let why = promise::clean(c.reason.trim_end_matches('.'), 160);
                if c.reason.starts_with(NO_START) && passes_now(ws, &l.spec, &live, p.id, &mut out)
                {
                    out.refuse = Some(format!(
                        "Undoing turn {k} stops the app from starting (promise {name}), so nothing was changed. To do it anyway, run this in your own terminal: torpor undo {k} --force\nStart: {why}."
                    ));
                    return out;
                }
                out.notes.push(format!(
                    "Promise {name} did not count: it could not run ({why})."
                ));
            }
            Call::Fail => {
                if !passes_now(ws, &l.spec, &live, p.id, &mut out) {
                    out.notes.push(format!(
                        "Promise {name} did not count: it does not pass now either."
                    ));
                    continue;
                }
                let step = c
                    .failed_step
                    .and_then(|n| l.spec.steps.get(n.wrapping_sub(1)))
                    .and_then(|s| s.why.clone());
                let mut text = format!(
                    "Undoing turn {k} breaks promise {name}, so nothing was changed. To do it anyway, run this in your own terminal: torpor undo {k} --force"
                );
                if let Some(why) = step {
                    text.push_str(&format!(
                        "\nFailing step: {}.",
                        promise::clean(why.trim_end_matches('.'), 160)
                    ));
                }
                out.refuse = Some(text);
                return out;
            }
        }
    }
    build_step(ws, &live, &opts, k, &mut out);
    if out.refuse.is_some() {
        return out;
    }
    if late > 0 {
        out.notes.push(format!(
            "{} did not run: the {} seconds for checks were up.",
            plural(late, "check", "checks"),
            check_secs()
        ));
    }
    if !passed.is_empty() {
        let list = passed
            .iter()
            .map(|p| gate::short_name(p))
            .collect::<Vec<_>>()
            .join(", ");
        out.notes.insert(
            0,
            format!("Checked first in a scratch copy, still passing: {list}."),
        );
    }
    if left_out > 0 {
        out.notes.push(format!(
            "{} (links, permissions, secret files or very big ones) were not part of the check.",
            plural(left_out, "change", "changes")
        ));
    }
    out
}

/// The newest Bash command of this project that ended with 0 and looks like a plain build (no pipes, quotes, `;`, `&` or masked parts).
fn build_command(ws: &Ws) -> Option<String> {
    let mut q = ws.conn.prepare("SELECT cmd_preview FROM tool_events WHERE tool_name='Bash' AND exit_code=0 AND failed=0 AND cmd_preview IS NOT NULL ORDER BY id DESC LIMIT 300").ok()?;
    let rows = q.query_map([], |r| r.get::<_, String>(0)).ok()?;
    rows.flatten()
        .map(|c| c.trim().to_string())
        .find(|c| is_build(c))
}

fn is_build(c: &str) -> bool {
    const HEADS: [&str; 13] = [
        "npm run build",
        "pnpm build",
        "pnpm run build",
        "yarn build",
        "yarn run build",
        "bun run build",
        "tsc",
        "npx tsc",
        "cargo build",
        "cargo check",
        "python -m compileall",
        "python3 -m compileall",
        "npm run typecheck",
    ];
    let plain = c
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || " _./:=@-".contains(ch));
    plain
        && HEADS
            .iter()
            .any(|h| c == *h || c.strip_prefix(h).is_some_and(|rest| rest.starts_with(' ')))
}

/// Spec 20 item 1: the build command the AI last ran with success must still succeed on the result. It refuses the undo only when it fails
/// there and passes on the project as it is (the control). A build that cannot run or is cut by the clock is only a note.
fn build_step(ws: &Ws, live: &[u8; 32], opts: &Opts, k: i64, out: &mut Checks) {
    let Some(cmd) = build_command(ws) else { return };
    if runner::out_of_time() {
        out.notes.push(format!(
            "The build `{cmd}` did not run: the {} seconds for checks were up.",
            check_secs()
        ));
        return;
    }
    let limit = Duration::from_secs(BUILD_SECS);
    match scratch::run_command(ws, live, opts, &cmd, limit) {
        Some(true) => out
            .notes
            .push(format!("The build `{cmd}` still passes on the result.")),
        Some(false) => match scratch::run_command(ws, live, &Opts::default(), &cmd, limit) {
            Some(true) => {
                out.refuse = Some(format!(
                    "Undoing turn {k} stops the build `{cmd}` from passing, so nothing was changed. To do it anyway, run this in your own terminal: torpor undo {k} --force"
                ))
            }
            _ => out.notes.push(format!(
                "The build `{cmd}` did not count: it does not pass on the project as it is either."
            )),
        },
        None => out.notes.push(format!(
            "The build `{cmd}` did not count: it could not run in time."
        )),
    }
}

/// The raw runs into `promise_runs`, kind undo, once the undo has ended: a refusal rolls its transaction back, and the runs happened all the same.
/// No tree id: the result is no saved version, and a run without one never counts toward a promise's flake estimate.
pub fn record(ws: &Ws, runs: &[(i64, Vec<RunResult>)]) {
    let now = now_ns();
    for (id, rs) in runs {
        for r in rs {
            // A run cut by the clock says nothing about the check.
            if r.judged.verdict == Verdict::Unrunnable
                && r.judged.reason.contains(runner::OUT_OF_TIME)
            {
                continue;
            }
            let _ = promise::record(&ws.conn, *id, "undo", None, r, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_build;

    #[test]
    fn only_plain_build_commands_count() {
        for c in [
            "npm run build",
            "cargo check --all",
            "tsc -p .",
            "python3 -m compileall -q .",
        ] {
            assert!(is_build(c), "{c}");
        }
        for c in [
            "npm run builder",
            "cargo build && rm -rf x",
            "tsc | tee log",
            "cargo test",
            "npm run build -- --token=[hidden]",
            "ls",
        ] {
            assert!(!is_build(c), "{c}");
        }
    }
}
