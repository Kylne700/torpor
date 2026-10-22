//! `torpor undo K` (spec 6.4): take one turn's changes out of the folder and keep what came after.
//! Per file: base = how turn K left it, theirs = how turn K found it, ours = how it is now (a three-way merge).
//! Any conflict means nothing is written. The plan runs through the same journaled executor as `back`.
use crate::capture::{join, lookup_path, TreeCache};
use crate::restore::{self, entry_state, live_perm, not_saved_text, not_saved_why, perm_for, Act, Entry, Job, Opts, Outcome, Plan, Skip, DB_NOTE, LOCK_WAIT_MS};
use crate::promise;
use crate::scan;
use crate::undo_check;
use crate::store::*;
use crate::util::*;
use rusqlite::{Connection, OptionalExtension};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Files above this size or with a NUL byte are not merged as text (spec 6.4 rule 5).
const TEXT_MAX: usize = 2 << 20;
/// Conflicts listed in full; the rest are counted.
const LIST_MAX: usize = 10;

type Side = Option<TreeEntry>;

/// Same bytes: kind and id. The mode is judged apart (rule 7).
fn same(a: &Side, b: &Side) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.kind == b.kind && a.id == b.id,
        _ => false,
    }
}

fn reg(e: &TreeEntry) -> bool {
    e.kind == 'f' || e.kind == 's'
}

fn mode_of(s: &Side) -> Option<u32> {
    s.as_ref().map(|e| e.mode)
}

fn turns_text(v: &[i64]) -> String {
    match v {
        [] => String::new(),
        [a] => format!("turn {a}"),
        [a, b] => format!("turns {a} and {b}"),
        _ => format!("turns {} and {}", v[..v.len() - 1].iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", "), v[v.len() - 1]),
    }
}

/// "a", "a/b" for "a/b/c.txt", shallowest first.
fn ancestors(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('/').collect();
    (1..parts.len()).map(|i| parts[..i].join("/")).collect()
}

fn depth(p: &str) -> usize {
    p.matches('/').count()
}

struct Undo<'a> {
    ws: &'a Ws,
    db: &'a Connection,
    key: Option<&'a Key>,
    k: i64,
    live: [u8; 32],
    start: [u8; 32],
    tc: TreeCache,
    del: Vec<Entry>,
    cre: Vec<Entry>,
    chm: Vec<Entry>,
    detail: Vec<String>,
    skips: Vec<Skip>,
    conflicts: Vec<(String, String)>,
    /// Files this undo removes / brings back, for the folder pass.
    removed: Vec<String>,
    created: Vec<String>,
}

impl Undo<'_> {
    fn skip(&mut self, path: &str, why: &str, bad: bool) {
        self.skips.push(Skip { path: path.to_string(), why: why.to_string(), bad });
    }

    fn conflict(&mut self, path: &str, why: String) {
        self.conflicts.push((path.to_string(), why));
    }

    /// Later turns (after K) whose changes touched this path.
    fn later(&self, path: &str) -> Vec<i64> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.db.prepare_cached("SELECT DISTINCT t.n FROM changes c JOIN turns t ON t.id=c.turn_id WHERE c.path=?1 AND t.n>?2 AND c.class IN ('edit','shell','subagent','shared') ORDER BY t.n") {
            if let Ok(rows) = st.query_map(rusqlite::params![path, self.k], |r| r.get::<_, i64>(0)) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// " (turns 18 and 20)" or " (outside the chat)"
    fn after(&self, path: &str) -> String {
        let t = turns_text(&self.later(path));
        if t.is_empty() { " (outside the chat)".to_string() } else { format!(" ({t})") }
    }

    fn key_ok(&mut self, kind: char, path: &str) -> bool {
        if kind == 's' && self.key.is_none() {
            self.skip(path, "it is a secret file and the Torpor key is missing", true);
            return false;
        }
        true
    }

    /// Plain text only: saved as a plain file, valid UTF-8, no NUL, at most 2 MiB.
    fn text(&self, e: &TreeEntry) -> Option<String> {
        if e.kind != 'f' || e.size as usize > TEXT_MAX {
            return None;
        }
        let bytes = self.ws.read_blob(self.db, &unhex32(&e.id)?).ok()?;
        if bytes.len() > TEXT_MAX || bytes.contains(&0) {
            return None;
        }
        String::from_utf8(bytes).ok()
    }

    fn delete(&mut self, path: &str, o: &TreeEntry, detail: String) {
        if !self.key_ok(o.kind, path) {
            return;
        }
        self.del.push(Entry { kind: o.kind, old: entry_state(o), ..Entry::bare(path.to_string(), Act::Delete, "", "") });
        self.removed.push(path.to_string());
        self.detail.push(format!("  {path}: {detail}"));
    }

    fn chmod(&mut self, path: &str, o: &TreeEntry, mode: u32, detail: String) {
        if !self.key_ok(o.kind, path) {
            return;
        }
        let new = format!("{}{}:{}", o.kind, mode, o.id);
        self.chm.push(Entry { kind: o.kind, perm: perm_for(o.kind, mode == 755, live_perm(&self.ws.root, path)), old: entry_state(o), new, ..Entry::bare(path.to_string(), Act::Chmod, "", "") });
        self.detail.push(format!("  {path}: {detail}"));
    }

    /// Put `t` (or the merged bytes) at `path`, over `ours` if something is there.
    fn write(&mut self, path: &str, ours: &Side, t: &TreeEntry, merged: Option<Vec<u8>>, mode: u32, detail: String) -> Result<()> {
        if !self.key_ok(t.kind, path) {
            return Ok(());
        }
        let mut old = ours.as_ref().map(entry_state).unwrap_or_default();
        if let Some(o) = ours.as_ref().filter(|o| (o.kind == 'l') != (t.kind == 'l')) {
            // A link became a file or the other way round: take the old one away first.
            self.del.push(Entry { kind: o.kind, old: std::mem::take(&mut old), ..Entry::bare(path.to_string(), Act::Delete, "", "") });
        }
        let existing = !old.is_empty();
        if t.kind == 'l' {
            self.cre.push(Entry { kind: 'l', perm: perm_for('l', false, None), blob: unhex32(&t.id), size: t.size, old, new: entry_state(t), ..Entry::bare(path.to_string(), Act::Symlink, "", "") });
        } else {
            let (blob, size) = match merged {
                Some(bytes) => {
                    let prep = prepare_plain("file", &bytes)?;
                    self.ws.insert_blob(self.db, &prep)?;
                    (prep.id, bytes.len() as u64)
                }
                None => (unhex32(&t.id).ok_or_else(|| Error::msg("a saved file has no copy"))?, t.size),
            };
            let act = if existing { Act::Overwrite } else { Act::Create };
            let perm = perm_for(t.kind, mode == 755, if existing { live_perm(&self.ws.root, path) } else { None });
            let new = format!("{}{}:{}", t.kind, mode, hex(&blob));
            self.cre.push(Entry { kind: t.kind, perm, blob: Some(blob), size, old, new, ..Entry::bare(path.to_string(), act, "", "") });
        }
        if !existing {
            self.created.push(path.to_string());
        }
        self.detail.push(format!("  {path}: {detail}"));
        Ok(())
    }

    /// The rules of spec 6.4 for one path.
    fn one(&mut self, p: &str, base: Side, theirs: Side, ours: Side) -> Result<()> {
        let k = self.k;
        // What Torpor cannot compare, or must not touch.
        if let Some(o) = ours.as_ref().filter(|o| o.kind == 'x') {
            self.skip(p, &not_saved_text(&o.id, true), false);
            return Ok(());
        }
        for s in [&base, &theirs] {
            if let Some(e) = s.as_ref().filter(|e| e.kind == 'x') {
                self.conflict(p, format!("it was not saved when turn {k} ran ({}), so it cannot be compared", not_saved_why(&e.id)));
                return Ok(());
            }
        }
        if [&base, &theirs, &ours].iter().any(|s| s.as_ref().is_some_and(|e| e.kind == 'd')) {
            if !same(&ours, &theirs) {
                self.conflict(p, "a file and a folder swapped places".to_string());
            }
            return Ok(());
        }
        if same(&base, &theirs) && mode_of(&base) == mode_of(&theirs) {
            return Ok(()); // turn K changed it and changed it back
        }
        if scan::is_db_name(p.rsplit('/').next().unwrap_or(p)) && ours.is_some() && !same(&ours, &theirs) {
            self.skip(p, DB_NOTE, false);
            return Ok(());
        }
        // Rule 2: the content is already as it was before turn K. Rule 7: the exec bit is put back when nobody changed it since.
        if same(&ours, &theirs) {
            if let (Some(b), Some(t), Some(o)) = (&base, &theirs, &ours) {
                if reg(b) && reg(t) && reg(o) && o.mode == b.mode && t.mode != b.mode {
                    self.chmod(p, o, t.mode, format!("turn {k}'s permission change removed"));
                }
            }
            return Ok(());
        }
        // Rule 1: nothing touched it after turn K.
        if same(&ours, &base) {
            match (&theirs, &ours) {
                (None, Some(o)) => self.delete(p, o, format!("removed (turn {k} created it)")),
                (Some(t), _) => {
                    let mode = match (&base, &ours) {
                        (Some(b), Some(o)) if reg(b) && reg(o) && o.mode != b.mode => o.mode,
                        _ => t.mode,
                    };
                    let detail = if ours.is_none() { format!("brought back (turn {k} deleted it)") } else { format!("turn {k}'s changes removed") };
                    self.write(p, &ours, t, None, mode, detail)?;
                }
                (None, None) => {}
            }
            return Ok(());
        }
        let Some(o) = ours.as_ref() else {
            let by = self.after(p);
            self.conflict(p, format!("turn {k} changed it, then it was deleted{by}"));
            return Ok(());
        };
        match (&base, &theirs) {
            // Rule 3: turn K created it, and it is not as turn K left it.
            (_, None) => {
                let by = self.after(p);
                self.conflict(p, format!("turn {k} created it, then it changed{by}"));
            }
            // Rule 4: turn K deleted it and it was made again: keep what is there.
            (None, Some(_)) => self.skip(p, &format!("kept as it is, turn {k} deleted it and it was created again later"), false),
            (Some(b), Some(t)) => {
                // Rules 5 and 6: text is merged, anything else is a conflict.
                let (Some(bt), Some(tt), Some(ot)) = (self.text(b), self.text(t), self.text(o)) else {
                    let by = self.after(p);
                    self.conflict(p, format!("it is not plain text and it changed after turn {k}{by}"));
                    return Ok(());
                };
                match diffy::merge(&bt, &ot, &tt) {
                    Ok(m) => {
                        let mode = if o.mode == b.mode { t.mode } else { o.mode };
                        if m == ot {
                            if mode != o.mode {
                                self.chmod(p, o, mode, format!("turn {k}'s permission change removed"));
                            }
                        } else {
                            let later = turns_text(&self.later(p));
                            let kept = if later.is_empty() { "the later changes kept".to_string() } else { format!("{later}'s kept") };
                            self.write(p, &ours, t, Some(m.into_bytes()), mode, format!("turn {k}'s changes removed, {kept}"))?;
                        }
                    }
                    Err(_) => {
                        let later = turns_text(&self.later(p));
                        let who = if later.is_empty() { format!("it was also changed outside the chat, on the same lines as turn {k}") } else { format!("{later} changed the same lines") };
                        self.conflict(p, who);
                    }
                }
            }
        }
        Ok(())
    }

    /// Folders: bring back the ones a recreated file needs, drop the ones a removed file leaves empty when turn K made them.
    fn folders(&mut self) -> Result<(Vec<Entry>, Vec<Entry>)> {
        let (mut mkd, mut rmd) = (Vec::new(), Vec::new());
        let mut made: BTreeSet<String> = BTreeSet::new();
        for p in self.created.clone() {
            for a in ancestors(&p) {
                match lookup_path(self.ws, self.db, &mut self.tc, &self.live, &a)? {
                    Some(e) if e.kind == 'd' => {}
                    None => {
                        if made.insert(a.clone()) {
                            mkd.push(Entry::bare(a, Act::Mkdir, "", "d"));
                        }
                    }
                    Some(_) => self.conflict(&p, format!("{a} is a file now, and it was a folder when turn {} ran", self.k)),
                }
            }
        }
        let mut gone: HashSet<String> = self.removed.iter().cloned().collect();
        let mut dirs: Vec<String> = self.removed.iter().flat_map(|p| ancestors(p)).collect::<BTreeSet<_>>().into_iter().collect();
        dirs.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| a.cmp(b)));
        for d in dirs {
            let Some(e) = lookup_path(self.ws, self.db, &mut self.tc, &self.live, &d)?.filter(|e| e.kind == 'd') else { continue };
            if lookup_path(self.ws, self.db, &mut self.tc, &self.start, &d)?.is_some() {
                continue; // it was there before turn K, so it stays
            }
            let kids = self.ws.load_tree(self.db, &unhex32(&e.id).ok_or_else(|| Error::msg("a saved folder has no id"))?)?;
            if kids.iter().all(|c| gone.contains(&join(&d, &c.name))) {
                rmd.push(Entry::bare(d.clone(), Act::RmdirIfEmpty, "d", ""));
                gone.insert(d);
            }
        }
        Ok((mkd, rmd))
    }
}

/// (start point, end point, tree of start, tree of end) of turn K, or the outcome that says why not.
fn turn_points(db: &Connection, k: i64) -> Result<std::result::Result<(i64, [u8; 32], [u8; 32]), Outcome>> {
    let row: Option<(Option<i64>, Option<i64>)> = db.query_row("SELECT start_point,end_point FROM turns WHERE n=?1", [k], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let Some((start, end)) = row else { return Ok(Err(Outcome::new(5, format!("There is no turn {k}.\n")))) };
    let Some(end) = end else { return Ok(Err(Outcome::new(3, format!("Turn {k} is still running. Wait until it ends, then undo it.\n")))) };
    let Some(start) = start else { return Ok(Err(Outcome::new(5, format!("Turn {k} has no save point from before it started, so it cannot be undone.\n")))) };
    let tree = |id: i64| -> Result<Option<[u8; 32]>> {
        let r: Option<(i64, Vec<u8>)> = db.query_row("SELECT dropped,tree_id FROM points WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        Ok(r.filter(|(d, _)| *d == 0).and_then(|(_, t)| blob_id_from_vec(&t)))
    };
    match (tree(start)?, tree(end)?) {
        (Some(s), Some(e)) => Ok(Ok((start, s, e))),
        _ => Ok(Err(Outcome::new(5, format!("The save points of turn {k} were cleaned up to save space.\n")))),
    }
}

/// `torpor undo K [--dry-run] [--force]` (spec 20; `force` skips the checks). Planning and writing hold the exclusive lock; the checks do not. They start
/// the app and take up to about 95 s, and while the lock is held every hook gives up. So pass one plans and lets go of the lock, the checks run, and pass
/// two plans again and writes only when the plan is the one that was checked. Restore still re-checks each file before it writes.
pub fn undo(ws: &mut Ws, k: i64, dry_run: bool, force: bool) -> Result<Outcome> {
    let (mut said, first) = locked(ws, |ws| undo_first(ws, k, dry_run, force))?;
    let mut out = match first {
        First::Done(o) => o,
        First::Check(before, ready) => {
            let mut checks = undo_check::run(ws, ready, k);
            if let Some(why) = checks.refuse.take() {
                // Nothing to write, and no reason to take the lock for it. The runs happened all the same.
                let _ = promise::write(ws, |w| Ok(undo_check::record(w, &checks.runs)));
                Outcome::new(3, format!("{why}\n"))
            } else {
                let (more, o) = locked(ws, |ws| undo_second(ws, k, &before, checks))?;
                said.extend(more);
                o
            }
        }
    };
    if !said.is_empty() {
        out.text = format!("{}\n{}", said.join("\n"), out.text);
    }
    Ok(out)
}

/// Take the exclusive lock, finish a restore that was cut off (its lines come back for the report), run `f`, let go.
fn locked<T>(ws: &mut Ws, f: impl FnOnce(&Ws) -> Result<T>) -> Result<(Vec<String>, T)> {
    ws.lock_exclusive(LOCK_WAIT_MS)?;
    let r = restore::recover_locked(ws).and_then(|said| Ok((said, f(ws)?)));
    ws.unlock();
    r
}

/// How the first pass of an undo ended.
enum First {
    /// Written, refused, or nothing to do or to check.
    Done(Outcome),
    /// Planned, nothing written, the lock let go: the plan, and the checks to run on its result.
    Check(Plan, undo_check::Ready),
}

fn undo_first(ws: &Ws, k: i64, dry_run: bool, force: bool) -> Result<First> {
    let prep = restore::prepare(ws, &format!("before undoing turn {k}"))?;
    let (start_point, plan, mut detail) = match plan_undo(ws, &prep, k)? {
        Ok(t) => t,
        Err(o) => return Ok(First::Done(o)),
    };
    // Spec 20: the result is checked before anything is written. Nothing to write or a refusal already: nothing to check.
    if !force && plan.refuse.is_none() && !plan.entries.is_empty() {
        match undo_check::ready(ws, &plan, dry_run) {
            Ok(ready) => return Ok(First::Check(plan, ready)), // `prep` goes with it: the transaction rolls back and no save point stays
            Err(mut quick) => detail.append(&mut quick.notes),
        }
    }
    Ok(First::Done(apply(ws, prep, k, dry_run, start_point, detail, plan)?))
}

/// Pass two, after the checks: plan again, and write only when the plan is the one that was checked.
fn undo_second(ws: &Ws, k: i64, before: &Plan, mut checks: undo_check::Checks) -> Result<Outcome> {
    let out = (|| {
        let prep = restore::prepare(ws, &format!("before undoing turn {k}"))?;
        let (start_point, plan, mut detail) = match plan_undo(ws, &prep, k)? {
            Ok(t) => t,
            Err(o) => return Ok(o),
        };
        if plan.refuse.is_none() && signature(&plan) != signature(before) {
            return Ok(Outcome::new(3, format!("The project changed while the checks ran, so nothing was changed. Run `torpor undo {k}` again.\n")));
        }
        detail.append(&mut checks.notes);
        apply(ws, prep, k, false, start_point, detail, plan)
    })();
    // The transaction is over now (a run row written inside it would go with it).
    undo_check::record(ws, &checks.runs);
    out
}

/// What a plan would do: for each entry the path, the action, what the path must hold, what it holds afterwards, and the permission bits.
fn signature(p: &Plan) -> Vec<(&str, Act, &str, &str, u32)> {
    p.entries.iter().map(|e| (e.path.as_str(), e.act, e.old.as_str(), e.new.as_str(), e.perm)).collect()
}

fn apply(ws: &Ws, prep: restore::Prep, k: i64, dry_run: bool, start_point: i64, detail: Vec<String>, plan: Plan) -> Result<Outcome> {
    let head = format!("{} turn {k} only. Later work stays.", if dry_run { "Would undo" } else { "Undoing" });
    restore::go(ws, prep, Job { kind: "undo", target_point: start_point, head, detail, plan }, &Opts { with_data: false, dry_run })
}

/// The plan for undoing turn K on the project as `prep` saw it: (the save point from before the turn, the plan, the lines that explain it), or the answer when there is nothing to plan.
fn plan_undo(ws: &Ws, prep: &restore::Prep, k: i64) -> Result<std::result::Result<(i64, Plan, Vec<String>), Outcome>> {
    let (start_point, start, end) = match turn_points(&prep.tx, k)? {
        Ok(t) => t,
        Err(o) => return Ok(Err(o)),
    };
    let paths: Vec<String> = {
        let mut st = prep.tx.prepare("SELECT DISTINCT c.path FROM changes c JOIN turns t ON t.id=c.turn_id WHERE t.n=?1 AND c.class IN ('edit','shell','subagent') ORDER BY c.path")?;
        let rows = st.query_map([k], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    if paths.is_empty() {
        return Ok(Err(Outcome::new(0, format!("Turn {k} changed no files, so there is nothing to undo.\n"))));
    }
    let mut u = Undo { ws, db: &prep.tx, key: prep.key.as_ref(), k, live: prep.live, start, tc: HashMap::new(), del: vec![], cre: vec![], chm: vec![], detail: vec![], skips: vec![], conflicts: vec![], removed: vec![], created: vec![] };
    for p in &paths {
        if restore::is_claude_settings(p) {
            u.skip(p, restore::CLAUDE_NOTE, false); // not compared either: a conflict on a file Torpor will not touch must not stop the undo
            continue;
        }
        let base = lookup_path(ws, u.db, &mut u.tc, &end, p)?;
        let theirs = lookup_path(ws, u.db, &mut u.tc, &start, p)?;
        let ours = lookup_path(ws, u.db, &mut u.tc, &prep.live, p)?;
        u.one(p, base, theirs, ours)?;
    }
    let (mkd, rmd) = u.folders()?;
    let refuse = if u.conflicts.is_empty() { None } else { Some(conflict_text(&u, &prep.tx)?) };
    let Undo { del, cre, chm, detail, skips, .. } = u;
    Ok(Ok((start_point, Plan::finish(del, rmd, mkd, cre, chm, skips, refuse), detail)))
}

/// "Can't undo turn 17 cleanly, so nothing was changed." and the way out.
fn conflict_text(u: &Undo, db: &Connection) -> Result<String> {
    let k = u.k;
    let mut s = format!("Can't undo turn {k} cleanly, so nothing was changed.");
    for (p, why) in u.conflicts.iter().take(LIST_MAX) {
        s.push_str(&format!("\n  {p}: {why}"));
    }
    if u.conflicts.len() > LIST_MAX {
        s.push_str(&format!("\n  and {} more", u.conflicts.len() - LIST_MAX));
    }
    let prev: Option<i64> = db.query_row("SELECT MAX(n) FROM turns WHERE n<?1 AND end_point IS NOT NULL", [k], |r| r.get(0))?;
    let (lo, hi, count): (Option<i64>, Option<i64>, i64) = db.query_row("SELECT MIN(n),MAX(n),COUNT(*) FROM turns WHERE n>?1", [k], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let back = match prev {
        Some(p) => format!("torpor back --turn {p}"),
        None => format!("torpor back --id {}", db.query_row("SELECT start_point FROM turns WHERE n=?1", [k], |r| r.get::<_, i64>(0))?),
    };
    let drops = match (lo, hi, count) {
        (Some(l), Some(h), c) if c > 0 && l == h => format!(" (also drops turn {l})"),
        (Some(l), Some(h), c) if c > 0 => format!(" (also drops turns {l}-{h})"),
        _ => String::new(),
    };
    s.push_str(&format!("\nInstead: {back}{drops}, or ask the AI to undo it by hand."));
    Ok(s)
}
