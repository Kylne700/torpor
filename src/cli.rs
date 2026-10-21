//! The command line (spec section 8). S1a has: hook, status, gc, quiet, version, help.
//! S1b adds back, undo, recover, log, show, init, statusline, off/on, forget, retain (and the user-only guard).
//! Exit codes: 0 done, 1 error, 2 usage, 3 refused for safety, 4 partly done, 5 not found.
use crate::store::*;
use crate::util::*;
use crate::restore::{self, Target};
use crate::{hook, retention, status};
use std::io::Write;

pub const HELP: &str = "Torpor keeps save points of your project, so you can go back.\n\
\n\
  torpor status [--all]    Is Torpor saving this folder? Problems, files it could not save\n\
  torpor back N            Put the whole folder back to an earlier state (see: torpor log)\n\
  torpor recover           Finish a restore that was interrupted\n\
  torpor gc                Free space used by old save points\n\
  torpor quiet on|off      Show or hide the line Torpor prints after each turn\n\
  torpor version           Which Torpor this is\n\
  torpor help              This screen\n";

fn out(s: &str) {
    let _ = std::io::stdout().write_all(s.as_bytes());
}

fn err(s: &str) {
    let _ = std::io::stderr().write_all(s.as_bytes());
}

fn usage(line: &str) -> i32 {
    err(&format!("Usage: torpor {line}\n"));
    2
}

/// Print the reason for an error and give its exit code.
fn fail(e: &Error) -> i32 {
    match e {
        Error::Newer => {
            err("This project's records were written by a newer Torpor. Update the plugin.\n");
            3
        }
        Error::NotFound(_) => {
            err("Torpor has no save points for this folder yet.\n");
            5
        }
        Error::Refused(w) if w == "store" => {
            err("This folder is Torpor's own store, not a project.\n");
            3
        }
        Error::Refused(_) => {
            err(&format!("{}\n", hook::MSG_REFUSED_ROOT));
            3
        }
        Error::Busy => {
            err("Torpor is busy with another restore or cleanup. Try again in a minute.\n");
            1
        }
        Error::Disk(d) => {
            err(&format!("Torpor needs more free disk space first ({d}).\n"));
            3
        }
        e => {
            err(&format!("Torpor could not finish: {e}\n"));
            1
        }
    }
}

pub fn run(args: Vec<String>) -> i32 {
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match a.first().copied() {
        // Hook mode never prints anything of its own and never fails (spec 3.1).
        Some("hook") => hook::run(a.get(1).copied().unwrap_or("")),
        Some("status") => status_cmd(&a[1..]),
        Some("back") => back_cmd(&a[1..]),
        Some("recover") => recover_cmd(&a[1..]),
        Some("gc") => gc_cmd(&a[1..]),
        Some("quiet") => quiet_cmd(&a[1..]),
        Some("version") | Some("--version") | Some("-V") => {
            out(&format!("torpor {}\n", crate::version()));
            0
        }
        Some("help") | Some("--help") | Some("-h") | None => {
            out(HELP);
            0
        }
        Some(other) => {
            err(&format!("Torpor does not know the command '{other}'. Try: torpor help\n"));
            2
        }
    }
}

fn open_here() -> Result<Ws> {
    let root = resolve_root(None)?;
    Ws::open(&root, false)
}

fn status_cmd(args: &[&str]) -> i32 {
    let (mut all, mut json) = (false, false);
    for a in args {
        match *a {
            "--all" => all = true,
            "--json" => json = true,
            _ => return usage("status [--all] [--json]"),
        }
    }
    let ws = match open_here() {
        Ok(ws) => ws,
        Err(Error::NotFound(_)) => {
            // Nothing has run in this folder yet; faults that belong to no folder (the launcher) still count.
            let mut problems = status::global_faults(&home_dir());
            problems.push("No Torpor hook has run in this folder yet. If Claude Code asked whether you trust this folder, answer yes; hooks wait for that.".to_string());
            if json {
                out(&format!("{}\n", serde_json::json!({"saving": false, "points": 0, "problems": problems})));
            } else {
                out("Torpor has no save points for this folder yet.\nProblems:\n");
                for p in problems {
                    out(&format!("  - {p}\n"));
                }
            }
            return 5;
        }
        Err(e) => return fail(&e),
    };
    match status::gather(&ws, all, true, now_ns()) {
        Ok(r) => {
            if json {
                out(&format!("{}\n", status::to_json(&r)));
            } else {
                out(&status::render(&r));
            }
            0
        }
        Err(e) => fail(&e),
    }
}

fn gc_cmd(args: &[&str]) -> i32 {
    let mut if_quiet = false;
    for a in args {
        match *a {
            "--if-quiet" => if_quiet = true,
            _ => return usage("gc [--if-quiet]"),
        }
    }
    let mut ws = match open_here() {
        Ok(ws) => ws,
        Err(Error::NotFound(_)) if if_quiet => return 0,
        Err(e) => return fail(&e),
    };
    let now = now_ns();
    match retention::gc(&mut ws, if_quiet, now) {
        Ok(retention::GcOutcome::Done(s)) => {
            let (n, oldest): (i64, Option<i64>) = ws
                .conn
                .query_row("SELECT COUNT(*),MIN(created_at) FROM points WHERE dropped=0 AND visible=1", [], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap_or((0, None));
            let back = oldest.map(|o| format!(" (back to {})", local_md(o))).unwrap_or_default();
            out(&format!("Freed {}. Kept {n} save point{}{back}.\n", human_bytes(s.bytes), if n == 1 { "" } else { "s" }));
            0
        }
        // A session is in use: cleanup waits. Silent for the background run, a refusal for the person typing it.
        Ok(retention::GcOutcome::NotQuiet) if if_quiet => 0,
        Ok(retention::GcOutcome::NotQuiet) => {
            err("Torpor is saving right now, so cleanup waits. Try again in ten minutes.\n");
            3
        }
        Err(e) => fail(&e),
    }
}

fn quiet_cmd(args: &[&str]) -> i32 {
    let on = match args {
        ["on"] => true,
        ["off"] => false,
        _ => return usage("quiet on|off"),
    };
    let ws = match open_here() {
        Ok(ws) => ws,
        Err(e) => return fail(&e),
    };
    // `quiet on` means the end-of-turn line is off.
    match meta_set(&ws.conn, "quiet", if on { "1" } else { "0" }) {
        Ok(()) => {
            out(&format!("The end-of-turn line is now {}.\n", if on { "off" } else { "on" }));
            0
        }
        Err(e) => fail(&e),
    }
}

fn finish_outcome(r: Result<restore::Outcome>) -> i32 {
    match r {
        Ok(o) => {
            out(&o.text);
            o.code
        }
        Err(e) => fail(&e),
    }
}

fn back_cmd(args: &[&str]) -> i32 {
    const U: &str = "back N | --turn K | --id P [--with-data] [--dry-run]";
    let (mut target, mut opts) = (None, restore::Opts::default());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let t = match *a {
            "--with-data" => {
                opts.with_data = true;
                continue;
            }
            "--dry-run" => {
                opts.dry_run = true;
                continue;
            }
            "--turn" => it.next().and_then(|v| v.parse::<i64>().ok()).filter(|k| *k >= 1).map(Target::Turn),
            "--id" => it.next().and_then(|v| v.parse::<i64>().ok()).filter(|k| *k >= 1).map(Target::Id),
            n => n.parse::<usize>().ok().filter(|k| *k >= 1).map(Target::Back),
        };
        match (t, target) {
            (Some(t), None) => target = Some(t),
            _ => return usage(U),
        }
    }
    let Some(target) = target else { return usage(U) };
    let mut ws = match open_here() {
        Ok(ws) => ws,
        Err(e) => return fail(&e),
    };
    finish_outcome(restore::back(&mut ws, target, opts))
}

fn recover_cmd(args: &[&str]) -> i32 {
    if !args.is_empty() {
        return usage("recover");
    }
    let mut ws = match open_here() {
        Ok(ws) => ws,
        Err(e) => return fail(&e),
    };
    finish_outcome(restore::recover(&mut ws))
}
