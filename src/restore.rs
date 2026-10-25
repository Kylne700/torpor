//! Whole-folder restore (spec 6.1 to 6.3), the crash protocol behind it, and `torpor recover`.
//! A restore is a journaled plan. After any crash the folder is exactly as before or exactly as planned, never a mix:
//! nothing outside the stage folder changes before the journal exists, and `recover` rolls the journal forward.
//! `merge.rs` builds the plan of an undo; planning, staging, applying and finishing are shared.
use crate::capture::{self, PointKind, PointSpec, Req, join};
use crate::scan;
use crate::store::*;
use crate::util::*;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Free space that must stay after a restore wrote its files (spec 6.3 step 0).
const SPARE_BYTES: u64 = 256 << 20;
const STALE_TMP_NS: i64 = 3600 * NS_PER_SEC;
pub const LOCK_WAIT_MS: u64 = 5000;
pub const DB_NOTE: &str =
    "database content is not rewound; if the schema changed, run your migrations";
pub const CLAUDE_NOTE: &str = "it is a Claude Code settings file, which Torpor never changes";

/// The project's own Claude Code settings (spec 30: never write a Claude Code file). Case-insensitive like the `.git` check,
/// because a case-insensitive disk would make `.Claude/Settings.json` the same file.
pub fn is_claude_settings(rel: &str) -> bool {
    let low = rel.to_ascii_lowercase();
    low == ".claude/settings.json" || low == ".claude/settings.local.json"
}

/// What a command prints (stdout) and its exit code.
#[derive(Debug)]
pub struct Outcome {
    pub code: i32,
    pub text: String,
}

impl Outcome {
    pub fn new(code: i32, text: impl Into<String>) -> Outcome {
        Outcome {
            code,
            text: text.into(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Target {
    /// `back N`: the Nth earlier state, newest first.
    Back(usize),
    /// `back --turn K`
    Turn(i64),
    /// `back --id P`
    Id(i64),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    pub with_data: bool,
    pub dry_run: bool,
}

// ---------------------------------------------------------------- plan

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Create,
    Overwrite,
    Delete,
    Mkdir,
    RmdirIfEmpty,
    Symlink,
    Chmod,
}

impl Act {
    pub fn as_str(self) -> &'static str {
        match self {
            Act::Create => "create",
            Act::Overwrite => "overwrite",
            Act::Delete => "delete",
            Act::Mkdir => "mkdir",
            Act::RmdirIfEmpty => "rmdir_if_empty",
            Act::Symlink => "symlink",
            Act::Chmod => "chmod",
        }
    }
    fn parse(s: &str) -> Option<Act> {
        [
            Act::Create,
            Act::Overwrite,
            Act::Delete,
            Act::Mkdir,
            Act::RmdirIfEmpty,
            Act::Symlink,
            Act::Chmod,
        ]
        .into_iter()
        .find(|a| a.as_str() == s)
    }
    fn writes(self) -> bool {
        matches!(self, Act::Create | Act::Overwrite | Act::Symlink)
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub n: usize,
    pub path: String,
    pub act: Act,
    /// What the path must hold for this entry to apply (`entry_state` format; "" = nothing there).
    pub old: String,
    /// What it holds afterwards.
    pub new: String,
    /// What gets written: f (file), s (secret file), l (symlink) or d.
    pub kind: char,
    /// Real Unix permission bits for the new file (the tree's decimal 644/755 is mapped before it gets here).
    pub perm: u32,
    /// Store id of the bytes to write. Not journaled: the stage holds them.
    pub blob: Option<[u8; 32]>,
    pub size: u64,
}

impl Entry {
    pub fn bare(path: String, act: Act, old: &str, new: &str) -> Entry {
        Entry {
            n: 0,
            path,
            act,
            old: old.to_string(),
            new: new.to_string(),
            kind: 'd',
            perm: 0,
            blob: None,
            size: 0,
        }
    }
}

/// A path the restore leaves alone, and why. `bad` skips make the exit code 4.
#[derive(Clone, Debug)]
pub struct Skip {
    pub path: String,
    pub why: String,
    pub bad: bool,
}

#[derive(Default)]
pub struct Plan {
    pub entries: Vec<Entry>,
    pub skips: Vec<Skip>,
    /// Set when the plan cannot be run at all (exit 3).
    pub refuse: Option<String>,
}

impl Plan {
    /// Order the entries (deletes deepest first, folders to remove, folders to make shallowest first, writes, chmods) and number them.
    pub fn finish(
        del: Vec<Entry>,
        rmd: Vec<Entry>,
        mkd: Vec<Entry>,
        cre: Vec<Entry>,
        chm: Vec<Entry>,
        skips: Vec<Skip>,
        refuse: Option<String>,
    ) -> Plan {
        fn depth(e: &Entry) -> usize {
            e.path.matches('/').count()
        }
        let (mut del, mut rmd, mut mkd, mut cre, mut chm) = (del, rmd, mkd, cre, chm);
        // Claude Code's settings files stay out of every plan (`undo` and `back` both end here). Said in the report, first in
        // the list so "and N more" never cuts them off.
        let mut skips = skips;
        for l in [&mut del, &mut rmd, &mut mkd, &mut cre, &mut chm] {
            for e in l.extract_if(.., |e| is_claude_settings(&e.path)) {
                if !skips.iter().any(|s| s.path == e.path) {
                    skips.push(Skip {
                        path: e.path,
                        why: CLAUDE_NOTE.to_string(),
                        bad: false,
                    });
                }
            }
        }
        skips.sort_by_key(|s| !is_claude_settings(&s.path)); // stable: the rest keeps its order
        del.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| a.path.cmp(&b.path)));
        rmd.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| a.path.cmp(&b.path)));
        mkd.sort_by(|a, b| depth(a).cmp(&depth(b)).then_with(|| a.path.cmp(&b.path)));
        cre.sort_by(|a, b| a.path.cmp(&b.path));
        chm.sort_by(|a, b| a.path.cmp(&b.path));
        let mut entries: Vec<Entry> = del
            .into_iter()
            .chain(rmd)
            .chain(mkd)
            .chain(cre)
            .chain(chm)
            .collect();
        for (i, e) in entries.iter_mut().enumerate() {
            e.n = i + 1;
        }
        Plan {
            entries,
            skips,
            refuse,
        }
    }
}

/// One comparable string for what a path holds: "" nothing, "d" a folder, otherwise kind + mode + id ("f644:<hex>").
pub fn entry_state(e: &TreeEntry) -> String {
    match e.kind {
        'd' => "d".to_string(),
        'x' => format!("x:{}", e.id),
        k => format!("{k}{}:{}", e.mode, e.id),
    }
}

/// The same string for what is on disk now. "?" = something Torpor cannot compare (special file, too big, secret without the key).
pub fn live_state(root: &Path, rel: &str, key: Option<&Key>) -> String {
    let p = root.join(rel);
    let md = match fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return String::new(),
        Err(_) => return "?".into(),
    };
    let ft = md.file_type();
    if ft.is_dir() {
        return "d".into();
    }
    if ft.is_symlink() {
        return match fs::read_link(&p) {
            Ok(t) => format!(
                "l644:{}",
                hex(blake3::hash(t.as_os_str().as_bytes()).as_bytes())
            ),
            Err(_) => "?".into(),
        };
    }
    if !ft.is_file() || md.len() > capture::PRECIOUS_CAP {
        return "?".into();
    }
    let Ok(bytes) = fs::read(&p) else {
        return "?".into();
    };
    let exec = if md.mode() & 0o111 != 0 { 755 } else { 644 };
    if scan::is_secret(rel) {
        match key {
            Some(k) => format!("s{exec}:{}", hex(&k.secret_id(&bytes))),
            None => "?".into(),
        }
    } else {
        format!("f{exec}:{}", hex(blake3::hash(&bytes).as_bytes()))
    }
}

/// Real permission bits for a written file. Secrets: 0600 (0700 with the exec bit). A file that exists keeps its live bits
/// with the exec bit taken from the tree. A recreated plain file: 0644 / 0755 (the umask applies when it is created).
pub fn perm_for(kind: char, exec: bool, live: Option<u32>) -> u32 {
    if kind == 's' {
        return if exec { 0o700 } else { 0o600 };
    }
    match live {
        Some(m) => {
            let b = m & 0o666;
            if exec { b | ((b & 0o444) >> 2) } else { b }
        }
        None => {
            if exec {
                0o755
            } else {
                0o644
            }
        }
    }
}

pub fn live_perm(root: &Path, rel: &str) -> Option<u32> {
    fs::symlink_metadata(root.join(rel))
        .ok()
        .map(|m| m.mode() & 0o7777)
}

pub(crate) fn not_saved_why(reason: &str) -> &'static str {
    match reason {
        "too_big" => "it was too big to save",
        "cap" => "it was not saved in time",
        "db_busy" => "its database was busy",
        "unreadable" => "it could not be read",
        "name" => "its name is not valid text",
        "special" => "it is not a regular file",
        "unstable" => "it kept changing while it was saved",
        _ => "it was not saved",
    }
}

pub(crate) fn not_saved_text(reason: &str, live_side: bool) -> String {
    let why = not_saved_why(reason);
    if live_side {
        format!("left alone, {why}")
    } else {
        format!("nothing to bring back, {why}")
    }
}

struct Planner<'a> {
    ws: &'a Ws,
    db: &'a Connection,
    key: Option<&'a Key>,
    with_data: bool,
    del: Vec<Entry>,
    rmd: Vec<Entry>,
    mkd: Vec<Entry>,
    cre: Vec<Entry>,
    chm: Vec<Entry>,
    skips: Vec<Skip>,
    refuse: Option<String>,
}

impl Planner<'_> {
    fn skip(&mut self, path: &str, why: &str, bad: bool) {
        self.skips.push(Skip {
            path: path.to_string(),
            why: why.to_string(),
            bad,
        });
    }

    /// Secret files need the key (to read the saved copy, or to tell what is on disk now).
    fn key_ok(&mut self, kinds: &[char], path: &str) -> bool {
        if self.key.is_none() && kinds.contains(&'s') {
            self.skip(
                path,
                "it is a secret file and the Torpor key is missing",
                true,
            );
            return false;
        }
        true
    }

    /// Databases (spec 6.3 step 2): a live DB file is left alone unless --with-data; a missing one is restored.
    fn db_ok(&mut self, path: &str, exists_live: bool) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        if !scan::is_db_name(name) || !exists_live {
            return true;
        }
        if !self.with_data {
            self.skip(path, DB_NOTE, false);
            return false;
        }
        let busy = fs::metadata(self.ws.root.join(format!("{path}-wal")))
            .is_ok_and(|m| m.len() > 0)
            || self.ws.root.join(format!("{path}-journal")).exists();
        if busy {
            self.refuse.get_or_insert_with(|| format!("Stop your app first: {path} is open (it has a write-ahead log or a journal next to it). Nothing was changed."));
            return false;
        }
        true
    }

    fn walk(&mut self, a: Option<[u8; 32]>, b: Option<[u8; 32]>, dir: &str) -> Result<()> {
        let la = match a {
            Some(id) => self.ws.load_tree(self.db, &id)?,
            None => Vec::new(),
        };
        let lb = match b {
            Some(id) => self.ws.load_tree(self.db, &id)?,
            None => Vec::new(),
        };
        let (mut i, mut j) = (0, 0);
        while i < la.len() || j < lb.len() {
            let ord = match (la.get(i), lb.get(j)) {
                (Some(x), Some(y)) => x.name.as_bytes().cmp(y.name.as_bytes()),
                (Some(_), None) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
            match ord {
                std::cmp::Ordering::Less => {
                    let x = la[i].clone();
                    i += 1;
                    self.gone(&x, dir)?;
                }
                std::cmp::Ordering::Greater => {
                    let y = lb[j].clone();
                    j += 1;
                    self.appear(&y, dir)?;
                }
                std::cmp::Ordering::Equal => {
                    let (x, y) = (la[i].clone(), lb[j].clone());
                    i += 1;
                    j += 1;
                    if x.kind == y.kind && x.mode == y.mode && x.id == y.id {
                        continue;
                    }
                    if x.kind == 'd' && y.kind == 'd' {
                        self.walk(unhex32(&x.id), unhex32(&y.id), &join(dir, &x.name))?;
                    } else {
                        self.replace(&x, &y, dir)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Something that exists now and not in the target.
    fn gone(&mut self, x: &TreeEntry, dir: &str) -> Result<()> {
        let path = join(dir, &x.name);
        match x.kind {
            'd' => {
                if let Some(id) = unhex32(&x.id) {
                    for c in self.ws.load_tree(self.db, &id)? {
                        self.gone(&c, &path)?;
                    }
                }
                self.rmd.push(Entry::bare(path, Act::RmdirIfEmpty, "d", ""));
            }
            'x' => self.skip(&path, &not_saved_text(&x.id, true), false),
            k => {
                if self.key_ok(&[k], &path) && self.db_ok(&path, true) {
                    self.del.push(Entry {
                        kind: k,
                        old: entry_state(x),
                        ..Entry::bare(path, Act::Delete, "", "")
                    });
                }
            }
        }
        Ok(())
    }

    /// Something in the target that does not exist now.
    fn appear(&mut self, y: &TreeEntry, dir: &str) -> Result<()> {
        let path = join(dir, &y.name);
        match y.kind {
            'd' => {
                self.mkd
                    .push(Entry::bare(path.clone(), Act::Mkdir, "", "d"));
                if let Some(id) = unhex32(&y.id) {
                    for c in self.ws.load_tree(self.db, &id)? {
                        self.appear(&c, &path)?;
                    }
                }
            }
            'x' => self.skip(&path, &not_saved_text(&y.id, false), false),
            k => {
                if self.key_ok(&[k], &path) && self.db_ok(&path, false) {
                    let act = if k == 'l' { Act::Symlink } else { Act::Create };
                    self.cre.push(Entry {
                        kind: k,
                        perm: perm_for(k, y.mode == 755, None),
                        blob: unhex32(&y.id),
                        size: y.size,
                        new: entry_state(y),
                        ..Entry::bare(path, act, "", "")
                    });
                }
            }
        }
        Ok(())
    }

    /// Both exist and differ, and they are not two folders.
    fn replace(&mut self, x: &TreeEntry, y: &TreeEntry, dir: &str) -> Result<()> {
        let path = join(dir, &x.name);
        if x.kind == 'x' {
            // What is on disk now was never saved: overwriting it would lose it for good.
            if self.with_data && scan::is_db_name(&x.name) {
                self.db_ok(&path, true); // an open database (log or journal next to it) makes the whole restore refuse
            }
            self.skip(&path, &not_saved_text(&x.id, true), false);
            return Ok(());
        }
        if y.kind == 'x' {
            self.skip(&path, &not_saved_text(&y.id, false), false);
            return Ok(());
        }
        let filey = |k: char| k == 'f' || k == 's';
        if filey(x.kind) && filey(y.kind) {
            if !self.key_ok(&[x.kind, y.kind], &path) || !self.db_ok(&path, true) {
                return Ok(());
            }
            let perm = perm_for(y.kind, y.mode == 755, live_perm(&self.ws.root, &path));
            let (act, blob, size) = if x.id != y.id {
                (Act::Overwrite, unhex32(&y.id), y.size)
            } else {
                (Act::Chmod, None, 0)
            };
            let e = Entry {
                kind: y.kind,
                perm,
                blob,
                size,
                old: entry_state(x),
                new: entry_state(y),
                ..Entry::bare(path, act, "", "")
            };
            if act == Act::Chmod {
                self.chm.push(e)
            } else {
                self.cre.push(e)
            }
            return Ok(());
        }
        if x.kind == 'l' && y.kind == 'l' {
            self.cre.push(Entry {
                kind: 'l',
                perm: perm_for('l', false, None),
                blob: unhex32(&y.id),
                size: y.size,
                old: entry_state(x),
                new: entry_state(y),
                ..Entry::bare(path, Act::Symlink, "", "")
            });
            return Ok(());
        }
        // A different kind of thing: take the old one away, then bring the new one.
        self.gone(x, dir)?;
        self.appear(y, dir)
    }
}

/// The plan that turns the folder with tree `live` into the one with tree `want`.
pub fn plan_trees(
    ws: &Ws,
    db: &Connection,
    live: &[u8; 32],
    want: &[u8; 32],
    key: Option<&Key>,
    with_data: bool,
) -> Result<Plan> {
    let mut p = Planner {
        ws,
        db,
        key,
        with_data,
        del: vec![],
        rmd: vec![],
        mkd: vec![],
        cre: vec![],
        chm: vec![],
        skips: vec![],
        refuse: None,
    };
    p.walk(Some(*live), Some(*want), "")?;
    Ok(Plan::finish(
        p.del, p.rmd, p.mkd, p.cre, p.chm, p.skips, p.refuse,
    ))
}

// ---------------------------------------------------------------- points and targets

struct State {
    id: i64,
    tree: [u8; 32],
}

fn tree_of(v: Vec<u8>) -> [u8; 32] {
    blob_id_from_vec(&v).unwrap_or([0; 32])
}

/// Visible, not thinned points newest first; a run of equal trees counts once (its newest point stands for it).
fn states(db: &Connection) -> Result<Vec<State>> {
    let mut st =
        db.prepare("SELECT id,tree_id FROM points WHERE visible=1 AND dropped=0 ORDER BY id DESC")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
    let mut out: Vec<State> = Vec::new();
    for r in rows {
        let (id, t) = r?;
        let tree = tree_of(t);
        if out.last().is_some_and(|s| s.tree == tree) {
            continue;
        }
        out.push(State { id, tree });
    }
    Ok(out)
}

/// "end of turn 15, today 13:55"
pub fn describe(db: &Connection, id: i64, now: i64) -> String {
    let row: Option<(String, i64, Option<String>, Option<i64>)> = db
        .query_row("SELECT p.kind,p.created_at,p.note,t.n FROM points p LEFT JOIN turns t ON t.id=p.turn_id WHERE p.id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .optional()
        .ok()
        .flatten();
    let Some((kind, at, note, n)) = row else {
        return "an earlier state".to_string();
    };
    let what = match (kind.as_str(), n) {
        ("end", Some(n)) => format!("end of turn {n}"),
        ("start", Some(n)) => format!("start of turn {n}"),
        ("pre_bash", _) => match note.as_deref() {
            Some(c) if !c.trim().is_empty() => {
                format!("before {}", c.split(" (").next().unwrap_or(c))
            }
            _ => "before a shell command".to_string(),
        },
        ("pre_restore", _) => "before a restore".to_string(),
        ("restored", _) => "after a restore".to_string(),
        ("keep", _) => match note.as_deref() {
            Some(w) if !w.trim().is_empty() => format!("kept \"{}\"", trunc_chars(w, 50)),
            _ => "a kept version".to_string(),
        },
        _ => "between turns".to_string(),
    };
    format!("{what}, {}", local_stamp(at, now))
}

/// Where `back` goes: (point id, its tree), or the outcome that explains why not.
fn resolve(
    db: &Connection,
    target: Target,
    live: &[u8; 32],
) -> Result<std::result::Result<(i64, [u8; 32]), Outcome>> {
    let by_point = |id: i64| -> Result<Option<(bool, [u8; 32])>> {
        Ok(db
            .query_row(
                "SELECT dropped,tree_id FROM points WHERE id=?1",
                [id],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
            .map(|(d, t)| (d, tree_of(t))))
    };
    match target {
        Target::Back(n) => {
            let mut list = states(db)?;
            while list.first().is_some_and(|s| s.tree == *live) {
                list.remove(0);
            }
            if n == 0 || n > list.len() {
                let msg = if list.is_empty() {
                    "There is no earlier state to go back to yet.".to_string()
                } else {
                    format!(
                        "There are only {} earlier states. Try: torpor back {}",
                        list.len(),
                        list.len()
                    )
                };
                return Ok(Err(Outcome::new(5, format!("{msg}\n"))));
            }
            let s = &list[n - 1];
            Ok(Ok((s.id, s.tree)))
        }
        Target::Turn(k) => {
            let row: Option<(String, Option<i64>)> = db
                .query_row(
                    "SELECT end_kind,end_point FROM turns WHERE n=?1",
                    [k],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((_, end)) = row else {
                return Ok(Err(Outcome::new(5, format!("There is no turn {k}.\n"))));
            };
            let Some(end) = end else {
                return Ok(Err(Outcome::new(
                    3,
                    format!(
                        "Turn {k} is still running. Wait until it ends, or use: torpor back --id <save point>\n"
                    ),
                )));
            };
            match by_point(end)? {
                Some((false, t)) => Ok(Ok((end, t))),
                _ => Ok(Err(Outcome::new(
                    5,
                    format!(
                        "The save point at the end of turn {k} was cleaned up to save space.\n"
                    ),
                ))),
            }
        }
        Target::Id(p) => match by_point(p)? {
            Some((false, t)) => Ok(Ok((p, t))),
            Some((true, _)) => Ok(Err(Outcome::new(
                5,
                format!("Save point {p} was cleaned up to save space.\n"),
            ))),
            None => Ok(Err(Outcome::new(
                5,
                format!("There is no save point {p}.\n"),
            ))),
        },
    }
}

// ---------------------------------------------------------------- the report

fn names(v: &[String]) -> String {
    match v.len() {
        0 => String::new(),
        1..=3 => v.join(", "),
        n => format!("{}, {} and {} more", v[0], v[1], n - 2),
    }
}

fn files_word(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

#[derive(Default, Clone, Debug)]
pub struct Counts {
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    pub brought: Vec<String>,
    pub not_touched: Vec<Skip>,
}

impl Counts {
    pub fn total(&self) -> usize {
        self.restored.len() + self.removed.len() + self.brought.len()
    }
    pub fn bad(&self) -> bool {
        self.not_touched.iter().any(|s| s.bad)
    }
    pub fn files(&self) -> Vec<String> {
        self.restored
            .iter()
            .chain(&self.removed)
            .chain(&self.brought)
            .cloned()
            .collect()
    }
    /// "Changes 4 files: 2 restored, 1 removed (new.txt), 1 brought back (src/c.ts)."
    pub fn changes_line(&self, would: bool) -> String {
        let mut parts = Vec::new();
        if !self.restored.is_empty() {
            parts.push(format!("{} restored", self.restored.len()));
        }
        if !self.removed.is_empty() {
            parts.push(format!(
                "{} removed ({})",
                self.removed.len(),
                names(&self.removed)
            ));
        }
        if !self.brought.is_empty() {
            parts.push(format!(
                "{} brought back ({})",
                self.brought.len(),
                names(&self.brought)
            ));
        }
        format!(
            "{} {}: {}.",
            if would { "Would change" } else { "Changes" },
            files_word(self.total()),
            parts.join(", ")
        )
    }
    pub fn not_touched_line(&self) -> Option<String> {
        if self.not_touched.is_empty() {
            return None;
        }
        let items: Vec<String> = self
            .not_touched
            .iter()
            .map(|s| format!("{} ({})", s.path, s.why))
            .collect();
        Some(format!(
            "Not touched: {}.",
            if items.len() > 4 {
                format!(
                    "{}, {}, {}, {} and {} more",
                    items[0],
                    items[1],
                    items[2],
                    items[3],
                    items.len() - 4
                )
            } else {
                items.join(", ")
            }
        ))
    }
    fn to_json(&self) -> Value {
        json!({
            "files": self.files(),
            "restored": self.restored.len(),
            "removed": self.removed.len(),
            "brought_back": self.brought.len(),
            "not_touched": self.not_touched.iter().map(|s| json!({"path": s.path, "why": s.why, "bad": s.bad})).collect::<Vec<_>>(),
        })
    }
}

fn skip_text(code: &str) -> (String, bool) {
    match code {
        "changed" => ("changed while restoring".to_string(), true),
        "notempty" => ("folder kept, it still holds other files".to_string(), false),
        "symlink" => ("it is inside a linked folder".to_string(), true),
        "nodir" => ("its folder is missing".to_string(), true),
        "stage" => ("the saved copy is missing".to_string(), true),
        "unsafe" => ("unsafe path".to_string(), true),
        "claude" => (CLAUDE_NOTE.to_string(), false),
        c => (
            format!(
                "could not write it ({})",
                c.strip_prefix("io:").unwrap_or(c)
            ),
            true,
        ),
    }
}

/// Sort the entries into what the report counts. A directory entry counts as nothing.
fn count(entries: &[Entry], done: &HashMap<usize, Option<String>>, plan_skips: &[Skip]) -> Counts {
    let mut c = Counts {
        not_touched: plan_skips.to_vec(),
        ..Default::default()
    };
    for e in entries {
        match done.get(&e.n) {
            Some(Some(code)) => {
                let (why, bad) = skip_text(code);
                if code != "notempty" || !e.path.is_empty() {
                    c.not_touched.push(Skip {
                        path: e.path.clone(),
                        why,
                        bad,
                    });
                }
            }
            _ => match e.act {
                Act::Overwrite | Act::Chmod => c.restored.push(e.path.clone()),
                Act::Symlink if !e.old.is_empty() => c.restored.push(e.path.clone()),
                Act::Delete => c.removed.push(e.path.clone()),
                Act::Create | Act::Symlink => c.brought.push(e.path.clone()),
                Act::Mkdir | Act::RmdirIfEmpty => {}
            },
        }
    }
    for l in [&mut c.restored, &mut c.removed, &mut c.brought] {
        l.sort();
    }
    c
}

// ---------------------------------------------------------------- journal

pub fn journal_dir(ws: &Ws) -> PathBuf {
    ws.dir.join("journal")
}
fn journal_path(ws: &Ws, id: i64) -> PathBuf {
    journal_dir(ws).join(format!("restore-{id}.journal"))
}
fn stage_root(ws: &Ws) -> PathBuf {
    ws.root.join(".torpor-stage")
}
fn stage_path(ws: &Ws, id: i64) -> PathBuf {
    stage_root(ws).join(id.to_string())
}

struct Header {
    id: i64,
    target: i64,
    pre: i64,
    started: i64,
    kind: String,
    entries: usize,
    skips: Vec<Skip>,
}

struct Journal {
    hdr: Header,
    entries: Vec<Entry>,
    done: HashMap<usize, Option<String>>,
    complete: bool,
}

fn header_json(h: &Header) -> Value {
    json!({
        "v": 1, "id": h.id, "target": h.target, "pre": h.pre, "started": h.started, "kind": h.kind, "entries": h.entries,
        "skips": h.skips.iter().take(200).map(|s| json!([s.path, s.why, s.bad])).collect::<Vec<_>>(),
    })
}

fn entry_json(e: &Entry) -> Value {
    json!({"n": e.n, "path": e.path, "action": e.act.as_str(), "old": e.old, "new": e.new, "kind": e.kind.to_string(), "perm": e.perm})
}

fn write_journal(ws: &Ws, h: &Header, entries: &[Entry]) -> Result<File> {
    let mut text = format!("{}\n", header_json(h));
    for e in entries {
        text.push_str(&format!("{}\n", entry_json(e)));
    }
    let path = journal_path(ws, h.id);
    fs::create_dir_all(journal_dir(ws))?;
    write_atomic(&path, text.as_bytes(), 0o600)?;
    Ok(OpenOptions::new().append(true).open(&path)?)
}

/// Read a journal. A line that does not parse (a torn append at the end) is ignored; the entry count in the header tells whether the plan is whole.
fn read_journal(path: &Path) -> Option<Journal> {
    let text = String::from_utf8_lossy(&fs::read(path).ok()?).to_string();
    let mut lines = text.lines();
    let h: Value = serde_json::from_str(lines.next()?).ok()?;
    if h.get("v")?.as_i64()? != 1 {
        return None;
    }
    let skips = h
        .get("skips")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(Skip {
                        path: x.get(0)?.as_str()?.to_string(),
                        why: x.get(1)?.as_str()?.to_string(),
                        bad: x.get(2)?.as_bool()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let hdr = Header {
        id: h.get("id")?.as_i64()?,
        target: h.get("target")?.as_i64()?,
        pre: h.get("pre")?.as_i64()?,
        started: h.get("started")?.as_i64()?,
        kind: h
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("back")
            .to_string(),
        entries: h.get("entries")?.as_u64()? as usize,
        skips,
    };
    let (mut entries, mut done, mut complete) = (Vec::new(), HashMap::new(), false);
    for l in lines {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        if v.get("complete").and_then(|c| c.as_bool()) == Some(true) {
            complete = true;
        } else if let Some(n) = v.get("done").and_then(|n| n.as_u64()) {
            done.insert(
                n as usize,
                v.get("skip")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string()),
            );
        } else if let (Some(n), Some(p), Some(a), Some(o), Some(nw)) = (
            v.get("n").and_then(|x| x.as_u64()),
            v.get("path").and_then(|x| x.as_str()),
            v.get("action")
                .and_then(|x| x.as_str())
                .and_then(Act::parse),
            v.get("old").and_then(|x| x.as_str()),
            v.get("new").and_then(|x| x.as_str()),
        ) {
            let kind = v
                .get("kind")
                .and_then(|k| k.as_str())
                .and_then(|k| k.chars().next())
                .unwrap_or('f');
            entries.push(Entry {
                n: n as usize,
                path: p.to_string(),
                act: a,
                old: o.to_string(),
                new: nw.to_string(),
                kind,
                perm: v.get("perm").and_then(|x| x.as_u64()).unwrap_or(0o644) as u32,
                blob: None,
                size: 0,
            });
        }
    }
    Some(Journal {
        hdr,
        entries,
        done,
        complete,
    })
}

// ---------------------------------------------------------------- applying

struct Cx<'a> {
    root: &'a Path,
    stage: &'a Path,
    key: Option<&'a Key>,
}

enum Res {
    Applied,
    Already,
    Skipped(String),
}

/// Spec 6.2: relative, no `..`, no NUL, no `.git` part.
fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains('\0')
        && !rel.starts_with('/')
        && rel
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && !c.eq_ignore_ascii_case(".git"))
}

/// A symlinked or missing parent folder: skip and report.
fn parent_problem(root: &Path, rel: &str) -> Option<&'static str> {
    let comps: Vec<&str> = rel.split('/').collect();
    let mut cur = root.to_path_buf();
    for c in &comps[..comps.len() - 1] {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return Some("symlink"),
            Ok(m) if m.is_dir() => {}
            _ => return Some("nodir"),
        }
    }
    None
}

fn rand_tmp(dir: &Path) -> PathBuf {
    let mut r = [0u8; 8];
    let _ = getrandom::fill(&mut r);
    dir.join(format!(".torpor-{}.tmp", hex(&r)))
}

/// Move a staged file into place. A rename is atomic; across file systems (EXDEV) protocol A writes a temp file in the target folder first.
fn place(src: &Path, dst: &Path, perm: u32) -> std::io::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let bytes = fs::read(src)?;
            write_atomic(dst, &bytes, perm)?;
            fs::set_permissions(dst, fs::Permissions::from_mode(perm))?;
            fs::remove_file(src)
        }
        Err(e) => Err(e),
    }
}

fn do_entry(cx: &Cx, e: &Entry, dst: &Path) -> std::result::Result<(), String> {
    let io = |r: std::io::Result<()>| r.map_err(|x| format!("io:{x}"));
    match e.act {
        Act::Create | Act::Overwrite => {
            let src = cx.stage.join(e.n.to_string());
            if !src.exists() {
                return Err("stage".into());
            }
            io(place(&src, dst, e.perm))
        }
        Act::Symlink => {
            let src = cx.stage.join(e.n.to_string());
            let target = fs::read(&src).map_err(|_| "stage".to_string())?;
            let tmp = rand_tmp(dst.parent().unwrap_or(cx.root));
            io(std::os::unix::fs::symlink(
                std::ffi::OsStr::from_bytes(&target),
                &tmp,
            ))?;
            let r = fs::rename(&tmp, dst);
            if r.is_err() {
                let _ = fs::remove_file(&tmp);
            }
            io(r)
        }
        Act::Delete => {
            // Into the stage, so the bytes stay on disk until the whole restore is finished.
            let _ = fs::create_dir_all(cx.stage);
            match fs::rename(dst, cx.stage.join(format!("trash-{}", e.n))) {
                Ok(()) => Ok(()),
                // Another file system: the saved copy in the store is the safety net.
                Err(x) if x.raw_os_error() == Some(libc::EXDEV) => io(fs::remove_file(dst)),
                Err(x) => Err(format!("io:{x}")),
            }
        }
        Act::Mkdir => io(fs::DirBuilder::new().mode(0o755).create(dst)),
        Act::RmdirIfEmpty => match fs::remove_dir(dst) {
            Ok(()) => Ok(()),
            Err(x) if matches!(x.raw_os_error(), Some(libc::ENOTEMPTY) | Some(libc::EEXIST)) => {
                Err("notempty".into())
            }
            Err(x) => Err(format!("io:{x}")),
        },
        Act::Chmod => io(fs::set_permissions(dst, fs::Permissions::from_mode(e.perm))),
    }
}

fn apply_entry(cx: &Cx, e: &Entry) -> Res {
    if !safe_rel(&e.path) {
        return Res::Skipped("unsafe".into());
    }
    if is_claude_settings(&e.path) {
        return Res::Skipped("claude".into()); // the plan leaves these out; this is for a journal an older version wrote
    }
    if let Some(p) = parent_problem(cx.root, &e.path) {
        return Res::Skipped(p.into());
    }
    let live = live_state(cx.root, &e.path, cx.key);
    if live == e.new {
        return Res::Already;
    }
    if live != e.old {
        return Res::Skipped("changed".into());
    }
    match do_entry(cx, e, &cx.root.join(&e.path)) {
        Ok(()) => Res::Applied,
        Err(code) => Res::Skipped(code),
    }
}

fn sync_dir(d: &Path) {
    let _ = File::open(d).and_then(|f| f.sync_all());
}

/// Apply every entry that is not finished yet; one `done` line per entry. `redo_changed`: entries the journal already
/// lists as done keep that status even when the file has been changed since (a recover long after the crash).
fn apply_all(
    cx: &Cx,
    entries: &[Entry],
    done: &mut HashMap<usize, Option<String>>,
    journal: &mut File,
) -> Result<()> {
    let mut touched: HashSet<PathBuf> = HashSet::new();
    for e in entries {
        if matches!(done.get(&e.n), Some(Some(_))) {
            continue;
        }
        let was_done = done.contains_key(&e.n);
        let res = apply_entry(cx, e);
        fault_point("restore");
        let skip = match res {
            Res::Applied => {
                touched.insert(
                    cx.root
                        .join(&e.path)
                        .parent()
                        .unwrap_or(cx.root)
                        .to_path_buf(),
                );
                None
            }
            Res::Already => None,
            Res::Skipped(c) if c == "changed" && was_done => None,
            Res::Skipped(c) => Some(c),
        };
        let line = match &skip {
            Some(c) => json!({"done": e.n, "skip": c}),
            None => json!({"done": e.n}),
        };
        journal.write_all(format!("{line}\n").as_bytes())?;
        fault_point("restore");
        done.insert(e.n, skip);
    }
    for d in touched {
        sync_dir(&d);
    }
    journal.sync_all()?;
    Ok(())
}

/// Spec 6.2 step 5: leftover temp files of an earlier crash, older than an hour, in the folders a plan touches.
fn sweep_tmp(root: &Path, entries: &[Entry]) {
    let now = now_ns();
    let dirs: HashSet<PathBuf> = entries
        .iter()
        .filter(|e| safe_rel(&e.path))
        .map(|e| root.join(&e.path).parent().unwrap_or(root).to_path_buf())
        .collect();
    for d in dirs {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for f in rd.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            let old = f
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|t| (t.as_nanos() as i64) < now - STALE_TMP_NS);
            if name.starts_with(".torpor-") && name.ends_with(".tmp") && old {
                let _ = fs::remove_file(f.path());
            }
        }
    }
}

/// Spec 6.3 step 3: every new blob under `<root>/.torpor-stage/<id>/<n>`, re-hashed on the way (the readers check the hash).
/// Returns the entries that could not be staged.
fn stage_all(
    ws: &Ws,
    stage: &Path,
    entries: &[Entry],
    key: Option<&Key>,
) -> Result<Vec<(usize, String)>> {
    let mut bad = Vec::new();
    fs::create_dir_all(stage)?;
    for d in [stage_root(ws), stage.to_path_buf()] {
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
    }
    for e in entries.iter().filter(|e| e.act.writes()) {
        let got = match e.blob {
            None => Err(Error::msg("no saved copy")),
            Some(id) if e.kind == 's' => ws.read_secret(&ws.conn, &id, key),
            Some(id) => ws.read_blob(&ws.conn, &id),
        };
        let bytes = match got {
            Ok(b) => b,
            Err(x) => {
                bad.push((e.n, format!("its saved copy cannot be read ({x})")));
                continue;
            }
        };
        let path = stage.join(e.n.to_string());
        // Created with the final bits so a kill never leaves a readable copy of a secret; plain new files follow the umask like any new file.
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(e.perm)
            .open(&path)?;
        f.write_all(&bytes)?;
        if e.kind == 's' || e.act != Act::Create {
            f.set_permissions(fs::Permissions::from_mode(e.perm))?;
        }
        f.sync_all()?;
    }
    sync_dir(stage);
    Ok(bad)
}

// ---------------------------------------------------------------- finishing

/// Close a restore whose entries are all applied: complete marker, stage removed, the `restored` point, the row, the journal.
fn finish(
    ws: &Ws,
    hdr: &Header,
    entries: &[Entry],
    done: &HashMap<usize, Option<String>>,
    complete_written: bool,
    journal_file: Option<&mut File>,
) -> Result<Counts> {
    let jpath = journal_path(ws, hdr.id);
    if !complete_written {
        let line = "{\"complete\":true}\n";
        match journal_file {
            Some(f) => f.write_all(line.as_bytes())?,
            None => OpenOptions::new()
                .append(true)
                .open(&jpath)?
                .write_all(line.as_bytes())?,
        }
    }
    fault_point("restore");
    let _ = fs::remove_dir_all(stage_path(ws, hdr.id));
    let _ = fs::remove_dir(stage_root(ws));
    fault_point("restore");
    let counts = count(entries, done, &hdr.skips);
    let final_row = ws
        .conn
        .query_row("SELECT status FROM restores WHERE id=?1", [hdr.id], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten();
    if !matches!(
        final_row.as_deref(),
        Some("done") | Some("partial") | Some("cancelled")
    ) {
        record_restored(ws, hdr, &counts)?;
    }
    fault_point("restore");
    let _ = fs::remove_file(&jpath);
    sync_dir(&journal_dir(ws));
    Ok(counts)
}

/// The `restored` point is a fresh scan, so it holds what the folder really is now. If it cannot be written (a full disk),
/// the row is still closed, without a result point, or every later recover would try again for ever.
fn record_restored(ws: &Ws, hdr: &Header, counts: &Counts) -> Result<()> {
    let status = if counts.bad() { "partial" } else { "done" };
    let mut report = counts.to_json();
    report["kind"] = json!(hdr.kind);
    report["target"] = json!(hdr.target);
    report["pre"] = json!(hdr.pre);
    let note = if hdr.kind == "undo" {
        "undid a turn".to_string()
    } else {
        format!("went back to save point {}", hdr.target)
    };
    let tx = Transaction::new_unchecked(&ws.conn, TransactionBehavior::Immediate)?;
    let cap = capture::capture(
        ws,
        &tx,
        &Req {
            session: "",
            points: vec![PointSpec {
                kind: PointKind::Restored,
                turn_id: None,
                note: Some(note),
            }],
            turn: None,
            only_if_changed: false,
            budget: None,
        },
    );
    match cap {
        Ok(c) => {
            tx.execute("UPDATE restores SET result_point=?2,finished_at=?3,status=?4,report_json=?5 WHERE id=?1", params![hdr.id, c.point_ids[0], now_ns(), status, report.to_string()])?;
            tx.commit()?;
        }
        Err(e) => {
            drop(tx);
            report["note"] = json!(format!("no save point was recorded after the restore: {e}"));
            ws.conn.execute(
                "UPDATE restores SET finished_at=?2,status=?3,report_json=?4 WHERE id=?1",
                params![hdr.id, now_ns(), status, report.to_string()],
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- the job (shared by back and undo)

pub struct Job {
    pub kind: &'static str,
    pub target_point: i64,
    /// First line of the report ("Going back to save point 52 (end of turn 15, today 13:55).")
    pub head: String,
    /// Lines between the head and the counts (undo lists each file).
    pub detail: Vec<String>,
    pub plan: Plan,
}

/// The pre-restore save point is taken first, inside a transaction that stays open while the plan is built.
/// Anything that ends without changing files rolls it back, so no pinned point is left behind.
pub struct Prep<'a> {
    pub tx: Transaction<'a>,
    pub pre_id: i64,
    pub live: [u8; 32],
    pub key: Option<Key>,
}

pub fn prepare<'a>(ws: &'a Ws, note: &str) -> Result<Prep<'a>> {
    let tx = Transaction::new_unchecked(&ws.conn, TransactionBehavior::Immediate)?;
    let c = capture::capture(
        ws,
        &tx,
        &Req {
            session: "",
            points: vec![PointSpec {
                kind: PointKind::PreRestore,
                turn_id: None,
                note: Some(note.to_string()),
            }],
            turn: None,
            only_if_changed: false,
            budget: None,
        },
    )?;
    let key = Key::load()?;
    Ok(Prep {
        tx,
        pre_id: c.point_ids[0],
        live: c.tree_id,
        key,
    })
}

/// Read back every saved copy the plan is about to replace or remove (the pre-restore point is "hash-verified", spec 6.3 step 1).
fn verify_old(ws: &Ws, db: &Connection, entries: &[Entry], key: Option<&Key>) -> Result<()> {
    for e in entries.iter().filter(|e| {
        matches!(e.act, Act::Overwrite | Act::Delete | Act::Symlink) && !e.old.is_empty()
    }) {
        let id = e.old.split_once(':').and_then(|(_, h)| unhex32(h));
        let Some(id) = id else { continue };
        let kind = e.old.chars().next().unwrap_or('f');
        let r = if kind == 's' {
            ws.read_secret(db, &id, key).map(|_| ())
        } else {
            ws.read_blob(db, &id).map(|_| ())
        };
        if r.is_err() {
            return Err(Error::msg(format!(
                "the save point of {} is damaged, so it cannot be safely replaced. Nothing was changed",
                e.path
            )));
        }
    }
    Ok(())
}

fn render(job: &Job, counts: &Counts, would: bool, pre_id: i64) -> String {
    let mut out = format!("{}\n", job.head);
    for d in &job.detail {
        out.push_str(&format!("{d}\n"));
    }
    // An undo lists its files one by one above; the summary is for `back`.
    if counts.total() > 0 && job.detail.is_empty() {
        out.push_str(&format!("{}\n", counts.changes_line(would)));
    }
    if let Some(l) = counts.not_touched_line() {
        out.push_str(&format!("{l}\n"));
    }
    if would {
        out.push_str("Nothing was changed (dry run).\n");
    } else {
        out.push_str(&format!("Done. To undo this: torpor back --id {pre_id}\n"));
    }
    out
}

/// Run a plan whose pre-restore point is still uncommitted in `prep`. Returns the report.
pub fn go(ws: &Ws, prep: Prep, job: Job, opts: &Opts) -> Result<Outcome> {
    let Prep {
        tx, pre_id, key, ..
    } = prep;
    if let Some(why) = &job.plan.refuse {
        return Ok(Outcome::new(3, format!("{why}\n")));
    }
    let entries = &job.plan.entries;
    let preview = count(entries, &HashMap::new(), &job.plan.skips);
    if entries.is_empty() {
        // Nothing to write: no pinned point should stay behind.
        drop(tx);
        let mut text = format!("{}\n", job.head);
        text.push_str("Nothing needed changing.\n");
        if let Some(l) = preview.not_touched_line() {
            text.push_str(&format!("{l}\n"));
        }
        return Ok(Outcome::new(if preview.bad() { 4 } else { 0 }, text));
    }
    let bytes: u64 = entries.iter().map(|e| e.size).sum();
    let (free, _) = free_space(&ws.root);
    if free < bytes + SPARE_BYTES {
        drop(tx);
        return Ok(Outcome::new(
            3,
            format!(
                "Not enough free space on this disk to go back safely: it needs {} for the files plus {} spare, and {} is free. Nothing was changed.\n",
                human_bytes(bytes),
                human_bytes(SPARE_BYTES),
                human_bytes(free)
            ),
        ));
    }
    verify_old(ws, &tx, entries, key.as_ref())?;
    if opts.dry_run {
        drop(tx);
        return Ok(Outcome::new(0, render(&job, &preview, true, pre_id)));
    }
    let started = now_ns();
    tx.execute("INSERT INTO restores(kind,target_point,pre_point,started_at,status) VALUES(?1,?2,?3,?4,'staging')", params![job.kind, job.target_point, pre_id, started])?;
    let rid = tx.last_insert_rowid();
    tx.commit()?;
    fault_point("restore");

    let hdr = Header {
        id: rid,
        target: job.target_point,
        pre: pre_id,
        started,
        kind: job.kind.to_string(),
        entries: 0,
        skips: job.plan.skips.clone(),
    };
    let stage = stage_path(ws, rid);
    sweep_tmp(&ws.root, entries);
    let failed = stage_all(ws, &stage, entries, key.as_ref())?;
    fault_point("restore");
    let mut hdr = hdr;
    let mut live_entries: Vec<Entry> = Vec::new();
    for e in entries {
        match failed.iter().find(|(n, _)| *n == e.n) {
            Some((_, why)) => hdr.skips.push(Skip {
                path: e.path.clone(),
                why: why.clone(),
                bad: true,
            }),
            None => live_entries.push(e.clone()),
        }
    }
    if live_entries.is_empty() {
        let _ = fs::remove_dir_all(&stage);
        let _ = fs::remove_dir(stage_root(ws));
        ws.conn.execute(
            "UPDATE restores SET status='cancelled',finished_at=?2 WHERE id=?1",
            params![rid, now_ns()],
        )?;
        let c = count(&[], &HashMap::new(), &hdr.skips);
        return Ok(Outcome::new(
            4,
            format!(
                "{}\nNothing could be restored.\n{}\n",
                job.head,
                c.not_touched_line().unwrap_or_default()
            ),
        ));
    }
    hdr.entries = live_entries.len();
    let mut jf = write_journal(ws, &hdr, &live_entries)?;
    ws.conn
        .execute("UPDATE restores SET status='applying' WHERE id=?1", [rid])?;
    fault_point("restore");

    let cx = Cx {
        root: &ws.root,
        stage: &stage,
        key: key.as_ref(),
    };
    let mut done = HashMap::new();
    apply_all(&cx, &live_entries, &mut done, &mut jf)?;
    let counts = finish(ws, &hdr, &live_entries, &done, false, Some(&mut jf))?;
    let code = if counts.bad() { 4 } else { 0 };
    Ok(Outcome::new(code, render(&job, &counts, false, pre_id)))
}

// ---------------------------------------------------------------- back

/// `torpor back`. Takes the exclusive lock itself.
pub fn back(ws: &mut Ws, target: Target, opts: Opts) -> Result<Outcome> {
    ws.lock_exclusive(LOCK_WAIT_MS)?;
    let r = back_locked(ws, target, opts);
    ws.unlock();
    r
}

fn back_locked(ws: &Ws, target: Target, opts: Opts) -> Result<Outcome> {
    let first = recover_locked(ws)?;
    let mut out = back_inner(ws, target, opts)?;
    if !first.is_empty() {
        out.text = format!("{}\n{}", first.join("\n"), out.text);
    }
    Ok(out)
}

fn back_inner(ws: &Ws, target: Target, opts: Opts) -> Result<Outcome> {
    let now = now_ns();
    let note = match target {
        Target::Back(_) => "before going back".to_string(),
        Target::Turn(k) => format!("before going back to turn {k}"),
        Target::Id(p) => format!("before going back to save point {p}"),
    };
    let prep = prepare(ws, &note)?;
    let (point, tree) = match resolve(&prep.tx, target, &prep.live)? {
        Ok(t) => t,
        Err(o) => return Ok(o),
    };
    let label = format!("save point {point} ({})", describe(&prep.tx, point, now));
    if tree == prep.live {
        return Ok(Outcome::new(
            0,
            format!("Your files already match {label}. Nothing to do.\n"),
        ));
    }
    let plan = plan_trees(
        ws,
        &prep.tx,
        &prep.live,
        &tree,
        prep.key.as_ref(),
        opts.with_data,
    )?;
    let head = format!(
        "{} {label}.",
        if opts.dry_run {
            "Would go back to"
        } else {
            "Going back to"
        }
    );
    go(
        ws,
        prep,
        Job {
            kind: "back",
            target_point: point,
            head,
            detail: vec![],
            plan,
        },
        &opts,
    )
}

// ---------------------------------------------------------------- recover

/// True when a crashed restore left a journal or a stage folder behind.
pub fn has_pending(ws: &Ws) -> bool {
    let journals = fs::read_dir(journal_dir(ws)).is_ok_and(|rd| {
        rd.flatten()
            .any(|f| f.file_name().to_string_lossy().ends_with(".journal"))
    });
    journals || fs::read_dir(stage_root(ws)).is_ok_and(|mut rd| rd.next().is_some())
}

/// Finish what a crashed restore started (spec 6.3 step 7). The caller holds the exclusive lock. Returns the report lines.
pub fn recover_locked(ws: &Ws) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    // A journal write that was killed half way leaves only its temp file; the plan was never started.
    if let Ok(rd) = fs::read_dir(journal_dir(ws)) {
        for f in rd.flatten().filter(|f| {
            f.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(".torpor-") && n.ends_with(".tmp"))
        }) {
            let _ = fs::remove_file(f.path());
        }
    }
    let mut journals: Vec<(i64, PathBuf)> = fs::read_dir(journal_dir(ws))
        .map(|rd| {
            rd.flatten()
                .filter_map(|f| {
                    f.file_name()
                        .to_str()
                        .and_then(|n| {
                            n.strip_prefix("restore-")?
                                .strip_suffix(".journal")?
                                .parse::<i64>()
                                .ok()
                        })
                        .map(|id| (id, f.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    journals.sort();
    let key = Key::load()?;
    for (id, path) in &journals {
        let Some(j) = read_journal(path) else {
            // Not even a header: nothing was applied.
            let _ = fs::remove_file(path);
            let _ = fs::remove_dir_all(stage_path(ws, *id));
            ws.conn.execute("UPDATE restores SET status='cancelled',finished_at=?2 WHERE id=?1 AND status IN ('staging','applying')", params![id, now_ns()])?;
            continue;
        };
        let stage = stage_path(ws, j.hdr.id);
        if j.entries.len() != j.hdr.entries {
            // The journal does not hold its whole plan: nothing was applied.
            let _ = fs::remove_file(path);
            let _ = fs::remove_dir_all(&stage);
            ws.conn.execute("UPDATE restores SET status='cancelled',finished_at=?2 WHERE id=?1 AND status IN ('staging','applying')", params![j.hdr.id, now_ns()])?;
            lines.push(
                "A restore was interrupted before it changed anything. Nothing to finish."
                    .to_string(),
            );
            continue;
        }
        let was_final = ws
            .conn
            .query_row("SELECT status FROM restores WHERE id=?1", [j.hdr.id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten()
            .is_some_and(|s| s == "done" || s == "partial" || s == "cancelled");
        let mut done = j.done.clone();
        if !j.complete {
            sweep_tmp(&ws.root, &j.entries);
            let mut jf = OpenOptions::new().append(true).open(path)?;
            let cx = Cx {
                root: &ws.root,
                stage: &stage,
                key: key.as_ref(),
            };
            apply_all(&cx, &j.entries, &mut done, &mut jf)?;
        }
        let counts = finish(ws, &j.hdr, &j.entries, &done, j.complete, None)?;
        if !was_final {
            let applied = j
                .entries
                .iter()
                .filter(|e| matches!(done.get(&e.n), Some(None)))
                .count();
            lines.push(format!(
                "A restore was interrupted at {}. Finishing it now. Done: {} of {} files.",
                local_hhmm(j.hdr.started),
                applied,
                j.entries.len()
            ));
            if let Some(l) = counts.not_touched_line() {
                lines.push(l);
            }
        }
    }
    // Rows that never got a journal, and stage folders without one.
    let open: Vec<i64> = {
        let mut st = ws
            .conn
            .prepare("SELECT id FROM restores WHERE status IN ('staging','applying')")?;
        let rows = st.query_map([], |r| r.get::<_, i64>(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for id in open {
        if !journal_path(ws, id).exists() {
            ws.conn.execute(
                "UPDATE restores SET status='cancelled',finished_at=?2 WHERE id=?1",
                params![id, now_ns()],
            )?;
            let _ = fs::remove_dir_all(stage_path(ws, id));
        }
    }
    if let Ok(rd) = fs::read_dir(stage_root(ws)) {
        for f in rd.flatten() {
            // Only folders with a restore id for a name are Torpor's; anything else in there is not touched.
            let Some(id) = f.file_name().to_str().and_then(|n| n.parse::<i64>().ok()) else {
                continue;
            };
            if !journal_path(ws, id).exists() {
                let _ = fs::remove_dir_all(f.path());
            }
        }
        let _ = fs::remove_dir(stage_root(ws));
    }
    Ok(lines)
}

/// `torpor recover`.
pub fn recover(ws: &mut Ws) -> Result<Outcome> {
    ws.lock_exclusive(LOCK_WAIT_MS)?;
    let r = recover_locked(ws);
    ws.unlock();
    let lines = r?;
    if lines.is_empty() {
        return Ok(Outcome::new(0, "Nothing to recover.\n"));
    }
    Ok(Outcome::new(0, format!("{}\n", lines.join("\n"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, act: Act) -> Entry {
        Entry::bare(path.to_string(), act, "", "")
    }

    #[test]
    fn claude_settings_files_leave_every_plan_and_are_said_first() {
        let said = |p: &str| Skip {
            path: p.to_string(),
            why: "x".into(),
            bad: false,
        };
        let plan = Plan::finish(
            vec![
                entry(".claude/settings.json", Act::Delete),
                entry("a.txt", Act::Delete),
            ],
            vec![entry(".claude/settings.local.json", Act::RmdirIfEmpty)],
            vec![],
            vec![
                entry(".Claude/Settings.JSON", Act::Create),
                entry("b.txt", Act::Create),
            ],
            vec![entry(".claude/settings.local.json", Act::Chmod)],
            vec![said("dev.db"), said(".claude/settings.local.json")],
            None,
        );
        assert_eq!(
            plan.entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["a.txt", "b.txt"]
        );
        let skips: Vec<(&str, &str)> = plan
            .skips
            .iter()
            .map(|s| (s.path.as_str(), s.why.as_str()))
            .collect();
        let want = [
            (".claude/settings.local.json", "x"),
            (".claude/settings.json", CLAUDE_NOTE),
            (".Claude/Settings.JSON", CLAUDE_NOTE),
            ("dev.db", "x"),
        ];
        assert_eq!(skips, want, "each once, first, an earlier reason kept");
        assert!(plan.skips.iter().all(|s| !s.bad));
    }

    #[test]
    fn applying_skips_a_claude_settings_file_even_from_an_old_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join(".claude/settings.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "keep").unwrap();
        let mut del = entry(".claude/settings.json", Act::Delete);
        del.old = live_state(tmp.path(), ".claude/settings.json", None); // exactly what makes a delete go through
        let cx = Cx {
            root: tmp.path(),
            stage: tmp.path(),
            key: None,
        };
        assert!(matches!(apply_entry(&cx, &del), Res::Skipped(c) if c == "claude"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
        assert_eq!(skip_text("claude"), (CLAUDE_NOTE.to_string(), false));
    }
}
