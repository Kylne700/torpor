//! The command line (spec section 8): hook, status, log, show, back, undo, recover, init, statusline, off, on, retain, forget, gc, quiet.
//! Exit codes: 0 done, 1 error, 2 usage, 3 refused for safety, 4 partly done, 5 not found.
use crate::restore::{self, Target};
use crate::store::*;
use crate::util::*;
use crate::{capture, destructive, dom, history, hook, merge, promise_cmd, retention, scan, status};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const HELP: &str = "Torpor keeps save points of your project, so you can go back.\n\
  torpor status [--all]    Is Torpor saving this folder? Problems, files it could not save\n\
  torpor log [-n 20]       The turns, newest first, and what each one changed\n\
  torpor show K            The files of turn K (torpor show --id P: of one save point)\n\
  torpor back N            Put the whole folder back to an earlier state (see: torpor log)\n\
  torpor undo K            Undo one turn and keep the work after it\n\
  torpor recover           Finish a restore that was interrupted\n\
  torpor init              Report what the first scan of this folder found\n\
  torpor statusline        One line for Claude Code's status bar (see the README)\n\
  torpor gc                Free space used by old save points\n\
  torpor quiet on|off      Show or hide the line Torpor prints after each turn\n\
  torpor keep \"words\"      Save a version you want to come back to (--check - adds a promise)\n\
  torpor promise add -     Save a check that proves something works (the AI writes it; read from stdin)\n\
  torpor promise list      The promises and how far each one is proven (show ID: one promise)\n\
  torpor promise dispute ID \"why\"   Say a promise is wrong; it turns advisory until you decide\n\
  torpor check ID... | --all   Run the checks against the folder as it is now\n\
  torpor find-break [--check - | --promise ID]   Which turn broke it (the AI's check or a promise; none: files changed since the last 'works')\n\
  torpor promise ok | drop | block ID   Keep or remove a disputed promise; block: let a shadow promise stop the AI now (only you)\n\
  torpor gate on | off     Let promises stop the AI, or not (only you)\n\
  torpor off | on          Stop or resume saving this folder (only you, in your own terminal)\n\
  torpor retain DAYS       Keep save points longer than Claude Code keeps chats (only you)\n\
  torpor runner install    Install the browser runner that page checks need; asks first (only you)\n\
  torpor forget            Delete the records of this folder; or --all, or --before YYYY-MM-DD (only you)\n\
  torpor version, torpor help   Which Torpor this is; this screen\n";

pub(crate) fn out(s: &str) {
    let _ = std::io::stdout().write_all(s.as_bytes());
}

pub(crate) fn err(s: &str) {
    let _ = std::io::stderr().write_all(s.as_bytes());
}

pub(crate) fn usage(line: &str) -> i32 {
    err(&format!("Usage: torpor {line}\n"));
    2
}

/// Print the reason for an error and give its exit code.
pub(crate) fn fail(e: &Error) -> i32 {
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
    // Commands that only the user may run: an agent's shell has CLAUDECODE set (spec 7.2, 8).
    if destructive::user_only_args(&a).is_some() && std::env::var_os("CLAUDECODE").is_some_and(|v| !v.is_empty()) {
        let typed: String = a.join(" ").chars().filter(|c| !c.is_control()).take(60).collect();
        err(&format!("Run this in your own terminal: ~/.torpor/bin/torpor {typed}\n"));
        return 3;
    }
    match a.first().copied() {
        // Hook mode never prints anything of its own and never fails (spec 3.1).
        Some("hook") => hook::run(a.get(1).copied().unwrap_or("")),
        Some("status") => status_cmd(&a[1..]),
        Some("log") => log_cmd(&a[1..]),
        Some("show") => show_cmd(&a[1..]),
        Some("back") => back_cmd(&a[1..]),
        Some("undo") => undo_cmd(&a[1..]),
        Some("recover") => recover_cmd(&a[1..]),
        Some("init") => init_cmd(&a[1..]),
        Some("statusline") => statusline_cmd(&a[1..]),
        Some("off") => off_on_cmd(&a[1..], true),
        Some("on") => off_on_cmd(&a[1..], false),
        Some("retain") => retain_cmd(&a[1..]),
        Some("runner") => runner_cmd(&a[1..]),
        Some("forget") => forget_cmd(&a[1..]),
        Some("gc") => gc_cmd(&a[1..]),
        Some("quiet") => quiet_cmd(&a[1..]),
        Some("promise") => promise_cmd::promise_cmd(&a[1..]),
        Some("keep") => promise_cmd::keep(&a[1..]),
        Some("check") => promise_cmd::check(&a[1..]),
        Some("gate") => promise_cmd::gate(&a[1..]),
        Some("find-break") => crate::findbreak::find_break(&a[1..]),
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

pub(crate) fn open_here() -> Result<Ws> {
    let root = resolve_root(None)?;
    Ws::open(&root, false)
}

/// Same, but a folder with no records yet gets them (off, retain, init).
fn open_or_create() -> Result<Ws> {
    let root = resolve_root(None)?;
    Ws::open(&root, true)
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

fn log_cmd(args: &[&str]) -> i32 {
    const U: &str = "log [-n 20] [--json]";
    let (mut n, mut json) = (20usize, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--json" => json = true,
            "-n" => match it.next().and_then(|v| v.parse::<usize>().ok()).filter(|k| (1..=10_000).contains(k)) {
                Some(k) => n = k,
                None => return usage(U),
            },
            _ => return usage(U),
        }
    }
    let ws = match open_here() {
        Ok(ws) => ws,
        Err(e) => return fail(&e),
    };
    match history::log_rows(&ws.conn, n) {
        Ok(rows) if json => {
            out(&format!("{}\n", history::log_json(&rows)));
            0
        }
        Ok(rows) => {
            out(&history::render_log(&rows, now_ns()));
            0
        }
        Err(e) => fail(&e),
    }
}

fn show_cmd(args: &[&str]) -> i32 {
    const U: &str = "show K | --id P [--json]";
    let json = args.contains(&"--json");
    let rest: Vec<&str> = args.iter().copied().filter(|a| *a != "--json").collect();
    let num = |s: &str| s.parse::<i64>().ok().filter(|k| *k >= 1);
    let (turn, point) = match rest.as_slice() {
        ["--id", p] => (None, num(p)),
        [k] => (num(k), None),
        _ => (None, None),
    };
    let ws = match (turn, point) {
        (None, None) => return usage(U),
        _ => match open_here() {
            Ok(ws) => ws,
            Err(e) => return fail(&e),
        },
    };
    let now = now_ns();
    let (found, what) = match (turn, point) {
        (Some(k), _) => (history::show_turn(&ws, k, now), format!("turn {k}")),
        (_, p) => (history::show_point(&ws, p.unwrap_or(0), now), format!("save point {}", p.unwrap_or(0))),
    };
    match found {
        Ok(Some((_, j))) if json => {
            out(&format!("{j}\n"));
            0
        }
        Ok(Some((text, _))) => {
            out(&text);
            0
        }
        Ok(None) => {
            err(&format!("Torpor has no {what}. Try: torpor log\n"));
            5
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

fn undo_cmd(args: &[&str]) -> i32 {
    const U: &str = "undo K [--dry-run] [--force]";
    let (mut turn, mut dry_run, mut force) = (None, false, false);
    for a in args {
        match *a {
            "--dry-run" => dry_run = true,
            "--force" => force = true,
            n => match (n.parse::<i64>().ok().filter(|k| *k >= 1), turn) {
                (Some(k), None) => turn = Some(k),
                _ => return usage(U),
            },
        }
    }
    let Some(k) = turn else { return usage(U) };
    let mut ws = match open_here() {
        Ok(ws) => ws,
        Err(e) => return fail(&e),
    };
    finish_outcome(merge::undo(&mut ws, k, dry_run, force))
}

// ---------------------------------------------------------------- init

/// 2140 -> "2,140".
fn grouped(n: usize) -> String {
    let s = n.to_string();
    let mut o = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            o.push(',');
        }
        o.push(c);
    }
    o
}

/// "a, b, c and 4 more".
fn short_list(items: &[String], max: usize) -> String {
    let mut s = items.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > max {
        s.push_str(&format!(" and {} more", items.len() - max));
    }
    s
}

fn init_cmd(args: &[&str]) -> i32 {
    if !args.is_empty() {
        return usage("init");
    }
    let ws = match open_or_create() {
        Ok(ws) => ws,
        Err(e) => return fail(&e),
    };
    let sc = match hook::first_scan(&ws, &ws.conn) {
        Ok(sc) => sc,
        Err(e) => return fail(&e),
    };
    let db = &ws.conn;
    let checks = if meta_get(db, "mode").as_deref() == Some("fast") { "checks after every shell command" } else { "checks at the end of each turn" };
    out(&format!("Torpor is set up for this folder. First scan: {} files in {} ms ({checks}).\n", grouped(sc.file_count()), sc.elapsed_ms));
    let files = || sc.entries.iter().filter(|e| e.kind != scan::EKind::Dir);
    let secret: Vec<String> = files().filter(|e| e.secret).map(|e| e.path.clone()).collect();
    if !secret.is_empty() {
        out(&format!("Secret files are saved encrypted and never shown: {}.\n", short_list(&secret, 5)));
    }
    let precious: Vec<String> = files().filter(|e| e.precious && !e.secret).map(|e| e.path.clone()).collect();
    if !precious.is_empty() {
        out(&format!("Saved even if git ignores them: {}.\n", short_list(&precious, 5)));
    }
    let skipped: Vec<String> = files()
        .filter_map(|e| {
            let reason = e.reason.or((e.kind == scan::EKind::File && e.size > capture::cap_for(e)).then_some("too_big"))?;
            Some(format!("{} ({})", e.path, status::why(reason, &e.path, None)))
        })
        .collect();
    if !skipped.is_empty() {
        out(&format!("Not saved: {}.\n", short_list(&skipped, 5)));
    }
    for line in status::deep_check_lines(db) {
        out(&format!("{line}\n"));
    }
    let last: Option<i64> = db.query_row("SELECT MAX(last_at) FROM heartbeat", [], |r| r.get(0)).ok().flatten();
    let blocked = status::gather(&ws, false, false, now_ns()).map(|r| r.blocking).unwrap_or(false);
    match last {
        None => out("Hooks: none seen yet. If Claude Code asked whether you trust this folder, answer yes; hooks wait for that.\n"),
        Some(_) if blocked => out("Hooks: they may not be running. See: torpor status\n"),
        Some(t) => out(&format!("Hooks: running (last event {}).\n", local_hhmm(t))),
    }
    0
}

// ---------------------------------------------------------------- statusline

const PROBLEM: &str = "torpor · problem: run torpor status";

/// Torpor's part of the status line. Reads the folder from Claude Code's JSON (`workspace.project_dir`, else `cwd`).
fn statusline_segment(input: &[u8]) -> String {
    let hint = serde_json::from_slice::<serde_json::Value>(input).ok().and_then(|v| {
        ["/workspace/project_dir", "/workspace/current_dir", "/cwd"].iter().find_map(|p| v.pointer(p).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string))
    });
    let ws = match resolve_root(hint.as_deref()).and_then(|r| Ws::open(&r, false)) {
        Ok(w) => w,
        Err(Error::NotFound(_)) => return "torpor · hooks not running".into(),
        Err(Error::Refused(_)) => return "torpor · not saving this folder".into(),
        Err(_) => return PROBLEM.into(),
    };
    let now = now_ns();
    let Ok(r) = status::gather(&ws, false, false, now) else { return PROBLEM.into() };
    if r.off {
        return "torpor · off".into();
    }
    if matches!(check_disk(&ws.home), Err(Error::Disk(_))) {
        return "torpor · paused: disk full".into();
    }
    if r.blocking {
        return "torpor · hooks not running".into();
    }
    let last = match r.newest.map(|t| ago(now, t)) {
        Some(a) if a == "just now" => " · saved just now".to_string(),
        Some(a) => format!(" · last {a}"),
        None => String::new(),
    };
    let more = if r.problems.is_empty() { "" } else { " · see torpor status" };
    let pr = &r.promises;
    let promises = if pr.total > 0 { format!(" · {} promise{} ok · caught {}", pr.ok, if pr.ok == 1 { "" } else { "s" }, pr.caught) } else { String::new() };
    format!("torpor · {} save point{}{promises}{last}{more}", r.points, if r.points == 1 { "" } else { "s" })
}

/// Run the user's own status line command on the same input; its first line, if it printed one within 2 s.
fn run_wrapped(cmd: &[String], input: &[u8]) -> Option<String> {
    use std::process::{Command, Stdio};
    let mut c = match cmd {
        // One argument: the user quoted a whole command line, so a shell reads it.
        [one] => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(one);
            c
        }
        [first, rest @ ..] => {
            let mut c = Command::new(first);
            c.args(rest);
            c
        }
        [] => return None,
    };
    let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let (mut stdin, stdout) = (child.stdin.take()?, child.stdout.take()?);
    let data = input.to_vec();
    std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(stdout.take(1 << 16)).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    // It has printed its line; give it a moment to finish by itself.
    let end = Instant::now() + Duration::from_millis(200);
    while Instant::now() < end && !matches!(child.try_wait(), Ok(Some(_))) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = child.kill();
    let _ = child.wait();
    Some(line.trim_end().to_string()).filter(|l| !l.is_empty())
}

pub(crate) fn stdin_is_tty() -> bool {
    // SAFETY: isatty only inspects file descriptor 0.
    unsafe { libc::isatty(0) == 1 }
}

/// One line, always exit 0 (spec 8): a status bar must never show an error.
fn statusline_cmd(args: &[&str]) -> i32 {
    let wrapped: Vec<String> = args.iter().position(|a| *a == "--").map(|i| args[i + 1..].iter().map(|s| s.to_string()).collect()).unwrap_or_default();
    let mut input = Vec::new();
    if !stdin_is_tty() {
        let _ = std::io::stdin().take(1 << 20).read_to_end(&mut input);
    }
    std::panic::set_hook(Box::new(|_| {}));
    let ours = std::panic::catch_unwind(|| statusline_segment(&input)).unwrap_or_else(|_| PROBLEM.to_string());
    match run_wrapped(&wrapped, &input) {
        Some(first) => out(&format!("{first} · {ours}\n")),
        None => out(&format!("{ours}\n")),
    }
    0
}

// ---------------------------------------------------------------- off, on, retain, forget

fn off_on_cmd(args: &[&str], off: bool) -> i32 {
    if !args.is_empty() {
        return usage(if off { "off" } else { "on" });
    }
    let opened = if off { open_or_create() } else { open_here() };
    let ws = match opened {
        Ok(ws) => ws,
        Err(Error::NotFound(_)) => {
            out("Torpor was not turned off here.\n");
            return 0;
        }
        Err(e) => return fail(&e),
    };
    match meta_set(&ws.conn, "off", if off { "1" } else { "0" }) {
        Ok(()) => {
            out(if off { "Torpor stopped saving this folder. Turn it back on with: torpor on\n" } else { "Torpor is saving this folder again.\n" });
            0
        }
        Err(e) => fail(&e),
    }
}

fn retain_cmd(args: &[&str]) -> i32 {
    const U: &str = "retain <days>";
    let [d] = args else { return usage(U) };
    let Some(days) = d.parse::<i64>().ok().filter(|d| (1..=3650).contains(d)) else { return usage(U) };
    let ws = match open_or_create() {
        Ok(ws) => ws,
        Err(e) => return fail(&e),
    };
    if let Err(e) = meta_set(&ws.conn, "retain_days", &days.to_string()) {
        return fail(&e);
    }
    let (claude, _) = retention::claude_days();
    if days > claude {
        out(&format!("Save points are now kept for {days} days (longer than Claude Code's {claude}).\n"));
    } else {
        out(&format!("Claude Code already keeps chats for {claude} days, which is not less than {days}, so save points are kept for {claude} days.\n"));
    }
    out("The 2 GB limit per project still applies.\n");
    0
}

/// Workspace folders of the store: hex-named, with a `root` file.
fn workspace_dirs(home: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(home) else { return Vec::new() };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| matches!(n.len(), 16 | 20) && n.bytes().all(|b| b.is_ascii_hexdigit())) && p.join("root").is_file())
        .collect()
}

/// Delete one workspace folder once nothing is saving into it. Ok(false): it stayed busy.
fn remove_workspace(dir: &Path) -> Result<bool> {
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("lock"))?;
    let start = Instant::now();
    while f.try_lock().is_err() {
        if start.elapsed() > Duration::from_secs(3) {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::remove_dir_all(dir)?;
    Ok(true)
}

fn confirmed(prompt: &str, word: &str) -> bool {
    out(prompt);
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && line.trim() == word
}

/// `torpor runner install [--yes]` (spec 24): the one download Torpor makes, and only after you say yes.
fn runner_cmd(args: &[&str]) -> i32 {
    const U: &str = "runner install [--yes]";
    let yes = match args {
        ["install"] => false,
        ["install", "--yes"] => true,
        _ => return usage(U),
    };
    let Some(node) = dom::find_node() else {
        err("Node.js is not installed, and the browser runner needs version 20 or newer.\n");
        return 1;
    };
    if let Err(m) = dom::node_ok(&node) {
        err(&format!("{m}\n"));
        return 1;
    }
    let dir = dom::runner_dir(&home_dir());
    let (need_package, need_browser) = (!dom::installed(&dir), dom::find_chromium().is_none() && !dom::own_browser(&dir));
    if !need_package && !need_browser {
        return match dom::refresh_script(&dir) {
            Ok(_) => {
                out(&format!("The browser runner is already installed in {}.\n", dir.display()));
                0
            }
            Err(e) => fail(&Error::Io(e)),
        };
    }
    let question = dom::prompt(&dir, need_package, need_browser);
    if yes {
        out(&format!("{question} y (--yes)\n"));
    } else if !stdin_is_tty() {
        // Nothing is downloaded without a person's answer: a script says yes with --yes.
        out(&format!("{question}\n"));
        err("Nothing was installed. Torpor needs your answer in a terminal; to say yes without one, run: torpor runner install --yes\n");
        return 3;
    } else if !confirmed_yes(&format!("{question} ")) {
        err("Nothing was installed.\n");
        return 3;
    }
    match dom::install(&dir, &node, need_package, need_browser) {
        Ok(said) => {
            out(&said);
            out("Page checks can run now.\n");
            0
        }
        Err(e) => {
            err(&format!("The browser runner was not installed: {e}\n"));
            1
        }
    }
}

fn confirmed_yes(prompt: &str) -> bool {
    out(prompt);
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn forget_cmd(args: &[&str]) -> i32 {
    const U: &str = "forget [--all | --before YYYY-MM-DD]";
    let (mut all, mut before) = (false, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--all" => all = true,
            "--before" => match it.next().and_then(|d| parse_local_date(d).map(|t| (t, d.to_string()))) {
                Some(b) => before = Some(b),
                None => return usage(U),
            },
            _ => return usage(U),
        }
    }
    if all && before.is_some() {
        return usage(U);
    }
    // The confirmation is typed by a person: no terminal, no deletion.
    if !stdin_is_tty() {
        err("torpor forget asks you to type a confirmation, so it only works in a terminal.\n");
        return 3;
    }
    if all {
        return forget_all();
    }
    let mut ws = match open_here() {
        Ok(ws) => ws,
        Err(e) => return fail(&e),
    };
    let name = ws.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let what = match &before {
        Some((_, d)) => format!("the save points of {} made before {d}, with the file names and commands recorded for them", ws.root.display()),
        None => format!("all of Torpor's records of {}", ws.root.display()),
    };
    if !confirmed(&format!("This deletes {what}. Your files are not touched.\nType the folder name to confirm: "), &name) {
        err("Nothing was deleted.\n");
        return 3;
    }
    if let Some((cutoff, d)) = before {
        return match retention::forget_before(&mut ws, cutoff, now_ns()) {
            Ok((n, s)) => {
                out(&format!("Forgot {n} save point{} made before {d} and freed {}. The newest save point is always kept.\n", if n == 1 { "" } else { "s" }, human_bytes(s.bytes)));
                0
            }
            Err(e) => fail(&e),
        };
    }
    if !workspace_dirs(&ws.home).contains(&ws.dir) {
        err("Torpor could not tell which folder holds these records, so nothing was deleted.\n");
        return 1;
    }
    match remove_workspace(&ws.dir) {
        Ok(true) => {
            out("Deleted Torpor's records of this folder. Your files were not touched.\nTorpor starts saving again at your next turn; to keep it from doing that, run: torpor off\n");
            0
        }
        Ok(false) => fail(&Error::Busy),
        Err(e) => fail(&e),
    }
}

fn forget_all() -> i32 {
    let home = home_dir();
    let dirs = workspace_dirs(&home);
    let prompt = format!("This deletes all of Torpor's records of {} folder{}. Your files are not touched.\nType all to confirm: ", dirs.len(), if dirs.len() == 1 { "" } else { "s" });
    if !confirmed(&prompt, "all") {
        err("Nothing was deleted.\n");
        return 3;
    }
    let (mut gone, mut busy) = (0, 0);
    for d in &dirs {
        match remove_workspace(d) {
            Ok(true) => gone += 1,
            _ => busy += 1,
        }
    }
    let _ = std::fs::remove_file(home.join("faults.log"));
    out(&format!("Deleted Torpor's records of {gone} folder{}.\n", if gone == 1 { "" } else { "s" }));
    out(&format!("Torpor's key file is still at {}; delete it too if you want no trace.\n", key_path().display()));
    if busy > 0 {
        err(&format!("{busy} folder{} stayed in use and {} left alone. Close Claude Code and run this again.\n", if busy == 1 { "" } else { "s" }, if busy == 1 { "was" } else { "were" }));
        return 4;
    }
    0
}
